//! Liveness types for `GET /api/status` — the UI's answer to "is
//! anything actually scraping, and did my search match?".

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TickStats {
    pub fetched: u64,
    pub new_listings: u64,
    pub updated_listings: u64,
    pub skipped: u64,
    pub notified: u64,
    pub gone: u64,
    /// Matches recorded without a push (wanted ads).
    #[serde(default)]
    pub suppressed: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceStatus {
    pub source_id: String,
    pub interval_minutes: u64,
    pub last_tick: Option<DateTime<Utc>>,
    pub last_stats: Option<TickStats>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

impl SourceStatus {
    pub fn idle(source_id: &str, interval_minutes: u64) -> Self {
        Self {
            source_id: source_id.to_string(),
            interval_minutes,
            last_tick: None,
            last_stats: None,
            last_error: None,
            consecutive_failures: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub sources: Vec<SourceStatus>,
    /// Current match count per search id.
    pub search_matches: HashMap<Uuid, i64>,
}

/// `GET /api/health` — the connectivity probe, with build identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    #[serde(default)]
    pub commit: Option<String>,
}

/// Per-commune price aggregates for the Communes dashboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommuneStat {
    pub commune: String,
    pub postal_code: Option<String>,
    /// Active listings with a known surface.
    pub listings: i64,
    /// Median €/m² (cents) over active listings, now.
    pub median_m2_cents: Option<i64>,
    /// Same median computed over prices observed ≥ 30 days ago, when
    /// enough history exists — the trend anchor.
    pub median_m2_cents_30d: Option<i64>,
}
