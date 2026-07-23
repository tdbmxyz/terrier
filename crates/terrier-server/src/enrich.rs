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
///
/// The CDN host is distinct from the scrape host, so images deliberately
/// bypass the per-source politeness layer; their spacing comes from the
/// fixed 500 ms sleep between downloads in `process_one`.
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

/// One queue item, end to end. Each step runs even when an earlier one
/// failed — a permanently-404 image must never starve extraction. Step
/// errors are collected; if any step failed the whole call errors so the
/// caller's backoff retries, and the already-succeeded steps are skipped
/// on the retry by their idempotency gates (enriched_at, local_path,
/// extracted_at).
pub async fn process_one(
    db: &Db,
    source: &dyn ImmoSource,
    images: &dyn ImageFetch,
    llm: &LlmHandle,
    config: &EnrichmentConfig,
    id: Uuid,
) -> anyhow::Result<()> {
    let mut step_errors: Vec<String> = Vec::new();
    let state = db.enrichment_state(id).await?;

    // 1. detail page (skipped once enriched; price-change cleared the mark).
    // On failure we still continue: the search page's truncated description
    // is often already present, so images + extraction can proceed.
    if state.enriched_at.is_none() {
        match source.fetch_detail(&state.listing.canonical_url).await {
            // an empty detail still marks the listing enriched — sources
            // without detail support are done after images + extraction
            Ok(detail) => {
                db.set_detail(id, &detail.unwrap_or_default()).await?;
            }
            Err(e) => {
                tracing::warn!(%id, error = %e, "detail fetch failed");
                step_errors.push(format!("detail: {e}"));
            }
        }
    }

    // 2. images: fetch once each, capped per listing (already-saved images
    // count against the cap so re-enqueues can't exceed it). Skipping over
    // the cap is fine; a failed download is a step error (retried later).
    let already = db.saved_image_count(id).await?;
    let budget = (config.max_images as i64 - already).max(0);
    for (position, url) in db.pending_images(id, budget).await? {
        let saved: anyhow::Result<()> = async {
            let bytes = images.fetch(&url).await?;
            let dir = config.images_dir.join(id.to_string());
            std::fs::create_dir_all(&dir)?;
            let file = format!("{position}.{}", extension_of(&url));
            std::fs::write(dir.join(&file), &bytes)?;
            db.mark_image_saved(id, position, &format!("{id}/{file}"))
                .await?;
            Ok(())
        }
        .await;
        if let Err(e) = saved {
            tracing::warn!(%id, position, error = %e, "image download failed");
            step_errors.push(format!("image {position}: {e}"));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 3. LLM extraction over whatever description we have
    let state = db.enrichment_state(id).await?;
    if state.extracted_at.is_none()
        && let Some(description) = &state.listing.description
    {
        let extractor = llm.read().await.extractor.clone();
        if let Some(extractor) = extractor {
            match extractor
                .extract(&ExtractInput {
                    title: &state.listing.title,
                    price_cents: state.listing.price_cents,
                    property_type: state.listing.property_type.label(),
                    surface_m2: state.listing.surface_m2,
                    rooms: state.listing.rooms,
                    description,
                })
                .await
            {
                // Structured attributes from the detail page (already stored)
                // are authoritative; the LLM only fills the prose-only gaps.
                Ok(attrs) => {
                    let mut merged = state.listing.attributes.clone();
                    merged.fill_gaps_from(&attrs);
                    db.set_attributes(id, &merged).await?
                }
                Err(e) => {
                    tracing::warn!(%id, error = %e, "llm extraction failed");
                    step_errors.push(format!("extract: {e}"));
                }
            }
        }
        // llm disabled: fine — the listing is done without attributes
    }

    if step_errors.is_empty() {
        db.enrichment_done(id).await?;
        Ok(())
    } else {
        // no enrichment_done: the caller's backoff keeps the item queued
        Err(anyhow::anyhow!(step_errors.join("; ")))
    }
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

    struct FailingImages;
    #[async_trait::async_trait]
    impl ImageFetch for FailingImages {
        async fn fetch(&self, _url: &str) -> anyhow::Result<Vec<u8>> {
            anyhow::bail!("404 from cdn");
        }
    }

    struct FakeLlm {
        calls: Mutex<u32>,
        expect: &'static str,
    }
    #[async_trait::async_trait]
    impl crate::llm::LlmExtract for FakeLlm {
        async fn extract(&self, input: &ExtractInput<'_>) -> anyhow::Result<ExtractedAttrs> {
            *self.calls.lock().unwrap() += 1;
            assert!(input.description.contains(self.expect));
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
            expect: "full description",
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
    async fn structured_attributes_win_over_llm_which_fills_gaps() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        // detail carries authoritative structured facts
        let detail = ListingDetail {
            description: Some("the full description".into()),
            attributes: ExtractedAttrs {
                chauffage_energie: Some("gaz".into()),
                orientation: Some("sud".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let (db, id) = setup().await;
        let source = FakeSource {
            detail: Some(detail),
            fail: false,
        };
        // FakeLlm returns fibre=true (prose-only) and nothing else, so the
        // structured chauffage/orientation must survive and fibre be adopted.
        let llm_impl = Arc::new(FakeLlm {
            calls: Mutex::new(0),
            expect: "full description",
        });
        let llm = llm_handle(Some(llm_impl.clone()));

        process_one(&db, &source, &FakeImages, &llm, &config(&tmp), id)
            .await
            .unwrap();

        let attrs = db.enrichment_state(id).await.unwrap().listing.attributes;
        assert_eq!(attrs.chauffage_energie.as_deref(), Some("gaz"));
        assert_eq!(attrs.orientation.as_deref(), Some("sud"));
        assert_eq!(attrs.fibre, Some(true), "prose-only fact from the llm");
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

    #[tokio::test]
    async fn image_failure_still_extracts() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        let detail = ListingDetail {
            description: Some("the full description".into()),
            image_urls: vec!["https://cdn/dead.jpg".into()],
            ..Default::default()
        };
        let (db, id) = setup().await;
        let source = FakeSource {
            detail: Some(detail),
            fail: false,
        };
        let llm_impl = Arc::new(FakeLlm {
            calls: Mutex::new(0),
            expect: "full description",
        });
        let llm = llm_handle(Some(llm_impl.clone()));

        let err = process_one(&db, &source, &FailingImages, &llm, &config(&tmp), id)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404 from cdn"));

        let st = db.enrichment_state(id).await.unwrap();
        assert!(
            st.extracted_at.is_some(),
            "extraction ran despite image 404"
        );
        assert_eq!(st.listing.attributes.fibre, Some(true));
        assert_eq!(*llm_impl.calls.lock().unwrap(), 1);
        assert_eq!(
            db.enrichment_depth().await.unwrap(),
            1,
            "stays queued so the image retries"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn detail_failure_still_runs_images_and_extraction() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        let db = Db::connect(FsPath::new(":memory:")).await.unwrap();
        // search page already gave a truncated description and an image
        let mut listing = crate::db::tests_listing_helper("https://x/1", 30_000_000);
        listing.description = Some("truncated search-page description".into());
        let (l, _) = db.upsert_listing(&listing).await.unwrap();
        db.add_image_urls(l.id, &["https://cdn/a.jpg".into()])
            .await
            .unwrap();
        db.enqueue_enrichment(l.id, "new").await.unwrap();

        let source = FakeSource {
            detail: None,
            fail: true,
        };
        let llm_impl = Arc::new(FakeLlm {
            calls: Mutex::new(0),
            expect: "truncated search-page",
        });
        let llm = llm_handle(Some(llm_impl.clone()));

        let err = process_one(&db, &source, &FakeImages, &llm, &config(&tmp), l.id)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked"));

        let st = db.enrichment_state(l.id).await.unwrap();
        assert!(
            st.extracted_at.is_some(),
            "extraction ran despite detail failure"
        );
        assert_eq!(*llm_impl.calls.lock().unwrap(), 1);
        assert!(
            db.pending_images(l.id, 10).await.unwrap().is_empty(),
            "image downloaded despite detail failure"
        );
        assert_eq!(db.enrichment_depth().await.unwrap(), 1, "detail retries");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[tokio::test]
    async fn image_cap_is_per_listing_and_not_an_error() {
        let tmp = std::env::temp_dir().join(format!("terrier-test-{}", Uuid::new_v4()));
        let detail = ListingDetail {
            description: None,
            image_urls: vec!["https://cdn/a.jpg".into(), "https://cdn/b.jpg".into()],
            ..Default::default()
        };
        let (db, id) = setup().await;
        let source = FakeSource {
            detail: Some(detail),
            fail: false,
        };
        let cfg = EnrichmentConfig {
            max_images: 1,
            ..config(&tmp)
        };

        process_one(&db, &source, &FakeImages, &llm_handle(None), &cfg, id)
            .await
            .unwrap();
        assert_eq!(
            db.pending_images(id, 10).await.unwrap().len(),
            1,
            "second image stays pending, not downloaded"
        );
        assert_eq!(
            db.enrichment_depth().await.unwrap(),
            0,
            "cap is not an error"
        );

        // a re-enqueue (e.g. price change) must not exceed the cap
        db.enqueue_enrichment(id, "price-change").await.unwrap();
        process_one(&db, &source, &FakeImages, &llm_handle(None), &cfg, id)
            .await
            .unwrap();
        assert_eq!(
            db.pending_images(id, 10).await.unwrap().len(),
            1,
            "cap holds across runs: still only one image saved"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn extension_default_and_stripping() {
        assert_eq!(extension_of("https://cdn/a.jpg"), "jpg");
        assert_eq!(extension_of("https://cdn/a.webp?rule=classified"), "webp");
        assert_eq!(extension_of("https://cdn/no-extension"), "jpg");
        assert_eq!(extension_of("https://cdn/x.superlongext"), "jpg");
    }
}
