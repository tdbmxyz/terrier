//! LLM configuration surface shared by the server API, client and UI.

use serde::{Deserialize, Serialize};

/// Effective settings as shown in the UI (never carries the key itself).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmSettings {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key_set: bool,
    pub from_override: bool,
}

/// UI → server: DB-stored override of the `[llm]` TOML section.
/// Empty url/model fall back to TOML; `api_key: None` keeps the stored key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmSettingsUpdate {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

/// Overridable system prompts (empty = built-in default).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmPrompts {
    pub extract: String,
}
