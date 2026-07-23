//! Per-source scheduling: one tokio task per source with exponential
//! backoff and a single ntfy alert per outage (ferret's scheduler,
//! without the LLM plumbing).

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use terrier_domain::{SourceStatus, TickStats};

use crate::config::ScrapeConfig;
use crate::db::Db;
use crate::notify::Notify;
use crate::pipeline;
use crate::scrape::ImmoSource;
use crate::state::StatusMap;

const BACKOFF_BASE: Duration = Duration::from_secs(60);
const BACKOFF_CAP: Duration = Duration::from_secs(3600);

pub struct FailureState {
    consecutive: u32,
    alert_after: u32,
    alerted: bool,
}

impl FailureState {
    pub fn new(alert_after: u32) -> Self {
        Self {
            consecutive: 0,
            alert_after,
            alerted: false,
        }
    }

    pub fn record_failure(&mut self) -> Duration {
        self.consecutive = self.consecutive.saturating_add(1);
        let factor = 2u32.saturating_pow(self.consecutive.saturating_sub(1).min(6));
        (BACKOFF_BASE * factor).min(BACKOFF_CAP)
    }

    pub fn should_alert(&mut self) -> bool {
        if !self.alerted && self.consecutive >= self.alert_after {
            self.alerted = true;
            return true;
        }
        false
    }

    pub fn record_success(&mut self) {
        self.consecutive = 0;
        self.alerted = false;
    }
}

pub fn spawn_all(
    sources: Vec<(Arc<dyn ImmoSource>, Duration)>,
    db: Db,
    scrape: ScrapeConfig,
    notifier: Arc<dyn Notify>,
    statuses: StatusMap,
) {
    for (source, interval) in sources {
        let db = db.clone();
        let scrape = scrape.clone();
        let notifier = notifier.clone();
        let statuses = statuses.clone();
        tokio::spawn(async move {
            statuses.write().await.insert(
                source.id().to_string(),
                SourceStatus::idle(source.id(), interval.as_secs() / 60),
            );
            run_source(source, interval, db, scrape, notifier, statuses).await;
        });
    }
}

async fn record_tick(statuses: &StatusMap, source_id: &str, result: Result<TickStats, String>) {
    let mut map = statuses.write().await;
    if let Some(status) = map.get_mut(source_id) {
        status.last_tick = Some(Utc::now());
        match result {
            Ok(stats) => {
                status.last_stats = Some(stats);
                status.last_error = None;
                status.consecutive_failures = 0;
            }
            Err(error) => {
                status.last_error = Some(error);
                status.consecutive_failures += 1;
            }
        }
    }
}

async fn run_source(
    source: Arc<dyn ImmoSource>,
    interval: Duration,
    db: Db,
    scrape: ScrapeConfig,
    notifier: Arc<dyn Notify>,
    statuses: StatusMap,
) {
    let mut failures = FailureState::new(scrape.failure_alert_after);
    loop {
        match source.fetch().await {
            Ok(listings) => {
                let count = listings.len();
                match pipeline::process_listings(
                    &db,
                    &scrape,
                    source.id(),
                    listings,
                    notifier.as_ref(),
                    true,
                )
                .await
                {
                    Ok(stats) => {
                        failures.record_success();
                        tracing::info!(
                            source = source.id(),
                            fetched = count,
                            new = stats.new_listings,
                            updated = stats.updated_listings,
                            notified = stats.notified,
                            suppressed = stats.suppressed,
                            gone = stats.gone,
                            "tick done"
                        );
                        record_tick(
                            &statuses,
                            source.id(),
                            Ok(TickStats {
                                fetched: count as u64,
                                new_listings: stats.new_listings,
                                updated_listings: stats.updated_listings,
                                skipped: stats.skipped,
                                notified: stats.notified,
                                gone: stats.gone,
                                suppressed: stats.suppressed,
                            }),
                        )
                        .await;
                    }
                    Err(e) => {
                        let backoff = failures.record_failure();
                        tracing::error!(source = source.id(), error = %e, ?backoff, "pipeline failed");
                        record_tick(&statuses, source.id(), Err(e.to_string())).await;
                        maybe_alert(&mut failures, source.id(), &e, notifier.as_ref()).await;
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                }
            }
            Err(e) => {
                let backoff = failures.record_failure();
                tracing::warn!(source = source.id(), error = %e, ?backoff, "fetch failed");
                record_tick(&statuses, source.id(), Err(e.to_string())).await;
                maybe_alert(&mut failures, source.id(), &e, notifier.as_ref()).await;
                tokio::time::sleep(backoff).await;
                continue;
            }
        }
        tokio::time::sleep(interval).await;
    }
}

async fn maybe_alert(
    failures: &mut FailureState,
    source_id: &str,
    error: &anyhow::Error,
    notifier: &dyn Notify,
) {
    if failures.should_alert() {
        notifier
            .send(
                &format!("terrier: source {source_id} is failing"),
                &format!("Repeated scrape failures, backing off.\nLast error: {error}"),
                "warning,terrier",
                "high",
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let mut fs = FailureState::new(3);
        assert_eq!(fs.record_failure(), Duration::from_secs(60));
        assert_eq!(fs.record_failure(), Duration::from_secs(120));
        for _ in 0..10 {
            fs.record_failure();
        }
        assert_eq!(fs.record_failure(), Duration::from_secs(3600));
    }

    #[test]
    fn alerts_once_per_outage_and_rearms() {
        let mut fs = FailureState::new(2);
        fs.record_failure();
        assert!(!fs.should_alert());
        fs.record_failure();
        assert!(fs.should_alert());
        assert!(!fs.should_alert());
        fs.record_success();
        fs.record_failure();
        fs.record_failure();
        assert!(fs.should_alert());
    }
}
