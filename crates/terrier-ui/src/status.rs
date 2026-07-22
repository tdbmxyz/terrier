//! Sources liveness strip — is anything actually scraping?

use leptos::prelude::*;
use terrier_client::TerrierClient;
use terrier_domain::{SourceStatus, StatusResponse};

use crate::DataVersion;

#[derive(Clone, Copy)]
pub struct StatusResource(pub LocalResource<Option<StatusResponse>>);

pub fn provide_status(tick: RwSignal<u32>) {
    let client: TerrierClient = expect_context();
    let version: DataVersion = expect_context();
    let resource = LocalResource::new(move || {
        tick.track();
        version.0.track();
        let client = client.clone();
        async move { client.status().await.ok() }
    });
    provide_context(StatusResource(resource));
}

fn ago(status: &SourceStatus) -> String {
    match status.last_tick {
        None => "waiting for first pass".into(),
        Some(t) => {
            let mins = (chrono::Utc::now() - t).num_minutes();
            match mins {
                m if m < 1 => "just now".into(),
                m if m < 60 => format!("{m} min ago"),
                m => format!("{}h{:02} ago", m / 60, m % 60),
            }
        }
    }
}

#[component]
pub fn SourcesStrip() -> impl IntoView {
    let status: StatusResource = expect_context();
    view! {
        <div class="sources">
            {move || match status.0.get().flatten() {
                None => view! { <span class="muted">"checking sources…"</span> }.into_any(),
                Some(s) if s.sources.is_empty() => view! {
                    <span class="badge bad">"no sources configured — nothing will be scraped"</span>
                }
                .into_any(),
                Some(s) => s
                    .sources
                    .iter()
                    .map(|src| {
                        let label = match (&src.last_error, &src.last_stats) {
                            (Some(_), _) => format!("{} ✗ failing, {}", src.source_id, ago(src)),
                            (None, Some(st)) => format!(
                                "{} ✓ {} annonces, {}",
                                src.source_id, st.fetched, ago(src)
                            ),
                            (None, None) => format!(
                                "{} · every {} min, {}",
                                src.source_id, src.interval_minutes, ago(src)
                            ),
                        };
                        let bad = src.last_error.is_some();
                        view! {
                            <span class="badge" class:bad=bad class:ok=!bad
                                title=src.last_error.clone().unwrap_or_default()>
                                {label}
                            </span>
                        }
                    })
                    .collect_view()
                    .into_any(),
            }}
        </div>
    }
}
