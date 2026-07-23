//! The enrichment worker: drains the per-source queue — detail page →
//! image downloads → LLM extraction — each step independent and fail-open,
//! with the queue's backoff handling retries. One worker task per source,
//! sharing the source's politeness budget via its `fetch_detail`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::config::EnrichmentConfig;
use crate::db::Db;
use crate::llm::{ExtractInput, LlmHandle};
use crate::scrape::ImmoSource;

/// Image downloads mocked in tests; the real one is a browser-UA client.
#[async_trait::async_trait]
pub trait ImageFetch: Send + Sync {
    async fn fetch(&self, url: &str) -> anyhow::Result<Vec<u8>>;
}

pub struct HttpImageFetch {
    client: reqwest::Client,
}

impl HttpImageFetch {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                     (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
                )
                .build()
                .expect("building image http client"),
        }
    }
}

#[async_trait::async_trait]
impl ImageFetch for HttpImageFetch {
    async fn fetch(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        let response = self.client.get(url).send().await?.error_for_status()?;
        Ok(response.bytes().await?.to_vec())
    }
}

/// ".jpg" from a CDN url, default jpg; query strings stripped.
fn extension_of(url: &str) -> &str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    match path.rsplit('.').next() {
        Some(ext) if ext.len() <= 4 && ext.chars().all(|c| c.is_ascii_alphanumeric()) => ext,
        _ => "jpg",
    }
}

