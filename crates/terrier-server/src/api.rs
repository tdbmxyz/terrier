//! REST API. Single-user LAN/tailnet trust model — no auth (ferret
//! convention). Listings come with their price history inline.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use terrier_domain::{ListingWithHistory, SearchRequest};
use uuid::Uuid;

use crate::db::DbError;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/status", get(status))
        .route("/api/searches", get(list_searches).post(create_search))
        .route(
            "/api/searches/{id}",
            axum::routing::put(update_search).delete(delete_search),
        )
        .route("/api/listings", get(list_listings))
        .route(
            "/api/listings/{id}/moderation",
            axum::routing::put(set_moderation),
        )
        .route("/api/communes", get(commune_stats))
        .route(
            "/api/settings/llm",
            get(get_llm_settings).put(put_llm_settings),
        )
        .route("/api/settings/prompts", get(get_prompts).put(put_prompts))
        .route("/api/llm/models", get(llm_models))
        .route("/api/llm/probe", axum::routing::post(llm_probe))
        .with_state(state)
}

async fn health() -> Response {
    Json(terrier_domain::HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        commit: Some(env!("TERRIER_COMMIT").into()),
    })
    .into_response()
}

struct ApiError(DbError);

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0 {
            DbError::NotFound => StatusCode::NOT_FOUND,
            _ => {
                tracing::error!(error = %self.0, "api database error");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        (status, self.0.to_string()).into_response()
    }
}

async fn status(State(state): State<AppState>) -> Result<Response, ApiError> {
    let mut sources: Vec<_> = state.statuses.read().await.values().cloned().collect();
    sources.sort_by(|a, b| a.source_id.cmp(&b.source_id));
    let search_matches = state.db.count_matches().await?;
    let enrichment_pending = state.db.enrichment_depth().await?;
    let llm = {
        let runtime = state.llm.read().await;
        let mut status = runtime.status.clone();
        status.busy = runtime.busy.load(std::sync::atomic::Ordering::SeqCst);
        Some(status)
    };
    Ok(Json(terrier_domain::StatusResponse {
        sources,
        search_matches,
        enrichment_pending,
        llm,
    })
    .into_response())
}

async fn list_searches(State(state): State<AppState>) -> Result<Response, ApiError> {
    Ok(Json(state.db.list_searches().await?).into_response())
}

async fn create_search(
    State(state): State<AppState>,
    Json(req): Json<SearchRequest>,
) -> Result<Response, ApiError> {
    let search = state.db.create_search(&req).await?;
    // instant feedback (ferret lesson): retro-match existing listings NOW
    let matched = retro_match(&state, &search).await.unwrap_or(0);
    let _ = crate::state::refresh_search_locations(
        &state.db,
        &state.shared_locations,
        state.location_cap,
    )
    .await;
    state
        .notifier
        .send(
            &format!("terrier: recherche « {} » créée", search.name),
            &if matched > 0 {
                format!("{matched} annonce(s) existante(s) correspondent déjà.")
            } else {
                "Aucune annonce existante ne correspond — les sources vont chercher.".into()
            },
            "mag",
            "default",
        )
        .await;
    Ok((StatusCode::CREATED, Json(search)).into_response())
}

/// Match all stored listings against a fresh/updated search; one summary
/// push instead of one per listing.
async fn retro_match(state: &AppState, search: &terrier_domain::Search) -> anyhow::Result<u64> {
    let mut matched = 0u64;
    for listing in state.db.list_listings(None, false).await? {
        if terrier_domain::search_matches(search, &listing) {
            if state.db.insert_match(listing.id, search.id).await? {
                state
                    .db
                    .mark_notified(listing.id, search.id, listing.price_cents)
                    .await?;
            }
            matched += 1;
        }
    }
    Ok(matched)
}

async fn update_search(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<SearchRequest>,
) -> Result<Response, ApiError> {
    let search = state.db.update_search(id, &req).await?;
    let _ = retro_match(&state, &search).await;
    let _ = crate::state::refresh_search_locations(
        &state.db,
        &state.shared_locations,
        state.location_cap,
    )
    .await;
    Ok(Json(search).into_response())
}

