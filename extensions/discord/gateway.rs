//! The Discord gateway: a WebSocket on which Discord pushes what happens (messages,
//! reactions, button presses, ...). Heartbeats keep it alive; a dropped connection resumes
//! the session, so what was sent meanwhile is replayed, not lost.

use crate::api::Api;
use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

/// GUILDS, GUILD_MESSAGES, GUILD_MESSAGE_REACTIONS, DIRECT_MESSAGES,
/// DIRECT_MESSAGE_REACTIONS, MESSAGE_CONTENT.
const INTENTS: u64 = 1 | 1 << 9 | 1 << 10 | 1 << 12 | 1 << 13 | 1 << 15;

/// A session to resume: its id, where to resume it and the last event seen.
struct Session {
    id: String,
    url: String,
    seq: Option<u64>,
}

/// How a connection ended.
enum End {
    /// Connect again, resuming the session if `true`.
    Again(bool),
    /// Discord won't take the bot as it is: connecting again can't help.
    Fatal(String),
}

/// Runs the gateway: each event (`t`, `d`) goes to `events` as it comes, in order; `seen`
/// hears of each answer from Discord (`None`) or failure (`Some(error)`). Returns only when
/// Discord refuses the bot for good, with why.
pub async fn run(api: &Api, events: impl Fn(String, Value), seen: impl Fn(Option<String>)) -> String {
    let mut session: Option<Session> = None;
    loop {
        let url = match &session {
            Some(s) => Ok(s.url.clone()),
            None => api.get("/gateway/bot").await.and_then(|g| g["url"].as_str().map(str::to_string).context("no gateway url")),
        };
        let end = match url {
            Ok(url) => connect(api, &url, &mut session, &events, &seen).await,
            Err(e) => Err(e),
        };
        match end {
            Ok(End::Fatal(why)) => {
                seen(Some(why.clone()));
                return why;
            }
            Ok(End::Again(resume)) => {
                if !resume {
                    session = None;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                seen(Some(format!("{e:#}")));
                eprintln!("gateway: {e:#}; reconnecting in 3s");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// One connection, from Hello to its end.
async fn connect(
    api: &Api,
    url: &str,
    session: &mut Option<Session>,
    events: &impl Fn(String, Value),
    seen: &impl Fn(Option<String>),
) -> Result<End> {
    let (ws, _) = tokio_tungstenite::connect_async(format!("{url}/?v=10&encoding=json")).await.context("gateway connect")?;
    let (mut tx, mut rx) = ws.split();
    let hello = tokio::time::timeout(Duration::from_secs(30), rx.next()).await.context("gateway: no Hello")?;
    let hello: Value = match hello {
        Some(Ok(Message::Text(t))) => serde_json::from_str(&t)?,
        other => return Err(anyhow!("gateway: expected Hello, got {other:?}")),
    };
    let interval = hello["d"]["heartbeat_interval"].as_u64().context("gateway: Hello without heartbeat_interval")?;
    seen(None);

    let start = match session {
        Some(s) => json!({"op": 6, "d": {"token": api.token(), "session_id": s.id, "seq": s.seq}}),
        None => json!({"op": 2, "d": {
            "token": api.token(),
            "intents": INTENTS,
            "properties": {"os": std::env::consts::OS, "browser": "august", "device": "august"},
        }}),
    };
    tx.send(Message::Text(start.to_string().into())).await?;

    let mut beat = tokio::time::interval(Duration::from_millis(interval));
    beat.tick().await;
    let mut acked = true;
    loop {
        let seq = session.as_ref().and_then(|s| s.seq);
        tokio::select! {
            _ = beat.tick() => {
                if !acked {
                    eprintln!("gateway: heartbeat not acknowledged, reconnecting");
                    return Ok(End::Again(true));
                }
                acked = false;
                tx.send(Message::Text(json!({"op": 1, "d": seq}).to_string().into())).await?;
            }
            msg = rx.next() => match msg {
                None => return Ok(End::Again(true)),
                Some(Err(e)) => return Err(e).context("gateway"),
                Some(Ok(Message::Close(frame))) => return Ok(closed(frame.map(|f| u16::from(f.code)))),
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or_default();
                    seen(None);
                    match v["op"].as_u64() {
                        Some(0) => {
                            let (t, d) = (v["t"].as_str().unwrap_or("").to_string(), v["d"].clone());
                            if t == "READY" {
                                let url = d["resume_gateway_url"].as_str().unwrap_or(url).to_string();
                                *session = Some(Session { id: d["session_id"].as_str().unwrap_or("").to_string(), url, seq: None });
                            }
                            if let (Some(s), Some(n)) = (session.as_mut(), v["s"].as_u64()) {
                                s.seq = Some(n);
                            }
                            events(t, d);
                        }
                        Some(1) => tx.send(Message::Text(json!({"op": 1, "d": seq}).to_string().into())).await?,
                        Some(7) => return Ok(End::Again(true)),
                        Some(9) => {
                            // Invalid session: wait a moment, as Discord asks, then identify anew
                            // unless it says the session can still be resumed.
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            return Ok(End::Again(v["d"] == true));
                        }
                        Some(11) => acked = true,
                        _ => {}
                    }
                }
                Some(Ok(_)) => {}
            }
        }
    }
}

/// What a close code means for connecting again.
fn closed(code: Option<u16>) -> End {
    match code {
        Some(4004) => End::Fatal("Discord rejected the bot token: /login discord".into()),
        Some(4013 | 4014) => End::Fatal(
            "Discord refused the bot's intents: turn on Message Content Intent under Bot in the Developer Portal, then restart".into(),
        ),
        Some(c @ 4010..=4012) => End::Fatal(format!("Discord closed the gateway for good (code {c})")),
        // Bad sequence, session timed out: start a new one.
        Some(4007 | 4009) => End::Again(false),
        _ => End::Again(true),
    }
}