/// One queue item, end to end. Any error propagates so the caller records
/// the failure with backoff; every step is idempotent for the retry.
pub async fn process_one(
    db: &Db,
    source: &dyn ImmoSource,
    images: &dyn ImageFetch,
    llm: &LlmHandle,
    config: &EnrichmentConfig,
    id: Uuid,
) -> anyhow::Result<()> {
    let state = db.enrichment_state(id).await?;

    // 1. detail page (skipped once enriched; price-change cleared the mark)
    if state.enriched_at.is_none() {
        let detail = source
            .fetch_detail(&state.listing.canonical_url)
            .await?
            .unwrap_or_default();
        // an empty detail still marks the listing enriched — sources
        // without detail support are done after images + extraction
        db.set_detail(id, &detail).await?;
    }

    // 2. images: fetch once each, capped
    for (position, url) in db.pending_images(id, config.max_images as i64).await? {
        let bytes = images.fetch(&url).await?;
        let dir = config.images_dir.join(id.to_string());
        std::fs::create_dir_all(&dir)?;
        let file = format!("{position}.{}", extension_of(&url));
        std::fs::write(dir.join(&file), &bytes)?;
        db.mark_image_saved(id, position, &format!("{id}/{file}"))
            .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 3. LLM extraction over whatever description we have
    let state = db.enrichment_state(id).await?;
    if state.extracted_at.is_none()
        && let Some(description) = &state.listing.description
    {
        let extractor = llm.read().await.extractor.clone();
        if let Some(extractor) = extractor {
            let attrs = extractor
                .extract(&ExtractInput {
                    title: &state.listing.title,
                    price_cents: state.listing.price_cents,
                    property_type: state.listing.property_type.label(),
                    surface_m2: state.listing.surface_m2,
                    rooms: state.listing.rooms,
                    description,
                })
                .await?;
            db.set_attributes(id, &attrs).await?;
        }
        // llm disabled: fine — the listing is done without attributes
    }

    db.enrichment_done(id).await?;
    Ok(())
}

/// The forever loop for one source.
pub async fn run_source_enricher(
    source: Arc<dyn ImmoSource>,
    db: Db,
    config: EnrichmentConfig,
    llm: LlmHandle,
) {
    let images = HttpImageFetch::new();
    loop {
        match db.due_enrichment(source.id(), 5).await {
            Ok(due) => {
                for id in due {
                    if let Err(e) =
                        process_one(&db, source.as_ref(), &images, &llm, &config, id).await
                    {
                        tracing::warn!(source = source.id(), %id, error = %e, "enrichment failed");
                        match db
                            .enrichment_failed(id, &e.to_string(), config.max_attempts)
                            .await
                        {
                            Ok(true) => {
                                tracing::warn!(%id, "enrichment given up after max attempts")
                            }
                            Ok(false) => {}
                            Err(e) => tracing::error!(error = %e, "recording enrichment failure"),
                        }
                    }
                }
            }
            Err(e) => tracing::error!(source = source.id(), error = %e, "reading enrichment queue"),
        }
        tokio::time::sleep(Duration::from_secs(config.poll_seconds)).await;
    }
}

/// Resolve where a saved image lives on disk (also used by main to serve).
pub fn images_root(config: &EnrichmentConfig) -> PathBuf {
    Path::new(&config.images_dir).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path as FsPath;
    use std::sync::Mutex;

    use terrier_domain::{ExtractedAttrs, ListingDetail, RawListing};

    struct FakeSource {
        detail: Option<ListingDetail>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl ImmoSource for FakeSource {
        fn id(&self) -> &str {
            "src"
        }
        async fn fetch(&self) -> anyhow::Result<Vec<RawListing>> {
            Ok(vec![])
        }
        async fn fetch_detail(&self, _url: &str) -> anyhow::Result<Option<ListingDetail>> {
            if self.fail {
                anyhow::bail!("blocked");
            }
            Ok(self.detail.clone())
        }
    }

    struct FakeImages;
    #[async_trait::async_trait]
    impl ImageFetch for FakeImages {
        async fn fetch(&self, _url: &str) -> anyhow::Result<Vec<u8>> {
            Ok(vec![0xFF, 0xD8, 0xFF])
        }
    }

    struct FakeLlm {
        calls: Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl crate::llm::LlmExtract for FakeLlm {
        async fn extract(&self, input: &ExtractInput<'_>) -> anyhow::Result<ExtractedAttrs> {
            *self.calls.lock().unwrap() += 1;
            assert!(input.description.contains("full description"));
            Ok(ExtractedAttrs {
                fibre: Some(true),
                ..Default::default()
            })
        }
    }

    async fn setup() -> (Db, Uuid) {
        let db = Db::connect(FsPath::new(":memory:")).await.unwrap();
        let (l, _) = db
            .upsert_listing(&crate::db::tests_listing_helper("https://x/1", 30_000_000))
            .await
            .unwrap();
        db.enqueue_enrichment(l.id, "new").await.unwrap();
        (db, l.id)
    }

    fn llm_handle(extractor: Option<Arc<dyn crate::llm::LlmExtract>>) -> LlmHandle {
        Arc::new(tokio::sync::RwLock::new(crate::llm::LlmRuntime {
            extractor,
            ..Default::default()
        }))
    }

    fn config(dir: &FsPath) -> EnrichmentConfig {
        EnrichmentConfig {
            images_dir: dir.to_path_buf(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn full_enrichment_detail_images_extraction() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        let detail = ListingDetail {
            description: Some("the full description".into()),
            image_urls: vec![
                "https://cdn/a.jpg".into(),
                "https://cdn/b.webp?rule=x".into(),
            ],
            ..Default::default()
        };
        let (db, id) = setup().await;
        let source = FakeSource {
            detail: Some(detail),
            fail: false,
        };
        let llm_impl = Arc::new(FakeLlm {
            calls: Mutex::new(0),
        });
        let llm = llm_handle(Some(llm_impl.clone()));

        process_one(&db, &source, &FakeImages, &llm, &config(&tmp), id)
            .await
            .unwrap();

        let st = db.enrichment_state(id).await.unwrap();
        assert!(st.enriched_at.is_some() && st.extracted_at.is_some());
        assert_eq!(
            st.listing.description.as_deref(),
            Some("the full description")
        );
        assert_eq!(st.listing.attributes.fibre, Some(true));
        assert_eq!(*llm_impl.calls.lock().unwrap(), 1);
        assert!(db.pending_images(id, 10).await.unwrap().is_empty());
        assert!(tmp.join(id.to_string()).join("0.jpg").exists());
        assert!(
            tmp.join(id.to_string()).join("1.webp").exists(),
            "query string stripped"
        );
        assert_eq!(db.enrichment_depth().await.unwrap(), 0, "dequeued");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn no_detail_support_and_no_llm_still_completes() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        let (db, id) = setup().await;
        let source = FakeSource {
            detail: None,
            fail: false,
        };
        process_one(
            &db,
            &source,
            &FakeImages,
            &llm_handle(None),
            &config(&tmp),
            id,
        )
        .await
        .unwrap();
        let st = db.enrichment_state(id).await.unwrap();
        assert!(
            st.enriched_at.is_some(),
            "marked enriched even without detail"
        );
        assert!(st.extracted_at.is_none(), "no llm: no extraction claim");
        assert_eq!(db.enrichment_depth().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn detail_failure_propagates_for_retry() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        let (db, id) = setup().await;
        let source = FakeSource {
            detail: None,
            fail: true,
        };
        let err = process_one(
            &db,
            &source,
            &FakeImages,
            &llm_handle(None),
            &config(&tmp),
            id,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("blocked"));
        assert_eq!(
            db.enrichment_depth().await.unwrap(),
            1,
            "stays queued for retry"
        );
    }

    #[test]
    fn extension_default_and_stripping() {
        assert_eq!(extension_of("https://cdn/a.jpg"), "jpg");
        assert_eq!(extension_of("https://cdn/a.webp?rule=classified"), "webp");
        assert_eq!(extension_of("https://cdn/no-extension"), "jpg");
        assert_eq!(extension_of("https://cdn/x.superlongext"), "jpg");
    }
}
