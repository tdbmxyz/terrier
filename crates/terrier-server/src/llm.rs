//! LLM extraction of structured facts from listing descriptions, via any
//! OpenAI-compatible chat-completions API (llama.cpp on zeus by default).
//! Ported from ferret's llm.rs: one structured-output call per listing,
//! every error fail-open — the LLM is a refinement layer, never a
//! dependency.

use std::time::Duration;

use serde::Deserialize;
use terrier_domain::{ExtractedAttrs, LlmPrompts, LlmSettings, LlmSettingsUpdate, LlmStatus};

use crate::config::LlmConfig;

/// What the extractor sees about a listing.
pub struct ExtractInput<'a> {
    pub title: &'a str,
    pub price_cents: i64,
    pub property_type: &'a str,
    pub surface_m2: Option<f64>,
    pub rooms: Option<i64>,
    pub description: &'a str,
}

#[async_trait::async_trait]
pub trait LlmExtract: Send + Sync {
    async fn extract(&self, input: &ExtractInput<'_>) -> anyhow::Result<ExtractedAttrs>;
}

/// One logged LLM call (the llm_requests table).
#[derive(Debug, Clone)]
pub struct LlmLogEntry {
    pub kind: String,
    pub model: String,
    pub duration_ms: i64,
    pub ok: bool,
    pub error: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
}

pub struct OpenAiExtractor {
    http: reqwest::Client,
    url: String,
    model: String,
    api_key: Option<String>,
    prompts: LlmPrompts,
    /// In-flight call count, shared with `/api/status` via the runtime.
    busy: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Request log sink (None in unit tests).
    db: Option<crate::db::Db>,
}

// ---- runtime configuration: TOML base + DB override, hot-swappable ----

pub const LLM_SETTINGS_KEY: &str = "llm";
pub const PROMPTS_SETTINGS_KEY: &str = "prompts";

/// Fully resolved LLM configuration, ready to build clients from.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveLlm {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub timeout_secs: u64,
    pub api_key: Option<String>,
    pub from_override: bool,
    pub override_key_set: bool,
}

/// Merge the TOML base with an optional DB override. The key file is only
/// read when the result is enabled — a broken path never blocks startup of
/// a disabled pass.
pub fn effective(base: &LlmConfig, o: Option<&LlmSettingsUpdate>) -> anyhow::Result<EffectiveLlm> {
    let pick = |over: &str, conf: &str| {
        if over.trim().is_empty() {
            conf.to_string()
        } else {
            over.trim().to_string()
        }
    };
    let (enabled, base_url, model) = match o {
        Some(o) => (
            o.enabled,
            pick(&o.base_url, &base.base_url),
            pick(&o.model, &base.model),
        ),
        None => (base.enabled, base.base_url.clone(), base.model.clone()),
    };
    let override_key = o.and_then(|o| o.api_key.clone()).filter(|k| !k.is_empty());
    let override_key_set = override_key.is_some();
    let api_key = match (&override_key, enabled) {
        (Some(key), _) => Some(key.clone()),
        (None, true) => match &base.api_key_file {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("reading llm api key {}: {e}", path.display()))?
                    .trim()
                    .to_string(),
            ),
            None => None,
        },
        (None, false) => None,
    };
    Ok(EffectiveLlm {
        enabled,
        base_url,
        model,
        timeout_secs: base.timeout_secs,
        api_key,
        from_override: o.is_some(),
        override_key_set,
    })
}

