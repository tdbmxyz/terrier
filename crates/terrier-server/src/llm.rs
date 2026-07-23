//! LLM extraction (implementation lands with the llm task).

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
