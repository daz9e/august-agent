//! Voice notes and audio files are transcribed for the agent via an OpenAI-compatible
//! `POST {url}/audio/transcriptions`. Settings `url` (default OpenAI; e.g. Groq), `key` and
//! `model` (default whisper-1); `AUGUST_TRANSCRIBE_URL`, `AUGUST_TRANSCRIBE_API_KEY` and
//! `AUGUST_TRANSCRIBE_MODEL` override them. Against OpenAI itself, `OPENAI_API_KEY` stands
//! in for a missing key.

use anyhow::{Context, Result, bail};
use august_ext::August;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

const OPENAI_URL: &str = "https://api.openai.com/v1";
const TIMEOUT: Duration = Duration::from_secs(100);

async fn transcribe(august: &August, http: &reqwest::Client, path: &Path, mime: &str) -> Result<String> {
    let s = august.settings().await?;
    let pick = |var: &str, key: &str| august.env(var).or_else(|| s[key].as_str().filter(|v| !v.is_empty()).map(String::from));
    let base = pick("AUGUST_TRANSCRIBE_URL", "url").unwrap_or_else(|| OPENAI_URL.into());
    let mut key = pick("AUGUST_TRANSCRIBE_API_KEY", "key").unwrap_or_default();
    if base.trim_end_matches('/') == OPENAI_URL {
        key = Some(key).filter(|k| !k.is_empty()).or_else(|| august.env("OPENAI_API_KEY")).context(
            "transcription is not configured: set the voice extension's `key` (an OpenAI key), or `url` \
             and `key` for another OpenAI-compatible service such as Groq",
        )?;
    }
    let bytes = tokio::fs::read(path).await.with_context(|| format!("could not read {}", path.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "audio".into());
    let part = reqwest::multipart::Part::bytes(bytes).file_name(name).mime_str(mime)?;
    let form = reqwest::multipart::Form::new()
        .text("model", pick("AUGUST_TRANSCRIBE_MODEL", "model").unwrap_or_else(|| "whisper-1".into()))
        .part("file", part);
    let mut req = http.post(format!("{}/audio/transcriptions", base.trim_end_matches('/'))).multipart(form);
    if !key.is_empty() {
        req = req.bearer_auth(key);
    }
    let res = req.send().await.context("transcription request failed")?;
    let status = res.status();
    let body = res.text().await?;
    if !status.is_success() {
        bail!("transcription failed: HTTP {status}: {}", body.chars().take(300).collect::<String>());
    }
    let json: Value = serde_json::from_str(&body).context("transcription: bad response")?;
    match json["text"].as_str().map(str::trim) {
        Some(text) if !text.is_empty() => Ok(text.to_string()),
        _ => bail!("transcription returned no text"),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await
}

async fn serve(august: August) {
    august.describe(
        "Transcribes voice notes and audio files for the agent",
        "Audio saved by `attachments` is sent to an OpenAI-compatible transcription endpoint \
         (settings `url`, `key`, `model`); the transcript joins the message text.",
    );
    august.settings_schema(json!({
        "type": "object",
        "properties": {
            "url": {"type": "string", "default": OPENAI_URL, "description": "OpenAI-compatible API with /audio/transcriptions, e.g. https://api.groq.com/openai/v1"},
            "key": {"type": "string", "secret": true, "description": "API key for `url` (OpenAI: OPENAI_API_KEY stands in)"},
            "model": {"type": "string", "default": "whisper-1", "description": "Transcription model, e.g. whisper-large-v3-turbo on Groq"},
        },
    }));
    let http = reqwest::Client::builder().timeout(TIMEOUT).build().expect("http client");
    let workspace = august.workspace().clone();
    let me = august.clone();
    august.on("message_in", move |data, _| {
        let (august, http, workspace) = (me.clone(), http.clone(), workspace.clone());
        async move {
            let audio: Vec<&Value> = data["files"].as_array().into_iter().flatten().filter(|f| f["mime"].as_str().is_some_and(|m| m.starts_with("audio/"))).collect();
            if audio.is_empty() {
                return Ok(None);
            }
            // All at once: the hook has two minutes for the whole message.
            let transcripts = audio.iter().map(|f| {
                let path = Path::new(f["path"].as_str().unwrap_or_default());
                let name = path.strip_prefix(&workspace).unwrap_or(path).display();
                let kind = if f["voice"] == true { "Voice message" } else { "Audio" };
                let (august, http) = (&august, &http);
                async move {
                    match transcribe(august, http, path, f["mime"].as_str().unwrap_or_default()).await {
                        Ok(text) => format!("[{kind} transcript, {name}]\n{text}"),
                        Err(e) => format!("[No transcript of {name}: {e:#}]"),
                    }
                }
            });
            let mut parts: Vec<String> = data["text"].as_str().filter(|t| !t.is_empty()).map(String::from).into_iter().collect();
            parts.extend(futures_util::future::join_all(transcripts).await);
            Ok(Some(json!({"text": parts.join("\n")})))
        }
    });
    august.run().await;
}

#[cfg(test)]
mod tests;