/// The live LLM layer, swapped in place when settings change so the
/// enrichment workers and API handlers pick the new backend up without a
/// restart.
#[derive(Clone, Default)]
pub struct LlmRuntime {
    pub extractor: Option<std::sync::Arc<dyn LlmExtract>>,
    pub status: LlmStatus,
    pub settings: LlmSettings,
    pub prompts: LlmPrompts,
    pub busy: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

pub type LlmHandle = std::sync::Arc<tokio::sync::RwLock<LlmRuntime>>;

pub fn build_runtime(
    eff: EffectiveLlm,
    prompts: LlmPrompts,
    db: Option<crate::db::Db>,
) -> LlmRuntime {
    let busy = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let extractor = eff.enabled.then(|| {
        std::sync::Arc::new(OpenAiExtractor {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(eff.timeout_secs))
                .user_agent(concat!("terrier/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("building llm http client"),
            url: format!("{}/chat/completions", eff.base_url.trim_end_matches('/')),
            model: eff.model.clone(),
            api_key: eff.api_key.clone(),
            prompts: effective_prompts(Some(&prompts)),
            busy: busy.clone(),
            db,
        }) as std::sync::Arc<dyn LlmExtract>
    });
    LlmRuntime {
        extractor,
        busy,
        status: LlmStatus {
            enabled: eff.enabled,
            model: eff.enabled.then(|| eff.model.clone()),
            busy: 0,
        },
        settings: LlmSettings {
            enabled: eff.enabled,
            base_url: eff.base_url,
            model: eff.model,
            api_key_set: eff.override_key_set,
            from_override: eff.from_override,
        },
        prompts,
    }
}

pub async fn load_override(db: &crate::db::Db) -> Option<LlmSettingsUpdate> {
    let raw = db.get_setting(LLM_SETTINGS_KEY).await.ok()??;
    serde_json::from_str(&raw)
        .map_err(|e| tracing::warn!(error = %e, "ignoring corrupt llm settings override"))
        .ok()
}

pub async fn load_prompts(db: &crate::db::Db) -> Option<LlmPrompts> {
    let raw = db.get_setting(PROMPTS_SETTINGS_KEY).await.ok()??;
    serde_json::from_str(&raw)
        .map_err(|e| tracing::warn!(error = %e, "ignoring corrupt prompt override"))
        .ok()
}

// ---- system prompt: default here, user-overridable via settings ----

pub const EXTRACT_PROMPT: &str = "You extract structured facts from a French real-estate SALE listing for \
     a price tracker. Fill ONLY what the text explicitly states — never \
     guess, never infer from what is typical; absent from the text means \
     null (empty list for notes).\n\
     - annee_construction: the build year when stated.\n\
     - travaux: \"a-prevoir\" (works needed), \"rafraichissement\" (light \
       refresh), \"aucun\" ONLY if the text says none are needed / recently \
       renovated.\n\
     - chauffage_type (individuel/collectif/pompe à chaleur/poêle…), \
       chauffage_energie (gaz/electrique/fioul/bois…).\n\
     - charges_copro_month_eur: MONTHLY copropriété charges in euros \
       (convert if given per year/quarter). taxe_fonciere_year_eur: YEARLY \
       property tax in euros.\n\
     - etage: the apartment's floor (0 = rez-de-chaussée).\n\
     - orientation: main exposure when stated.\n\
     - notes: short French phrases for notable facts the other fields don't \
       cover (servitude, locataire en place, viager occupé, DPE vierge, \
       zone inondable, travaux de copropriété votés…).\n\
     Answer only with the JSON object.";

pub fn default_prompts() -> LlmPrompts {
    LlmPrompts {
        extract: EXTRACT_PROMPT.into(),
    }
}

/// Stored override merged over the defaults (empty field = default).
pub fn effective_prompts(stored: Option<&LlmPrompts>) -> LlmPrompts {
    let defaults = default_prompts();
    let Some(stored) = stored else {
        return defaults;
    };
    LlmPrompts {
        extract: if stored.extract.trim().is_empty() {
            defaults.extract
        } else {
            stored.extract.trim().to_string()
        },
    }
}

/// The JSON schema the model must answer with (strict structured output).
/// Money is asked in EUROS — models mangle cents; conversion happens here.
fn response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "annee_construction": { "type": ["integer", "null"] },
            "travaux": { "type": ["string", "null"],
                "enum": ["a-prevoir", "rafraichissement", "aucun", null] },
            "chauffage_type": { "type": ["string", "null"] },
            "chauffage_energie": { "type": ["string", "null"] },
            "fibre": { "type": ["boolean", "null"] },
            "charges_copro_month_eur": { "type": ["number", "null"] },
            "taxe_fonciere_year_eur": { "type": ["number", "null"] },
            "etage": { "type": ["integer", "null"] },
            "ascenseur": { "type": ["boolean", "null"] },
            "jardin": { "type": ["boolean", "null"] },
            "garage_parking": { "type": ["boolean", "null"] },
            "piscine": { "type": ["boolean", "null"] },
            "orientation": { "type": ["string", "null"] },
            "mitoyenne": { "type": ["boolean", "null"] },
            "notes": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["annee_construction", "travaux", "chauffage_type",
            "chauffage_energie", "fibre", "charges_copro_month_eur",
            "taxe_fonciere_year_eur", "etage", "ascenseur", "jardin",
            "garage_parking", "piscine", "orientation", "mitoyenne", "notes"],
        "additionalProperties": false
    })
}

