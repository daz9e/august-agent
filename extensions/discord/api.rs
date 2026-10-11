//! Minimal Discord REST API client (only what August needs).

use anyhow::{Context, Result, bail};
use reqwest::Method;
use serde_json::Value;
use std::time::Duration;

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    /// `{host}/api/v10`, without a trailing slash.
    base: String,
    token: String,
}

#[derive(Debug)]
pub struct ApiError(pub String);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "discord: {}", self.0)
    }
}
impl std::error::Error for ApiError {}

/// Uploads and downloads may be large; other calls use the client's shorter timeout.
const FILE_TIMEOUT: Duration = Duration::from_secs(300);

impl Api {
    /// `base`: another API root (or a test double), else Discord's.
    pub fn new(base: Option<&str>, token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .user_agent(format!("DiscordBot (https://github.com/daz9e/august-agent, {})", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(60))
                .build()
                .expect("http client"),
            base: base.unwrap_or("https://discord.com/api/v10").trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Calls `method path` with an optional JSON body.
    pub async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        self.request(path, || {
            let req = self.http.request(method.clone(), &url);
            match &body {
                Some(b) => req.json(b),
                None => req,
            }
        })
        .await
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        self.call(Method::GET, path, None).await
    }

    /// Calls `method path` with `payload` and files (`files[n]`), as multipart.
    pub async fn upload(&self, method: Method, path: &str, payload: &Value, files: &[(String, Vec<u8>)]) -> Result<Value> {
        let url = format!("{}{path}", self.base);
        self.request(path, || {
            let mut form = reqwest::multipart::Form::new().text("payload_json", payload.to_string());
            for (i, (name, bytes)) in files.iter().enumerate() {
                form = form.part(format!("files[{i}]"), reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.clone()));
            }
            self.http.request(method.clone(), &url).timeout(FILE_TIMEOUT).multipart(form)
        })
        .await
    }

    /// Downloads an attachment from its (CDN) url; no token goes there.
    pub async fn download(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(url)
            .timeout(FILE_TIMEOUT)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .context("discord: file download failed")?;
        Ok(resp.bytes().await?.to_vec())
    }

    /// Sends a request built by `build`, authorized as the bot; waits and retries when rate
    /// limited (429).
    async fn request(&self, what: &str, build: impl Fn() -> reqwest::RequestBuilder) -> Result<Value> {
        for attempt in 0..4 {
            let resp = build()
                .header("Authorization", format!("Bot {}", self.token))
                .send()
                .await
                .with_context(|| format!("discord {what}: request failed"))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if status.is_success() {
                return Ok(body);
            }
            if status.as_u16() == 429 && attempt < 3 {
                let secs = body["retry_after"].as_f64().unwrap_or(1.0).min(30.0);
                tokio::time::sleep(Duration::from_secs_f64(secs + 0.1)).await;
                continue;
            }
            let message = body["message"].as_str().map_or(text.as_str(), |m| m);
            return Err(ApiError(format!("{what}: HTTP {status}: {message}")).into());
        }
        bail!("discord {what}: gave up after rate-limit retries")
    }
}

/// `s` as one URL path segment (an emoji in a reaction's path).
pub fn segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
