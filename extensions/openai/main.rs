//! `openai`: models behind an OpenAI-compatible Chat Completions API (OpenAI itself,
//! OpenRouter, Ollama, vLLM, ...). Provider `openai` uses `base_url`; well-known services
//! (`PRESETS`: Groq, DeepSeek, Mistral, xAI, Gemini, ...) are providers of their own id out of
//! the box; every entry of `endpoints` is one more provider, or overrides a preset's fields:
//!
//! ```json
//! {"endpoints": {"openrouter": {"label": "OpenRouter", "base_url": "https://openrouter.ai/api/v1",
//!   "key_env": "OPENROUTER_API_KEY", "model": "anthropic/claude-sonnet-4.5", "context_window": 200000}}}
//! ```
//!
//! Each provider is an account signed in to with an API key (`/login <id>`); the variable
//! (`OPENAI_API_KEY`, an endpoint's `key_env`) stands in for it, and a local server needs
//! none. Endpoints are read at start; `/reload` picks up changes.

use anyhow::{Result, anyhow};
use august_ext::August;
use august_ext::llm::{LlmProvider, ModelInfo};
use august_llm::openai::OpenAi;
use serde_json::{Value, json};

const DEFAULT_URL: &str = "https://api.openai.com/v1";

/// Well-known OpenAI-compatible services: (id, label, base_url, key_env).
const PRESETS: &[(&str, &str, &str, &str)] = &[
    ("openrouter", "OpenRouter", "https://openrouter.ai/api/v1", "OPENROUTER_API_KEY"),
    ("groq", "Groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
    ("deepseek", "DeepSeek", "https://api.deepseek.com", "DEEPSEEK_API_KEY"),
    ("mistral", "Mistral", "https://api.mistral.ai/v1", "MISTRAL_API_KEY"),
    ("xai", "xAI (Grok)", "https://api.x.ai/v1", "XAI_API_KEY"),
    ("gemini", "Google Gemini", "https://generativelanguage.googleapis.com/v1beta/openai", "GEMINI_API_KEY"),
    ("together", "Together AI", "https://api.together.xyz/v1", "TOGETHER_API_KEY"),
    ("fireworks", "Fireworks AI", "https://api.fireworks.ai/inference/v1", "FIREWORKS_API_KEY"),
    ("cerebras", "Cerebras", "https://api.cerebras.ai/v1", "CEREBRAS_API_KEY"),
    ("deepinfra", "DeepInfra", "https://api.deepinfra.com/v1/openai", "DEEPINFRA_API_KEY"),
    ("moonshot", "Moonshot AI (Kimi)", "https://api.moonshot.ai/v1", "MOONSHOT_API_KEY"),
    ("zai", "Z.ai (GLM)", "https://api.z.ai/api/paas/v4", "ZAI_API_KEY"),
    ("qwen", "Alibaba Qwen (DashScope)", "https://dashscope-intl.aliyuncs.com/compatible-mode/v1", "DASHSCOPE_API_KEY"),
    ("minimax", "MiniMax", "https://api.minimax.io/v1", "MINIMAX_API_KEY"),
    ("nebius", "Nebius AI Studio", "https://api.studio.nebius.com/v1", "NEBIUS_API_KEY"),
    ("huggingface", "Hugging Face", "https://router.huggingface.co/v1", "HF_TOKEN"),
    ("sambanova", "SambaNova", "https://api.sambanova.ai/v1", "SAMBANOVA_API_KEY"),
    ("novita", "Novita AI", "https://api.novita.ai/v3/openai", "NOVITA_API_KEY"),
    ("venice", "Venice AI", "https://api.venice.ai/api/v1", "VENICE_API_KEY"),
    ("hyperbolic", "Hyperbolic", "https://api.hyperbolic.xyz/v1", "HYPERBOLIC_API_KEY"),
    ("chutes", "Chutes", "https://llm.chutes.ai/v1", "CHUTES_API_KEY"),
    ("cohere", "Cohere", "https://api.cohere.ai/compatibility/v1", "COHERE_API_KEY"),
];

/// One provider's connection, resolved at the time of a call.
struct Endpoint {
    base_url: String,
    key: String,
    window: Option<usize>,
}

fn text(v: &Value, k: &str) -> Option<String> {
    v[k].as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// Provider `id`'s entry: its preset, with the fields of its `endpoints` entry over it.
fn entry(settings: &Value, id: &str) -> Value {
    let mut e = json!({});
    if let Some((_, label, base_url, key_env)) = PRESETS.iter().find(|p| p.0 == id) {
        e = json!({"label": label, "base_url": base_url, "key_env": key_env});
    }
    for (k, v) in settings["endpoints"][id].as_object().into_iter().flatten() {
        e[k] = v.clone();
    }
    e
}

/// The variable that stands in for provider `id`'s key.
fn key_env(settings: &Value, id: &str) -> Option<String> {
    if id == "openai" { Some("OPENAI_API_KEY".into()) } else { text(&entry(settings, id), "key_env") }
}

/// Where provider `id` is served and with which key: its variable, else the signed-in key,
/// else one written into the settings by hand (older homes).
async fn endpoint(august: &August, id: &str) -> Result<Endpoint> {
    let settings = august.settings().await?;
    let key = match key_env(&settings, id).and_then(|k| august.env(&k)) {
        Some(k) => Some(k),
        None => august.secret(id).await?,
    };
    if id == "openai" {
        return Ok(Endpoint {
            base_url: august.env("OPENAI_BASE_URL").or_else(|| text(&settings, "base_url")).unwrap_or_else(|| DEFAULT_URL.into()),
            key: key.or_else(|| text(&settings, "key")).unwrap_or_default(),
            window: None,
        });
    }
    let e = &entry(&settings, id);
    Ok(Endpoint {
        base_url: text(e, "base_url").ok_or_else(|| anyhow!("no endpoint `{id}` with a base_url in the openai extension's settings"))?,
        key: key.or_else(|| text(e, "key")).unwrap_or_default(),
        window: e["context_window"].as_u64().map(|n| n as usize),
    })
}

async fn list(e: &Endpoint, default: Option<&str>) -> Result<Vec<ModelInfo>> {
    let mut req = august_llm::http_client().get(format!("{}/models", e.base_url.trim_end_matches('/')));
    if !e.key.is_empty() {
        req = req.bearer_auth(&e.key);
    }
    let body: Value = req.send().await?.error_for_status()?.json().await?;
    // Most answer `{"data": [...]}`, some (Together) a bare array.
    let mut models: Vec<ModelInfo> = body["data"]
        .as_array()
        .or(body.as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| Some(ModelInfo { id: m["id"].as_str()?.to_string(), context_window: e.window }))
        .collect();
    if models.is_empty() && let Some(m) = default {
        models.push(ModelInfo { id: m.into(), context_window: e.window });
    }
    Ok(models)
}

fn provide(august: &August, settings: &Value, id: &str, label: &str, default_model: Option<&str>) {
    let (for_models, for_complete, for_check) = (august.clone(), august.clone(), august.clone());
    let default = default_model.map(String::from);
    august.register_provider(
        id,
        label,
        default_model,
        move |id| {
            let (august, default) = (for_models.clone(), default.clone());
            async move { list(&endpoint(&august, &id).await?, default.as_deref()).await }
        },
        move |req, stream| {
            let august = for_complete.clone();
            async move {
                let e = endpoint(&august, &req.provider).await?;
                let model = OpenAi::new(e.base_url, e.key, req.model).with_context_window(e.window).with_options(req.options);
                let mut on_text = |t: &str| stream.text(t);
                model.complete_stream(&req.session, &req.system, &req.messages, &req.tools, &mut on_text).await
            }
        },
    );
    // A key is checked by listing the models with it.
    august.register_key_account(id, label, &[id], "API key", key_env(settings, id).as_deref(), move |id, key| {
        let august = for_check.clone();
        async move {
            let mut e = endpoint(&august, &id).await?;
            e.key = key;
            list(&e, None).await.map(drop)
        }
    });
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
    august.describe(
        "Models behind OpenAI-compatible APIs (OpenAI, OpenRouter, Groq, Gemini, Ollama, ...)",
        "Provider `openai` (key: /login openai or OPENAI_API_KEY), one per well-known service (/login groq, GROQ_API_KEY, ...) and one per entry of `endpoints`.",
    );
    august.settings_schema(json!({
        "type": "object",
        "properties": {
            "base_url": {"type": "string", "default": DEFAULT_URL, "description": "Address of the OpenAI-compatible API"},
            "endpoints": {"type": "object", "description": "More providers by id, or overrides of a built-in one's fields: {label, base_url, key_env, model, context_window}"},
        },
    }));
    provide(&august, &Value::Null, "openai", "OpenAI-compatible (OpenAI, OpenRouter, Ollama, ...)", None);
    for (id, label, ..) in PRESETS {
        provide(&august, &Value::Null, id, label, None);
    }
    // Settings can only be read once the link runs.
    let me = august.clone();
    tokio::spawn(async move {
        let Ok(settings) = me.settings().await else { return };
        for id in settings["endpoints"].as_object().into_iter().flatten().map(|(id, _)| id) {
            let e = entry(&settings, id);
            provide(&me, &settings, id, e["label"].as_str().unwrap_or(id), e["model"].as_str());
        }
    });
    august.run().await;
}

#[cfg(test)]
mod tests;