/// The model's answer shape (euros); converted into the domain type.
#[derive(Debug, Deserialize)]
struct RawExtraction {
    annee_construction: Option<i64>,
    travaux: Option<String>,
    chauffage_type: Option<String>,
    chauffage_energie: Option<String>,
    fibre: Option<bool>,
    charges_copro_month_eur: Option<f64>,
    taxe_fonciere_year_eur: Option<f64>,
    etage: Option<i64>,
    ascenseur: Option<bool>,
    jardin: Option<bool>,
    garage_parking: Option<bool>,
    piscine: Option<bool>,
    orientation: Option<String>,
    mitoyenne: Option<bool>,
    #[serde(default)]
    notes: Vec<String>,
}

impl From<RawExtraction> for ExtractedAttrs {
    fn from(r: RawExtraction) -> Self {
        let cents = |eur: Option<f64>| eur.map(|e| (e * 100.0).round() as i64);
        ExtractedAttrs {
            annee_construction: r.annee_construction,
            travaux: r.travaux,
            chauffage_type: r.chauffage_type,
            chauffage_energie: r.chauffage_energie,
            fibre: r.fibre,
            charges_copro_month_cents: cents(r.charges_copro_month_eur),
            taxe_fonciere_year_cents: cents(r.taxe_fonciere_year_eur),
            etage: r.etage,
            ascenseur: r.ascenseur,
            jardin: r.jardin,
            garage_parking: r.garage_parking,
            piscine: r.piscine,
            orientation: r.orientation,
            mitoyenne: r.mitoyenne,
            notes: r.notes,
        }
    }
}

pub(crate) fn request_body(
    model: &str,
    input: &ExtractInput<'_>,
    system: &str,
) -> serde_json::Value {
    let listing = serde_json::json!({
        "title": input.title,
        "price": format!("{:.0} EUR", input.price_cents as f64 / 100.0),
        "property_type": input.property_type,
        "surface_m2": input.surface_m2,
        "rooms": input.rooms,
        "description": input.description,
    });
    serde_json::json!({
        "model": model,
        "temperature": 0,
        // explicit budget with room for chain-of-thought: reasoning models
        // think first and the thoughts count against max_tokens; ollama-style
        // backends would otherwise cap at ~128 and truncate the JSON
        "max_tokens": 4000,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user", "content": listing.to_string() }
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": { "name": "extraction", "strict": true, "schema": response_schema() }
        }
    })
}

