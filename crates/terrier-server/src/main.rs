mod api;
mod config;
mod db;
mod enrich;
mod llm;
mod notify;
mod pipeline;
mod politeness;
mod scheduler;
mod scrape;
mod state;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tracing_subscriber::EnvFilter;

use crate::notify::{NoopNotifier, Notify, NtfyNotifier};
use crate::scrape::ImmoSource;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = config::load().context("loading configuration")?;
    let db = db::Db::connect(&config.db_path)
        .await
        .with_context(|| format!("opening database {}", config.db_path.display()))?;

    let llm_override = llm::load_override(&db).await;
    let prompts = llm::effective_prompts(llm::load_prompts(&db).await.as_ref());
    let llm_eff = llm::effective(&config.llm, llm_override.as_ref())
        .context("resolving llm configuration")?;
    if llm_eff.enabled {
        tracing::info!(model = %llm_eff.model, url = %llm_eff.base_url, "llm extraction enabled");
    }
    let llm_handle: llm::LlmHandle = Arc::new(tokio::sync::RwLock::new(llm::build_runtime(
        llm_eff,
        prompts,
        Some(db.clone()),
    )));

    let notifier: Arc<dyn Notify> = match NtfyNotifier::new(&config.notifications)
        .context("configuring ntfy notifier")?
    {
        Some(n) => {
            tracing::info!("ntfy notifications enabled");
            Arc::new(n)
        }
        None => Arc::new(NoopNotifier),
    };

    // active searches' locations feed the scrape rotation
    let shared_locations: state::SharedLocations =
        Arc::new(tokio::sync::RwLock::new(Vec::new()));
    state::refresh_search_locations(&db, &shared_locations, config.scrape.max_search_locations)
        .await
        .context("loading search locations")?;

    let mut sources: Vec<(Arc<dyn ImmoSource>, Duration)> = Vec::new();
    if config.leboncoin.enabled {
        let lbc = &config.leboncoin;
        let client =
            politeness::scrape_client(Duration::from_millis(lbc.delay_ms), 1);
        sources.push((
            Arc::new(scrape::leboncoin::LeboncoinSource::new(
                lbc.clone(),
                client,
                Some(shared_locations.clone()),
            )),
            Duration::from_secs(lbc.interval_minutes * 60),
        ));
    }
    if config.ouestfrance.enabled {
        sources.push((
            Arc::new(scrape::ouestfrance::OuestFranceSource::new(
                config.ouestfrance.clone(),
                Some(shared_locations.clone()),
            )),
            Duration::from_secs(config.ouestfrance.interval_minutes * 60),
        ));
    }
    tracing::info!(sources = sources.len(), "configuration loaded");

    let statuses: state::StatusMap = Arc::new(tokio::sync::RwLock::new(Default::default()));
    // keep a handle on each source for its enrichment worker
    let enrich_sources: Vec<Arc<dyn ImmoSource>> =
        sources.iter().map(|(s, _)| s.clone()).collect();
    scheduler::spawn_all(
        sources,
        db.clone(),
        config.scrape.clone(),
        notifier.clone(),
        statuses.clone(),
    );
    for source in enrich_sources {
        tokio::spawn(enrich::run_source_enricher(
            source,
            db.clone(),
            config.enrichment.clone(),
            llm_handle.clone(),
        ));
    }

    let mut app = api::router(state::AppState {
        db,
        notifier,
        statuses,
        shared_locations,
        location_cap: config.scrape.max_search_locations,
        llm: llm_handle.clone(),
        llm_base: config.llm.clone(),
    })
    .layer(tower_http::cors::CorsLayer::permissive())
    .layer(tower_http::trace::TraceLayer::new_for_http());

    let images_dir = enrich::images_root(&config.enrichment);
    std::fs::create_dir_all(&images_dir).ok();
    app = app.nest_service("/images", tower_http::services::ServeDir::new(&images_dir));

    if let Some(dir) = &config.static_dir {
        let index = dir.join("index.html");
        app = app.fallback_service(
            tower_http::services::ServeDir::new(dir)
                .fallback(tower_http::services::ServeFile::new(index)),
        );
        tracing::info!(dir = %dir.display(), "serving web frontend");
    }

    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    tracing::info!(listen = %config.listen, "terrier-server up");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("server error")
}
