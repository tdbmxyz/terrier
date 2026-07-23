//! Shared application state for the axum handlers.

use std::collections::HashMap;
use std::sync::Arc;

use terrier_domain::SourceStatus;
use tokio::sync::RwLock;

use crate::db::Db;
use crate::notify::Notify;

pub type StatusMap = Arc<RwLock<HashMap<String, SourceStatus>>>;

/// Active searches' locations, merged into each source's configured
/// locations at fetch time. Refreshed by the search API handlers.
pub type SharedLocations = Arc<RwLock<Vec<String>>>;

pub async fn refresh_search_locations(
    db: &Db,
    shared: &SharedLocations,
    cap: usize,
) -> crate::db::Result<()> {
    let mut locations: Vec<String> = Vec::new();
    for search in db.list_searches().await? {
        if !search.active {
            continue;
        }
        for l in search.locations {
            let l = l.trim().to_string();
            if !l.is_empty() && !locations.iter().any(|x| x.eq_ignore_ascii_case(&l)) {
                locations.push(l);
            }
        }
    }
    locations.truncate(cap);
    *shared.write().await = locations;
    Ok(())
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub notifier: Arc<dyn Notify>,
    pub statuses: StatusMap,
    pub shared_locations: SharedLocations,
    pub location_cap: usize,
    pub llm: crate::llm::LlmHandle,
    /// TOML base for the [llm] section (settings PUT merges over it).
    pub llm_base: crate::config::LlmConfig,
}