/// The assistant text of a chat-completions response body.
pub(crate) fn content_of(body: &str) -> anyhow::Result<String> {
    let v: serde_json::Value = serde_json::from_str(body)?;
    let choice = &v["choices"][0];
    if let Some(content) = choice["message"]["content"].as_str()
        && !content.trim().is_empty()
    {
        return Ok(content.to_string());
    }
    // llama.cpp reasoning models put thoughts in reasoning_content; when
    // the token budget runs out mid-think, content stays empty
    if let Some(reasoning) = choice["message"]["reasoning_content"].as_str()
        && !reasoning.trim().is_empty()
    {
        let finish = choice["finish_reason"].as_str().unwrap_or("?");
        if finish == "stop" && reasoning.contains('{') {
            return Ok(reasoning.to_string());
        }
        anyhow::bail!(
            "the model spent its whole token budget reasoning without answering \
             (finish_reason={finish}) — thinking should be disabled for this call"
        );
    }
    anyhow::bail!("no choices[0].message.content in llm response")
}

/// Models love wrapping JSON in ```fences``` or prose despite instructions —
/// cut the answer down to its outermost object before parsing.
pub(crate) fn extract_json(content: &str) -> &str {
    let content = match content.rfind("</think>") {
        Some(i) => &content[i + "</think>".len()..],
        None => content,
    };
    match (content.find('{'), content.rfind('}')) {
        (Some(start), Some(end)) if end > start => &content[start..=end],
        _ => content,
    }
}

impl OpenAiExtractor {
    /// Returns the assistant content plus token usage when reported.
    async fn post_chat(
        &self,
        body: &serde_json::Value,
        usage: &mut Option<(i64, i64)>,
    ) -> anyhow::Result<String> {
        let mut request = self.http.post(&self.url).json(body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "{status}: {}",
                text.chars().take(300).collect::<String>().trim()
            );
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            && let (Some(p), Some(c)) = (
                v["usage"]["prompt_tokens"].as_i64(),
                v["usage"]["completion_tokens"].as_i64(),
            )
        {
            *usage = Some((p, c));
        }
        content_of(&text)
    }

    /// One structured chat call, resilient to backends that reject OR
    /// silently mangle `response_format`: any failure on the structured
    /// attempt gets one plain retry — the prompt already demands a bare
    /// JSON object. Every call is timed and logged.
    async fn chat_json<T: serde::de::DeserializeOwned>(
        &self,
        kind: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<T> {
        use std::sync::atomic::Ordering;
        self.busy.fetch_add(1, Ordering::SeqCst);
        let start = std::time::Instant::now();
        let mut usage = None;
        let result = self.chat_json_inner(body, &mut usage).await;
        self.busy.fetch_sub(1, Ordering::SeqCst);
        if let Some(db) = &self.db {
            let entry = LlmLogEntry {
                kind: kind.to_string(),
                model: self.model.clone(),
                duration_ms: start.elapsed().as_millis() as i64,
                ok: result.is_ok(),
                error: result.as_ref().err().map(|e| e.to_string()),
                prompt_tokens: usage.map(|(p, _)| p),
                completion_tokens: usage.map(|(_, c)| c),
            };
            let db = db.clone();
            // fire-and-forget: the log must never slow or fail the call
            tokio::spawn(async move {
                if let Err(e) = db.log_llm_request(&entry).await {
                    tracing::debug!(error = %e, "llm request log failed");
                }
            });
        }
        result
    }

    async fn chat_json_inner<T: serde::de::DeserializeOwned>(
        &self,
        mut body: serde_json::Value,
        usage: &mut Option<(i64, i64)>,
    ) -> anyhow::Result<T> {
        fn parse<T: serde::de::DeserializeOwned>(content: &str) -> anyhow::Result<T> {
            Ok(serde_json::from_str(extract_json(content))?)
        }
        let first = match self.post_chat(&body, usage).await {
            Ok(content) => match parse(&content) {
                Ok(v) => return Ok(v),
                Err(e) => anyhow::anyhow!(
                    "{e} (content: {})",
                    content.chars().take(120).collect::<String>()
                ),
            },
            Err(e) => e,
        };
        if body.get("response_format").is_none() {
            return Err(first);
        }
        tracing::debug!(error = %first, "structured attempt failed — retrying plain");
        body.as_object_mut()
            .expect("chat body is an object")
            .remove("response_format");
        let content = self
            .post_chat(&body, usage)
            .await
            .map_err(|e| anyhow::anyhow!("{first}; plain retry: {e}"))?;
        parse(&content).map_err(|e| anyhow::anyhow!("{first}; plain retry: {e}"))
    }
}

#[async_trait::async_trait]
impl LlmExtract for OpenAiExtractor {
    async fn extract(&self, input: &ExtractInput<'_>) -> anyhow::Result<ExtractedAttrs> {
        let raw: RawExtraction = self
            .chat_json(
                "extract",
                request_body(&self.model, input, &self.prompts.extract),
            )
            .await?;
        Ok(raw.into())
    }
}

// ---- endpoint discovery & probing (settings UI helpers) ----

fn probe_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .user_agent(concat!("terrier/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("building llm probe client")
}

/// `GET {base_url}/models` — the standard OpenAI-compatible catalog.
pub async fn list_models(base_url: &str, api_key: Option<&str>) -> anyhow::Result<Vec<String>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = probe_client(10).get(&url);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "{status}: {}",
            text.chars().take(300).collect::<String>().trim()
        );
    }
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let mut models: Vec<String> = v["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("no data[] in {url} response"))?
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect();
    models.sort();
    Ok(models)
}

