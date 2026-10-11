//! The Discord messenger against a fake core, a fake gateway and a fake REST API: what users
//! write comes in as `inbound` (allowed users only, servers marking what is addressed,
//! threads as places of their own), slash commands and button presses are answered and come
//! in, what August sends goes out as Markdown with components, plus files, reactions, the
//! command menu, threads, the `pin` action, downloads, resuming the gateway, a refused bot
//! and pairing the owner at login.

#[path = "../fake_http.rs"]
mod fake_http;

use super::serve;
use august_ext::FakeAugust;
use fake_http::{FakeHttp, Req, Resp};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

const TOKEN: &str = "TEST";
const OWNER: &str = "7";
const BOT: &str = "99";
/// The DM channel with anyone, a server's text channel, a thread the bot opened in it, a forum.
const DM: &str = "800";
const TEXT: &str = "500";
const THREAD: &str = "600";
const FORUM: &str = "700";

/// The fake gateway: says Hello, records what the bot sends (identify, resume, heartbeats)
/// and pushes what the test queues, to whichever connection is open.
struct Gateway {
    url: String,
    got: Arc<Mutex<Vec<Value>>>,
    out: mpsc::UnboundedSender<Message>,
    seq: Mutex<u64>,
}

impl Gateway {
    async fn start() -> Gateway {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let (out, rx) = mpsc::unbounded_channel::<Message>();
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let got: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (log, me) = (got.clone(), url.clone());
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (rx, log, me) = (rx.clone(), log.clone(), me.clone());
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                    let hello = json!({"op": 10, "d": {"heartbeat_interval": 60000}});
                    ws.send(Message::Text(hello.to_string().into())).await.unwrap();
                    let mut rx = rx.lock().await;
                    loop {
                        tokio::select! {
                            m = ws.next() => match m {
                                Some(Ok(Message::Text(t))) => {
                                    let v: Value = serde_json::from_str(&t).unwrap();
                                    if v["op"] == 2 {
                                        let ready = json!({"op": 0, "t": "READY", "s": 1, "d": {
                                            "session_id": "s1", "resume_gateway_url": me, "user": {"id": BOT}}});
                                        ws.send(Message::Text(ready.to_string().into())).await.unwrap();
                                    }
                                    log.lock().unwrap().push(v);
                                }
                                Some(Ok(_)) => {}
                                _ => break,
                            },
                            m = rx.recv() => match m {
                                Some(m) => if ws.send(m).await.is_err() { break },
                                None => break,
                            },
                        }
                    }
                });
            }
        });
        Gateway { url, got, out, seq: Mutex::new(1) }
    }

    /// Discord dispatches event `t`.
    fn dispatch(&self, t: &str, d: Value) {
        let s = {
            let mut seq = self.seq.lock().unwrap();
            *seq += 1;
            *seq
        };
        self.send(json!({"op": 0, "t": t, "s": s, "d": d}));
    }

    fn send(&self, v: Value) {
        self.out.send(Message::Text(v.to_string().into())).unwrap();
    }

    fn close(&self, code: u16) {
        self.out.send(Message::Close(Some(CloseFrame { code: code.into(), reason: "".into() }))).unwrap();
    }

    async fn wait_op(&self, op: u64, n: usize) -> Value {
        for _ in 0..1000 {
            if let Some(v) = self.got.lock().unwrap().iter().filter(|v| v["op"] == op).nth(n - 1) {
                return v.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no op {op} #{n}")
    }
}

/// The fake REST API, pointing at `gateway`.
async fn rest(gateway: &str) -> FakeHttp {
    let gateway = gateway.to_string();
    FakeHttp::start(move |req| {
        let (m, p) = (req.method.as_str(), req.path.as_str());
        let channel = |id: &str| match id {
            TEXT => json!({"id": TEXT, "type": 0, "name": "general", "guild_id": "g"}),
            THREAD => json!({"id": THREAD, "type": 11, "name": "Trip", "parent_id": TEXT, "owner_id": BOT}),
            FORUM => json!({"id": FORUM, "type": 15, "name": "ideas"}),
            _ => json!({"id": id, "type": 0}),
        };
        match (m, p) {
            (_, "/files/doc") => Resp::bytes("application/pdf", b"%PDF".to_vec()),
            (_, "/files/gone") => Resp::status(404, "gone"),
            _ if req.header("authorization") != format!("Bot {TOKEN}") => Resp::status(401, r#"{"message": "401: Unauthorized"}"#),
            ("GET", "/users/@me") => Resp::json(json!({"id": BOT, "username": "august"})),
            ("GET", "/applications/@me") => Resp::json(json!({"id": "app1"})),
            ("GET", "/gateway/bot") => Resp::json(json!({"url": gateway})),
            // A channel Discord is slow to tell about.
            ("GET", "/channels/900") => Resp::json(channel("900")).after(Duration::from_secs(2)),
            ("GET", p) if p.starts_with("/channels/") => Resp::json(channel(&p["/channels/".len()..])),
            ("POST", "/users/@me/channels") => Resp::json(json!({"id": DM, "type": 1})),
            ("POST", p) if p.ends_with("/threads") => {
                let b = req.json();
                Resp::json(json!({"id": "77", "type": 11, "name": b["name"], "parent_id": TEXT, "owner_id": BOT}))
            }
            ("POST", p) if p.ends_with("/messages") => Resp::json(json!({"id": "42"})),
            _ => Resp::status(204, ""),
        }
    })
    .await
}

/// A message from `user` in `channel` (a DM unless it is in the server).
fn message(id: &str, channel: &str, user: &str, text: &str) -> Value {
    let mut m = json!({"id": id, "channel_id": channel, "content": text,
        "author": {"id": user, "username": "ann", "global_name": "Ann"}});
    if channel != DM {
        m["guild_id"] = json!("g");
    }
    m
}

struct Discord {
    fake: FakeAugust,
    api: FakeHttp,
    gw: Gateway,
}

impl Discord {
    fn calls(&self, method: &str, suffix: &str) -> Vec<Req> {
        self.api.to(suffix).into_iter().filter(|r| r.method == method).collect()
    }

    async fn wait_call(&self, method: &str, suffix: &str) -> Req {
        for _ in 0..1000 {
            if let Some(r) = self.calls(method, suffix).pop() {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no {method} {suffix}")
    }

    async fn messenger(&self, method: &str, mut params: Value) -> anyhow::Result<Value> {
        params["messenger"] = json!("discord");
        self.fake.request(method, params).await
    }
}

/// Discord connected with the token and the owner allowed by the environment.
async fn discord() -> Discord {
    let gw = Gateway::start().await;
    let api = rest(&gw.url).await;
    let (fake, august) =
        FakeAugust::new(&[("DISCORD_BOT_TOKEN", TOKEN), ("DISCORD_ALLOWED_USERS", OWNER), ("DISCORD_API_BASE", &api.url)]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake.wait_until("the messenger", |f| f.manifest().is_some_and(|m| m["messengers"][0]["id"] == "discord")).await;
    gw.wait_op(2, 1).await;
    Discord { fake, api, gw }
}

#[tokio::test]
async fn the_bot_identifies_and_dms_and_commands_come_in() {
    let dc = discord().await;
    let identify = dc.gw.wait_op(2, 1).await;
    assert_eq!(identify["d"]["token"], TOKEN);
    assert_ne!(identify["d"]["intents"].as_u64().unwrap() & 1 << 15, 0, "message content");

    dc.gw.dispatch("MESSAGE_CREATE", message("1", DM, OWNER, "hi"));
    dc.gw.dispatch("MESSAGE_CREATE", message("2", DM, OWNER, "/model x"));
    let hi = dc.fake.wait_call("inbound", 1).await;
    assert_eq!((hi["kind"].as_str(), hi["text"].as_str(), hi["addressed"].as_bool()), (Some("message"), Some("hi"), Some(true)));
    assert_eq!(hi["thread"], json!({"messenger": "discord", "id": DM}));
    assert_eq!((hi["user"]["id"].as_str(), hi["user"]["name"].as_str(), hi["place"]["kind"].as_str()), (Some("7"), Some("Ann (@ann)"), Some("dm")));
    let cmd = dc.fake.wait_call("inbound", 2).await;
    assert_eq!((cmd["kind"].as_str(), cmd["name"].as_str(), cmd["args"].as_str()), (Some("command"), Some("model"), Some("x")));
}

#[tokio::test]
async fn slash_commands_and_presses_are_answered_and_come_in() {
    let dc = discord().await;
    let interaction = |id: &str, user: &str, kind: u64, data: Value| {
        json!({"id": id, "token": "tok", "type": kind, "channel_id": TEXT, "guild_id": "g",
            "member": {"user": {"id": user, "username": "ann"}}, "data": data})
    };
    dc.gw.dispatch("INTERACTION_CREATE", interaction("i1", OWNER, 2, json!({"name": "model", "options": [{"name": "args", "type": 3, "value": "gpt 5"}]})));
    dc.gw.dispatch("INTERACTION_CREATE", interaction("i2", OWNER, 3, json!({"custom_id": "k1.0", "component_type": 2})));
    // A stranger's press is turned away, only to them; nothing reaches August.
    dc.gw.dispatch("INTERACTION_CREATE", interaction("i3", "8", 3, json!({"custom_id": "k"})));
    dc.gw.dispatch("MESSAGE_CREATE", message("4", DM, OWNER, "done"));
    dc.fake.wait_call("inbound", 3).await;
    let got = dc.fake.calls("inbound");
    assert_eq!(got.len(), 3);
    assert_eq!((got[0]["kind"].as_str(), got[0]["name"].as_str(), got[0]["args"].as_str()), (Some("command"), Some("model"), Some("gpt 5")));
    assert_eq!((got[0]["thread"]["id"].as_str(), got[0]["place"]["kind"].as_str()), (Some(TEXT), Some("group")));
    assert_eq!((got[1]["kind"].as_str(), got[1]["button"].as_str()), (Some("press"), Some("k1.0")));

    let answer = |id: &str| dc.calls("POST", &format!("/interactions/{id}/tok/callback"))[0].json();
    assert_eq!((answer("i1")["type"].as_u64(), answer("i1")["data"]["content"].as_str()), (Some(4), Some("`/model gpt 5`")));
    assert_eq!(answer("i2"), json!({"type": 6}));
    assert_eq!(answer("i3")["data"]["flags"], 64);
    assert!(answer("i3")["data"]["content"].as_str().unwrap().contains("not authorized"));
}

#[tokio::test]
async fn presses_are_answered_at_once_even_behind_slow_events() {
    let dc = discord().await;
    dc.gw.dispatch("MESSAGE_CREATE", message("1", "900", OWNER, "hi"));
    dc.gw.dispatch("INTERACTION_CREATE", json!({"id": "i1", "token": "tok", "type": 3, "channel_id": DM,
        "user": {"id": OWNER, "username": "ann"}, "data": {"custom_id": "k"}}));
    dc.wait_call("POST", "/interactions/i1/tok/callback").await;
    assert!(dc.fake.calls("inbound").is_empty(), "answered before the message ahead of it was handed in");
    // August still gets both, in order.
    dc.fake.wait_call("inbound", 2).await;
    let kinds: Vec<Value> = dc.fake.calls("inbound").iter().map(|e| e["kind"].clone()).collect();
    assert_eq!(kinds, [json!("message"), json!("press")]);
}

#[tokio::test]
async fn strangers_are_turned_away_and_ignored_in_servers() {
    let dc = discord().await;
    dc.gw.dispatch("MESSAGE_CREATE", message("1", TEXT, "8", "hi all"));
    dc.gw.dispatch("MESSAGE_CREATE", message("2", DM, "8", "hi"));
    let denied = dc.wait_call("POST", &format!("/channels/{DM}/messages")).await.json();
    assert!(denied["content"].as_str().unwrap().contains("Your Discord user id: 8"), "{denied}");
    dc.gw.dispatch("MESSAGE_CREATE", message("3", DM, OWNER, "hi"));
    dc.fake.wait_call("inbound", 1).await;
    assert!(dc.calls("POST", &format!("/channels/{TEXT}/messages")).is_empty(), "the server chatter got no answer");
    assert_eq!(dc.fake.calls("inbound").len(), 1);
}

#[tokio::test]
async fn servers_mark_what_is_addressed_and_threads_are_places() {
    let dc = discord().await;
    dc.gw.dispatch("MESSAGE_CREATE", message("1", TEXT, OWNER, "dinner at 8?"));
    dc.gw.dispatch("MESSAGE_CREATE", message("2", TEXT, OWNER, "<@99> what's the weather"));
    let mut reply = message("3", TEXT, OWNER, "and tomorrow?");
    reply["referenced_message"] = json!({"id": "9", "content": "Sunny.", "author": {"id": BOT}});
    dc.gw.dispatch("MESSAGE_CREATE", reply);
    // In a thread the bot opened, everything is for August.
    dc.gw.dispatch("MESSAGE_CREATE", message("4", THREAD, OWNER, "and flights?"));
    dc.fake.wait_call("inbound", 4).await;
    let got = dc.fake.calls("inbound");
    let texts: Vec<(&str, bool)> = got.iter().map(|e| (e["text"].as_str().unwrap(), e["addressed"] == true)).collect();
    assert_eq!(texts, [("dinner at 8?", false), ("what's the weather", true), ("and tomorrow?", true), ("and flights?", true)]);
    assert_eq!(got[0]["place"], json!({"kind": "group", "parent": null, "title": "general"}));
    assert_eq!(got[2]["reply_to"], json!({"id": "9", "text": "Sunny.", "mine": true}));
    assert_eq!(got[3]["thread"]["id"], THREAD);
    assert_eq!(got[3]["place"], json!({"kind": "thread", "parent": TEXT, "title": "Trip"}));
}

#[tokio::test]
async fn channels_are_known_from_the_gateway_and_renames_are_seen() {
    let dc = discord().await;
    dc.gw.dispatch("GUILD_CREATE", json!({"id": "g", "channels": [{"id": "501", "type": 5, "name": "news"}], "threads": []}));
    dc.gw.dispatch("MESSAGE_CREATE", message("1", "501", OWNER, "hi"));
    dc.gw.dispatch("CHANNEL_UPDATE", json!({"id": "501", "type": 5, "name": "updates"}));
    dc.gw.dispatch("MESSAGE_CREATE", message("2", "501", OWNER, "hi"));
    dc.fake.wait_call("inbound", 2).await;
    let titles: Vec<Value> = dc.fake.calls("inbound").iter().map(|e| e["place"].clone()).collect();
    assert_eq!(titles, [json!({"kind": "channel", "parent": null, "title": "news"}), json!({"kind": "channel", "parent": null, "title": "updates"})]);
    assert!(dc.api.to("/channels/501").is_empty(), "no lookups");
}

#[tokio::test]
async fn edits_files_and_reactions_come_in() {
    let dc = discord().await;
    dc.gw.dispatch("MESSAGE_UPDATE", message("4", DM, OWNER, "hi there"));
    let mut voice = message("5", DM, OWNER, "");
    voice["flags"] = json!(1 << 13);
    voice["attachments"] = json!([{"id": "a", "url": "https://cdn/x.ogg", "filename": "voice-message.ogg", "content_type": "audio/ogg", "size": 9}]);
    dc.gw.dispatch("MESSAGE_CREATE", voice);
    let reaction = |t: &str| {
        dc.gw.dispatch(t, json!({"user_id": OWNER, "channel_id": DM, "message_id": "40", "emoji": {"id": null, "name": "👍"}}));
    };
    reaction("MESSAGE_REACTION_ADD");
    reaction("MESSAGE_REACTION_REMOVE");
    dc.fake.wait_call("inbound", 4).await;
    let got = dc.fake.calls("inbound");
    assert_eq!((got[0]["kind"].as_str(), got[0]["id"].as_str(), got[0]["text"].as_str()), (Some("edited"), Some("4"), Some("hi there")));
    assert_eq!(got[1]["files"][0], json!({"id": "https://cdn/x.ogg", "kind": "voice", "name": "voice-message.ogg", "mime": "audio/ogg", "size": 9}));
    assert_eq!((got[2]["kind"].as_str(), got[2]["message"].as_str(), got[2]["emoji"].as_str()), (Some("reaction"), Some("40"), Some("👍")));
    assert_eq!(got[3]["emoji"], "");
}

#[tokio::test]
async fn sent_messages_are_markdown_with_components_and_files() {
    let dc = discord().await;
    let msg = json!({"text": "**Done**", "reply_to": "3", "buttons": [[{"id": "a", "label": "✅ Allow"}, {"id": "d", "label": "❌ Deny"}]]});
    let id = dc.messenger("messenger_send", json!({"thread": TEXT, "message": msg})).await.unwrap();
    assert_eq!(id, "42");
    let sent = dc.calls("POST", &format!("/channels/{TEXT}/messages"))[0].json();
    assert_eq!(sent["content"], "**Done**");
    assert_eq!(sent["message_reference"]["message_id"], "3");
    assert_eq!(sent["allowed_mentions"], json!({"parse": ["users"]}));
    assert_eq!(sent["components"][0]["components"][1], json!({"type": 2, "style": 2, "label": "❌ Deny", "custom_id": "d"}));

    // More rows than fit: the buttons are packed, five to a row.
    let many: Vec<Value> = (0..12).map(|i| json!([{"id": format!("b{i}"), "label": format!("{i}")}])).collect();
    dc.messenger("messenger_send", json!({"thread": TEXT, "message": {"text": "Pick", "buttons": many}})).await.unwrap();
    let rows = dc.calls("POST", &format!("/channels/{TEXT}/messages"))[1].json()["components"].clone();
    let sizes: Vec<usize> = rows.as_array().unwrap().iter().map(|r| r["components"].as_array().unwrap().len()).collect();
    assert_eq!(sizes, [5, 5, 2]);

    // An edit without buttons takes them away.
    dc.messenger("messenger_edit", json!({"thread": TEXT, "id": "42", "message": {"text": "Done."}})).await.unwrap();
    assert_eq!(dc.calls("PATCH", "/messages/42")[0].json(), json!({"content": "Done.", "components": []}));

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("chart.png");
    std::fs::write(&file, b"PNG").unwrap();
    dc.messenger("messenger_send", json!({"thread": DM, "message": {"text": "Here", "files": [file]}})).await.unwrap();
    let upload = dc.calls("POST", &format!("/channels/{DM}/messages"))[0].text();
    assert!(upload.contains("name=\"payload_json\"") && upload.contains("\"content\":\"Here\""), "{upload}");
    assert!(upload.contains("name=\"files[0]\"; filename=\"chart.png\""), "{upload}");
}

#[tokio::test]
async fn reactions_commands_threads_and_pins_go_out() {
    let dc = discord().await;
    dc.messenger("messenger_react", json!({"thread": TEXT, "id": "1", "emoji": "👀"})).await.unwrap();
    dc.messenger("messenger_react", json!({"thread": TEXT, "id": "1", "emoji": "✅"})).await.unwrap();
    let base = format!("/channels/{TEXT}/messages/1/reactions");
    let puts: Vec<String> = dc.calls("PUT", "/@me").iter().map(|r| r.path.clone()).collect();
    assert_eq!(puts, [format!("{base}/%F0%9F%91%80/@me"), format!("{base}/%E2%9C%85/@me")]);
    // The new reaction replaced the old one.
    assert_eq!(dc.calls("DELETE", "/@me")[0].path, format!("{base}/%F0%9F%91%80/@me"));

    let commands = json!([{"name": "new", "description": "New session"}, {"name": "Not Valid", "description": "x"}]);
    dc.messenger("messenger_commands", json!({"commands": commands})).await.unwrap();
    let menu = dc.calls("PUT", "/applications/app1/commands")[0].json();
    assert_eq!(menu.as_array().unwrap().len(), 1);
    assert_eq!((menu[0]["name"].as_str(), menu[0]["description"].as_str()), (Some("new"), Some("New session")));
    assert_eq!(menu[0]["options"][0]["name"], "args");

    let thread = dc.messenger("messenger_open_thread", json!({"parent": TEXT, "title": "Trip"})).await.unwrap();
    assert_eq!(thread, "77");
    assert_eq!(dc.calls("POST", &format!("/channels/{TEXT}/threads"))[0].json(), json!({"name": "Trip", "type": 11}));
    // A forum's thread is a post with a first message.
    dc.messenger("messenger_open_thread", json!({"parent": FORUM, "title": "Idea"})).await.unwrap();
    assert_eq!(dc.calls("POST", &format!("/channels/{FORUM}/threads"))[0].json()["message"]["content"], "Idea");

    // A pin without a message id never reaches Discord.
    assert!(dc.messenger("messenger_action", json!({"thread": TEXT, "action": "pin", "args": {}})).await.is_err());
    dc.messenger("messenger_action", json!({"thread": TEXT, "action": "pin", "args": {"message": "1"}})).await.unwrap();
    assert_eq!(dc.calls("PUT", "/pins/1")[0].path, format!("/channels/{TEXT}/messages/pins/1"));
}

#[tokio::test]
async fn files_download_without_the_token() {
    let dc = discord().await;
    let file = |name: &str| json!({"thread": DM, "file": {"id": format!("{}/files/{name}", dc.api.url), "kind": "document"}});
    let got = dc.messenger("messenger_download", file("doc")).await.unwrap();
    assert_eq!(got, "JVBERg=="); // "%PDF", base64
    assert_eq!(dc.api.to("/files/doc")[0].header("authorization"), "");
    let err = dc.messenger("messenger_download", file("gone")).await.unwrap_err().to_string();
    assert!(err.contains("download failed"), "{err}");
}

#[tokio::test]
async fn a_dropped_gateway_resumes_where_it_was() {
    let dc = discord().await;
    dc.gw.dispatch("MESSAGE_CREATE", message("1", DM, OWNER, "one"));
    dc.fake.wait_call("inbound", 1).await;
    // Discord asks the bot to reconnect: it resumes the session from the last event.
    dc.gw.send(json!({"op": 7, "d": null}));
    let resume = dc.gw.wait_op(6, 1).await;
    assert_eq!(resume["d"], json!({"token": TOKEN, "session_id": "s1", "seq": 2}));
    dc.gw.dispatch("MESSAGE_CREATE", message("2", DM, OWNER, "two"));
    assert_eq!(dc.fake.wait_call("inbound", 2).await["text"], "two");
}

#[tokio::test]
async fn a_bot_without_message_content_fails_its_health_check_saying_why() {
    let dc = discord().await;
    assert_eq!(dc.fake.request("health", json!({})).await.unwrap()["status"], "ok");
    dc.gw.close(4014);
    for _ in 0..500 {
        let h = dc.fake.request("health", json!({})).await.unwrap();
        if h["status"] == "failed" {
            assert!(h["detail"].as_str().unwrap().contains("Message Content Intent"), "{h}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("health never failed");
}

#[tokio::test]
async fn login_connects_the_bot_and_pairs_the_owner() {
    let gw = Gateway::start().await;
    let api = rest(&gw.url).await;
    let (fake, august) = FakeAugust::new(&[("DISCORD_API_BASE", &api.url)]);
    fake.on("login_ask", |_| Ok(json!(TOKEN)));
    fake.on("login_choose", |_| Ok(json!("Allow")));
    tokio::spawn(serve(august));
    fake.started().await;

    let login = tokio::spawn({
        let fake = fake.clone();
        async move { fake.request("login", json!({"account": "discord", "session": 1})).await }
    });
    // The token, then the invite; the owner messages the bot and is confirmed.
    fake.wait_until("pairing", |f| f.calls("login_progress").iter().any(|p| p["text"].as_str().unwrap().contains("client_id=app1"))).await;
    gw.wait_op(2, 1).await;
    gw.dispatch("MESSAGE_CREATE", message("1", DM, OWNER, "hi"));
    assert_eq!(login.await.unwrap().unwrap()["who"], "@august");
    assert!(fake.calls("login_choose")[0]["question"].as_str().unwrap().contains("Ann (@ann) (id 7)"));
    assert_eq!(fake.calls("secret_set")[0], json!({"key": "token", "value": TOKEN}));
    assert_eq!(fake.calls("config_set")[0], json!({"path": "extensions.discord.settings.allowed", "value": ["7"]}));
    let hello = api.to(&format!("/channels/{DM}/messages"));
    assert!(hello[0].json()["content"].as_str().unwrap().contains("connected"));

    // Now the owner talks to August in Discord.
    gw.dispatch("MESSAGE_CREATE", message("2", DM, OWNER, "hello"));
    assert_eq!(fake.wait_call("inbound", 1).await["text"], "hello");
}
