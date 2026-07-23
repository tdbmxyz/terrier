//! Deal notifications via ntfy. Best-effort by design: a failed publish is
//! a warning in the log, never an error that stalls the pipeline. Fully
//! off (no client, no task) when `[notifications].ntfy_url` is unset.

use std::time::Duration;

use url::Url;

use crate::config::NotificationsConfig;

/// Pipeline-facing notification sink; the integration test provides a
/// recording impl instead of hitting ntfy.
#[async_trait::async_trait]
pub trait Notify: Send + Sync {
    /// Publish one notification. Infallible from the caller's view.
    async fn send(&self, title: &str, message: &str, tags: &str, priority: &str);
}

/// No-op sink used when notifications are disabled.
pub struct NoopNotifier;

#[async_trait::async_trait]
impl Notify for NoopNotifier {
    async fn send(&self, _title: &str, _message: &str, _tags: &str, _priority: &str) {}
}

pub struct NtfyNotifier {
    http: reqwest::Client,
    /// `{ntfy_url}/{topic}` — ntfy publishes with a plain POST to the topic.
    pub(crate) endpoint: Url,
    token: Option<String>,
}

impl NtfyNotifier {
    /// `None` when notifications aren't configured (`ntfy_url` unset).
    pub fn new(config: &NotificationsConfig) -> anyhow::Result<Option<Self>> {
        let Some(base) = config.ntfy_url.clone() else {
            return Ok(None);
        };
        let topic = config.topic.trim();
        anyhow::ensure!(
            !topic.is_empty(),
            "notifications.ntfy_url is set but notifications.topic is empty"
        );
        let endpoint = base
            .join(topic)
            .map_err(|e| anyhow::anyhow!("joining ntfy topic onto {base}: {e}"))?;
        let token = match &config.token_file {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("reading ntfy token {}: {e}", path.display()))?
                    .trim()
                    .to_string(),
            ),
            None => None,
        };
        Ok(Some(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .user_agent(concat!("terrier/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("building ntfy http client"),
            endpoint,
            token,
        }))
    }
}

#[async_trait::async_trait]
impl Notify for NtfyNotifier {
    async fn send(&self, title: &str, message: &str, tags: &str, priority: &str) {
        let mut request = self
            .http
            .post(self.endpoint.clone())
            .header("Title", title)
            .header("Tags", tags)
            .header("Priority", priority)
            .body(message.to_string());
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        match request.send().await {
            Ok(resp) if !resp.status().is_success() => {
                tracing::warn!(status = %resp.status(), "ntfy publish rejected");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "ntfy publish failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_endpoint_from_url_and_topic() {
        let config = NotificationsConfig {
            ntfy_url: Some(url::Url::parse("https://notify.zeus.balem.fr").unwrap()),
            topic: "deals-zeus".into(),
            token_file: None,
        };
        let notifier = NtfyNotifier::new(&config).unwrap().unwrap();
        assert_eq!(
            notifier.endpoint.as_str(),
            "https://notify.zeus.balem.fr/deals-zeus"
        );
    }

    #[test]
    fn disabled_when_url_unset() {
        let notifier = NtfyNotifier::new(&NotificationsConfig::default()).unwrap();
        assert!(notifier.is_none());
    }

    #[test]
    fn empty_topic_is_an_error() {
        let config = NotificationsConfig {
            ntfy_url: Some(url::Url::parse("https://ntfy.sh").unwrap()),
            topic: "".into(),
            token_file: None,
        };
        assert!(NtfyNotifier::new(&config).is_err());
    }
}