/// One tiny real completion against the endpoint — the settings panel's
/// "Test" button. Errors carry the backend's message verbatim.
pub async fn probe(base_url: &str, model: &str, api_key: Option<&str>) -> anyhow::Result<()> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 2048,
        "messages": [{ "role": "user", "content": "Reply with the single word: ok" }],
    });
    // a reasoning model may think before its one-word answer
    let mut request = probe_client(90).post(&url).json(&body);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "{status}: {}",
            text.chars().take(300).collect::<String>().trim()
        );
    }
    content_of(&text).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ExtractInput<'static> {
        ExtractInput {
            title: "Maison 5 pièces Bruz",
            price_cents: 32_000_000,
            property_type: "maison",
            surface_m2: Some(110.0),
            rooms: Some(5),
            description: "Maison familiale, charges 45 € par mois, taxe foncière 1200 €. \
                          Chauffage gaz. Fibre. Travaux de rafraîchissement à prévoir.",
        }
    }

    fn parse_response(body: &str) -> anyhow::Result<ExtractedAttrs> {
        let content = content_of(body)?;
        let raw: RawExtraction = serde_json::from_str(extract_json(&content))?;
        Ok(raw.into())
    }

    #[test]
    fn parses_extraction_and_converts_euros_to_cents() {
        let body = r#"{"choices": [{ "message": { "role": "assistant", "content":
            "{\"annee_construction\": 1998, \"travaux\": \"rafraichissement\", \"chauffage_type\": \"individuel\", \"chauffage_energie\": \"gaz\", \"fibre\": true, \"charges_copro_month_eur\": 45, \"taxe_fonciere_year_eur\": 1200.5, \"etage\": null, \"ascenseur\": null, \"jardin\": true, \"garage_parking\": null, \"piscine\": null, \"orientation\": null, \"mitoyenne\": null, \"notes\": [\"locataire en place\"]}"
        }}]}"#;
        let a = parse_response(body).unwrap();
        assert_eq!(a.annee_construction, Some(1998));
        assert_eq!(a.charges_copro_month_cents, Some(4500));
        assert_eq!(a.taxe_fonciere_year_cents, Some(120_050));
        assert_eq!(a.travaux.as_deref(), Some("rafraichissement"));
        assert_eq!(a.notes, vec!["locataire en place"]);
    }

    #[test]
    fn extract_json_strips_fences_prose_and_think_blocks() {
        assert_eq!(extract_json("{\"a\": 1}"), "{\"a\": 1}");
        assert_eq!(extract_json("```json\n{\"a\": 1}\n```"), "{\"a\": 1}");
        assert_eq!(
            extract_json("Sure! Here: {\"a\": {\"b\": 2}} hope it helps"),
            "{\"a\": {\"b\": 2}}"
        );
        assert_eq!(
            extract_json("<think>Hmm {tricky}</think>\n{\"a\": 1}"),
            "{\"a\": 1}"
        );
        assert_eq!(extract_json("no json at all"), "no json at all");
    }

    #[test]
    fn empty_content_with_reasoning_is_a_clear_error() {
        let body = r#"{"choices": [{"finish_reason": "length", "message":
            {"role": "assistant", "content": "", "reasoning_content": "Let me think..."}}]}"#;
        let err = content_of(body).unwrap_err().to_string();
        assert!(err.contains("token budget reasoning"), "got: {err}");

        let body = r#"{"choices": [{"finish_reason": "stop", "message":
            {"role": "assistant", "content": "", "reasoning_content": "here: {\"a\": 1}"}}]}"#;
        assert!(content_of(body).unwrap().contains("{\"a\": 1}"));
    }

    #[test]
    fn request_carries_listing_strict_schema_and_token_room() {
        let body = request_body("qwen3", &input(), EXTRACT_PROMPT);
        assert_eq!(body["model"], "qwen3");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert!(body["max_tokens"].as_u64().unwrap() >= 4000, "room for CoT");
        let user = body["messages"][1]["content"].as_str().unwrap();
        assert!(user.contains("Maison 5 pièces Bruz"));
        assert!(user.contains("taxe foncière 1200"));
        assert!(user.contains("320000 EUR"));
    }

    #[test]
    fn rejects_malformed_responses() {
        assert!(parse_response("not json").is_err());
        assert!(parse_response(r#"{"choices": []}"#).is_err());
        let bad = r#"{"choices": [{"message": {"content": "{\"travaux\": 42}"}}]}"#;
        assert!(parse_response(bad).is_err(), "wrong field type rejected");
    }

    #[test]
    fn disabled_config_builds_no_extractor() {
        let eff = effective(&LlmConfig::default(), None).unwrap();
        let runtime = build_runtime(eff, default_prompts(), None);
        assert!(runtime.extractor.is_none());
        assert!(!runtime.status.enabled && runtime.status.model.is_none());
    }

    #[test]
    fn override_supersedes_config_blank_fields_fall_back() {
        let base = LlmConfig {
            model: "conf-model".into(),
            ..Default::default()
        };
        let o = LlmSettingsUpdate {
            enabled: true,
            base_url: "http://zeus:8080/v1".into(),
            model: String::new(),
            api_key: Some("sk-x".into()),
        };
        let eff = effective(&base, Some(&o)).unwrap();
        assert!(eff.enabled, "override enables a config-disabled pass");
        assert_eq!(eff.base_url, "http://zeus:8080/v1");
        assert_eq!(
            eff.model, "conf-model",
            "blank override field falls back to TOML"
        );
        assert_eq!(eff.api_key.as_deref(), Some("sk-x"));

        let runtime = build_runtime(eff, default_prompts(), None);
        assert!(runtime.extractor.is_some());
        assert_eq!(runtime.status.model.as_deref(), Some("conf-model"));
        assert!(runtime.settings.api_key_set && runtime.settings.from_override);
    }

    #[test]
    fn prompt_override_merges_over_default() {
        let stored = LlmPrompts {
            extract: "  ".into(),
        };
        assert_eq!(effective_prompts(Some(&stored)).extract, EXTRACT_PROMPT);
        let stored = LlmPrompts {
            extract: "custom".into(),
        };
        assert_eq!(effective_prompts(Some(&stored)).extract, "custom");
    }
}