async fn delete_search(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    state.db.delete_search(id).await?;
    let _ = crate::state::refresh_search_locations(
        &state.db,
        &state.shared_locations,
        state.location_cap,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
struct ListingsQuery {
    search_id: Option<Uuid>,
    #[serde(default)]
    hidden: bool,
}

async fn list_listings(
    State(state): State<AppState>,
    Query(q): Query<ListingsQuery>,
) -> Result<Response, ApiError> {
    let listings = state.db.list_listings(q.search_id, q.hidden).await?;
    let ids: Vec<Uuid> = listings.iter().map(|l| l.id).collect();
    let mut histories = state.db.prices_for(&ids).await?;
    let mut images = state.db.images_for(&ids).await?;
    let out: Vec<ListingWithHistory> = listings
        .into_iter()
        .map(|listing| ListingWithHistory {
            history: histories.remove(&listing.id).unwrap_or_default(),
            images: images
                .remove(&listing.id)
                .unwrap_or_default()
                .into_iter()
                .map(|i| terrier_domain::ListingImage {
                    position: i.position,
                    url: match i.local_path {
                        Some(p) => format!("/images/{p}"),
                        None => i.url,
                    },
                })
                .collect(),
            listing,
        })
        .collect();
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
struct ModerationRequest {
    moderation: terrier_domain::Moderation,
}

async fn set_moderation(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<ModerationRequest>,
) -> Result<Response, ApiError> {
    state.db.set_moderation(id, req.moderation).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn commune_stats(State(state): State<AppState>) -> Result<Response, ApiError> {
    Ok(Json(state.db.commune_stats().await?).into_response())
}

async fn get_llm_settings(State(state): State<AppState>) -> Response {
    Json(state.llm.read().await.settings.clone()).into_response()
}

async fn put_llm_settings(
    State(state): State<AppState>,
    Json(mut update): Json<terrier_domain::LlmSettingsUpdate>,
) -> Result<Response, ApiError> {
    // api_key None = keep the previously stored key
    if update.api_key.is_none()
        && let Some(prev) = crate::llm::load_override(&state.db).await
    {
        update.api_key = prev.api_key;
    }
    state
        .db
        .put_setting(
            crate::llm::LLM_SETTINGS_KEY,
            &serde_json::to_string(&update).expect("settings serialize"),
        )
        .await?;
    let eff = crate::llm::effective(&state.llm_base, Some(&update))
        .map_err(|e| ApiError(DbError::Corrupt(e.to_string())))?;
    let prompts = state.llm.read().await.prompts.clone();
    let runtime = crate::llm::build_runtime(eff, prompts, Some(state.db.clone()));
    let settings = runtime.settings.clone();
    *state.llm.write().await = runtime;
    Ok(Json(settings).into_response())
}

async fn get_prompts(State(state): State<AppState>) -> Response {
    Json(state.llm.read().await.prompts.clone()).into_response()
}

async fn put_prompts(
    State(state): State<AppState>,
    Json(prompts): Json<terrier_domain::LlmPrompts>,
) -> Result<Response, ApiError> {
    state
        .db
        .put_setting(
            crate::llm::PROMPTS_SETTINGS_KEY,
            &serde_json::to_string(&prompts).expect("prompts serialize"),
        )
        .await?;
    let merged = crate::llm::effective_prompts(Some(&prompts));
    let override_ = crate::llm::load_override(&state.db).await;
    let eff = crate::llm::effective(&state.llm_base, override_.as_ref())
        .map_err(|e| ApiError(DbError::Corrupt(e.to_string())))?;
    let runtime = crate::llm::build_runtime(eff, merged.clone(), Some(state.db.clone()));
    *state.llm.write().await = runtime;
    Ok(Json(merged).into_response())
}

#[derive(Deserialize)]
struct LlmProbeRequest {
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    api_key: Option<String>,
}

async fn llm_models(Query(q): Query<LlmProbeRequest>) -> Response {
    match crate::llm::list_models(&q.base_url, q.api_key.as_deref()).await {
        Ok(models) => Json(models).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

async fn llm_probe(Json(q): Json<LlmProbeRequest>) -> Response {
    match crate::llm::probe(&q.base_url, &q.model, q.api_key.as_deref()).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path as FsPath;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use terrier_domain::Search;
    use tower::ServiceExt;

    use crate::db::Db;

    async fn app() -> Router {
        let db = Db::connect(FsPath::new(":memory:")).await.unwrap();
        router(AppState {
            db,
            notifier: Arc::new(crate::notify::NoopNotifier),
            statuses: Arc::new(tokio::sync::RwLock::new(Default::default())),
            shared_locations: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            location_cap: 20,
            llm: Default::default(),
            llm_base: Default::default(),
        })
    }

    async fn body_json<T: serde::de::DeserializeOwned>(resp: Response) -> T {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn search_lifecycle_over_http() {
        let app = app().await;
        let resp = app
            .clone()
            .oneshot(
                Request::post("/api/searches")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name": "maison bruz", "locations": ["Bruz 35170"],
                            "max_price_cents": 40000000, "property_types": ["house"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created: Search = body_json(resp).await;
        assert!(created.active, "active defaults to true");

        let resp = app
            .clone()
            .oneshot(Request::get("/api/searches").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let listed: Vec<Search> = body_json(resp).await;
        assert_eq!(listed.len(), 1);

        let resp = app
            .clone()
            .oneshot(
                Request::delete(format!("/api/searches/{}", created.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn listings_come_with_inline_history() {
        let db = Db::connect(FsPath::new(":memory:")).await.unwrap();
        let mut l = crate::db::tests_listing_helper("https://x/1", 30_000_000);
        l.commune = Some("Bruz".into());
        db.upsert_listing(&l).await.unwrap();
        let app = router(AppState {
            db,
            notifier: Arc::new(crate::notify::NoopNotifier),
            statuses: Arc::new(tokio::sync::RwLock::new(Default::default())),
            shared_locations: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            location_cap: 20,
            llm: Default::default(),
            llm_base: Default::default(),
        });
        let resp = app
            .oneshot(Request::get("/api/listings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let listings: Vec<ListingWithHistory> = body_json(resp).await;
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].history.len(), 1, "history rides along");
        assert_eq!(listings[0].history[0].price_cents, 30_000_000);
    }

    #[tokio::test]
    async fn listings_carry_images_with_local_urls() {
        let db = Db::connect(FsPath::new(":memory:")).await.unwrap();
        let l = crate::db::tests_listing_helper("https://x/1", 30_000_000);
        let (stored, _) = db.upsert_listing(&l).await.unwrap();
        db.add_image_urls(
            stored.id,
            &["https://cdn/a.jpg".into(), "https://cdn/b.jpg".into()],
        )
        .await
        .unwrap();
        db.mark_image_saved(stored.id, 0, &format!("{}/0.jpg", stored.id))
            .await
            .unwrap();
        let app = router(AppState {
            db,
            notifier: Arc::new(crate::notify::NoopNotifier),
            statuses: Arc::new(tokio::sync::RwLock::new(Default::default())),
            shared_locations: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            location_cap: 20,
            llm: Default::default(),
            llm_base: Default::default(),
        });
        let resp = app
            .oneshot(Request::get("/api/listings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let listings: Vec<ListingWithHistory> = body_json(resp).await;
        assert_eq!(listings[0].images.len(), 2);
        assert_eq!(
            listings[0].images[0].url,
            format!("/images/{}/0.jpg", stored.id)
        );
        assert_eq!(
            listings[0].images[1].url, "https://cdn/b.jpg",
            "not yet local: CDN url"
        );
    }

    #[tokio::test]
    async fn llm_settings_roundtrip() {
        let app = app().await;
        let resp = app
            .clone()
            .oneshot(
                Request::get("/api/settings/llm")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let s: terrier_domain::LlmSettings = body_json(resp).await;
        assert!(!s.enabled);

        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/settings/llm")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"enabled": true, "base_url": "http://zeus:8080/v1",
                            "model": "qwen3", "api_key": null}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let s: terrier_domain::LlmSettings = body_json(resp).await;
        assert!(s.enabled && s.from_override);
        assert_eq!(s.model, "qwen3");
    }
}
