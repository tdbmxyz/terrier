//! Typed client for the terrier HTTP API — dual native/wasm via reqwest.

use std::time::Duration;

use serde::Serialize;
use terrier_domain::{
    CommuneStat, HealthResponse, ListingWithHistory, LlmPrompts, LlmSettings, LlmSettingsUpdate,
    Moderation, Search, SearchRequest, StatusResponse,
};
use url::Url;
use uuid::Uuid;

const DATA_TIMEOUT: Duration = Duration::from_secs(10);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, thiserror::Error)]
pub enum ClientError {
    #[error("request failed: {0}")]
    Transport(String),
    #[error("server returned {status}: {message}")]
    Api { status: u16, message: String },
    #[error("invalid response body: {0}")]
    Decode(String),
}

pub type Result<T> = std::result::Result<T, ClientError>;

#[derive(Debug, Clone)]
pub struct TerrierClient {
    base: Url,
    http: reqwest::Client,
}

impl TerrierClient {
    pub fn new(base: Url) -> Self {
        Self {
            base,
            http: reqwest::Client::new(),
        }
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base
            .join(path)
            .map_err(|e| ClientError::Transport(format!("bad url {path:?}: {e}")))
    }

    async fn send<T: serde::de::DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
        timeout: Duration,
    ) -> Result<T> {
        let mut request = request
            .build()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        *request.timeout_mut() = Some(timeout);
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(ClientError::Api {
                status: status.as_u16(),
                message,
            });
        }
        response
            .json()
            .await
            .map_err(|e| ClientError::Decode(e.to_string()))
    }

    async fn send_empty(&self, request: reqwest::RequestBuilder) -> Result<()> {
        let mut request = request
            .build()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        *request.timeout_mut() = Some(DATA_TIMEOUT);
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(ClientError::Api {
                status: status.as_u16(),
                message,
            });
        }
        Ok(())
    }

    pub async fn health(&self) -> Result<HealthResponse> {
        self.send(self.http.get(self.url("api/health")?), HEALTH_TIMEOUT)
            .await
    }

    pub async fn status(&self) -> Result<StatusResponse> {
        self.send(self.http.get(self.url("api/status")?), DATA_TIMEOUT)
            .await
    }

    pub async fn searches(&self) -> Result<Vec<Search>> {
        self.send(self.http.get(self.url("api/searches")?), DATA_TIMEOUT)
            .await
    }

    pub async fn create_search(&self, request: &SearchRequest) -> Result<Search> {
        self.send(
            self.http.post(self.url("api/searches")?).json(request),
            DATA_TIMEOUT,
        )
        .await
    }

    pub async fn update_search(&self, id: Uuid, request: &SearchRequest) -> Result<Search> {
        self.send(
            self.http
                .put(self.url(&format!("api/searches/{id}"))?)
                .json(request),
            DATA_TIMEOUT,
        )
        .await
    }

    pub async fn delete_search(&self, id: Uuid) -> Result<()> {
        self.send_empty(self.http.delete(self.url(&format!("api/searches/{id}"))?))
            .await
    }

    /// Listings with their full price history inline.
    /// `hidden = true` lists ONLY dismissed/banned listings.
    pub async fn listings(
        &self,
        search_id: Option<Uuid>,
        hidden: bool,
    ) -> Result<Vec<ListingWithHistory>> {
        let path = match (search_id, hidden) {
            (Some(id), h) => format!("api/listings?search_id={id}&hidden={h}"),
            (None, true) => "api/listings?hidden=true".into(),
            (None, false) => "api/listings".into(),
        };
        self.send(self.http.get(self.url(&path)?), DATA_TIMEOUT)
            .await
    }

    pub async fn set_moderation(&self, listing_id: Uuid, moderation: Moderation) -> Result<()> {
        #[derive(Serialize)]
        struct Body {
            moderation: Moderation,
        }
        self.send_empty(
            self.http
                .put(self.url(&format!("api/listings/{listing_id}/moderation"))?)
                .json(&Body { moderation }),
        )
        .await
    }

    pub async fn communes(&self) -> Result<Vec<CommuneStat>> {
        self.send(self.http.get(self.url("api/communes")?), DATA_TIMEOUT)
            .await
    }

    pub async fn llm_settings(&self) -> Result<LlmSettings> {
        self.send(self.http.get(self.url("api/settings/llm")?), DATA_TIMEOUT)
            .await
    }

    pub async fn update_llm_settings(&self, update: &LlmSettingsUpdate) -> Result<LlmSettings> {
        self.send(
            self.http.put(self.url("api/settings/llm")?).json(update),
            DATA_TIMEOUT,
        )
        .await
    }

    pub async fn llm_prompts(&self) -> Result<LlmPrompts> {
        self.send(
            self.http.get(self.url("api/settings/prompts")?),
            DATA_TIMEOUT,
        )
        .await
    }

    pub async fn update_llm_prompts(&self, prompts: &LlmPrompts) -> Result<LlmPrompts> {
        self.send(
            self.http
                .put(self.url("api/settings/prompts")?)
                .json(prompts),
            DATA_TIMEOUT,
        )
        .await
    }

    /// POST so the api_key never lands in query-string logs
    /// (model is unused server-side, which is fine).
    pub async fn llm_models(&self, update: &LlmSettingsUpdate) -> Result<Vec<String>> {
        #[derive(Serialize)]
        struct Body<'a> {
            base_url: &'a str,
            model: &'a str,
            api_key: &'a Option<String>,
        }
        self.send(
            self.http.post(self.url("api/llm/models")?).json(&Body {
                base_url: &update.base_url,
                model: &update.model,
                api_key: &update.api_key,
            }),
            DATA_TIMEOUT,
        )
        .await
    }

    /// The settings panel's "Test": one tiny completion (slow local models).
    pub async fn llm_probe(&self, update: &LlmSettingsUpdate) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            base_url: &'a str,
            model: &'a str,
            api_key: &'a Option<String>,
        }
        let mut request = self
            .http
            .post(self.url("api/llm/probe")?)
            .json(&Body {
                base_url: &update.base_url,
                model: &update.model,
                api_key: &update.api_key,
            })
            .build()
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        *request.timeout_mut() = Some(Duration::from_secs(120));
        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| ClientError::Transport(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(ClientError::Api {
                status: status.as_u16(),
                message,
            });
        }
        Ok(())
    }
}
