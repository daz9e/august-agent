//! The wizard against fakes: a core on a control socket that answers its operations, a
//! terminal window socket where a sign-in asks, and a scripted terminal UI.

use super::{Answer, Core, Say, Ui, serve, wizard};
use august_ext::FakeAugust;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

/// Answers given in turn, and what was shown.
#[derive(Default)]
struct Script {
    answers: Mutex<VecDeque<Option<Answer>>>,
    asked: Mutex<Vec<(String, Vec<String>)>>,
    said: Mutex<Vec<(Say, String)>>,
}

impl Script {
    fn new(answers: Vec<Option<Answer>>) -> Arc<Script> {
        Arc::new(Script { answers: Mutex::new(answers.into()), ..Default::default() })
    }

    fn next(&self, question: &str, options: &[String]) -> Option<Answer> {
        self.asked.lock().unwrap().push((question.into(), options.to_vec()));
        self.answers.lock().unwrap().pop_front().unwrap_or_else(|| panic!("nothing scripted for {question:?}"))
    }
}

impl Ui for Script {
    fn step(&self, _: usize, _: &str) {}
    fn say(&self, kind: Say, text: &str) {
        self.said.lock().unwrap().push((kind, text.into()));
    }
    fn busy(&self, _: Option<&str>) {}
    fn select(&self, question: &str, options: &[String], _: bool, _: &AtomicBool) -> Option<Answer> {
        self.next(question, options)
    }
    fn input(&self, question: &str, _: bool, _: &AtomicBool) -> Option<String> {
        self.next(question, &[]).map(|a| match a {
            Answer::Typed(t) => t,
            Answer::Pick(_) => panic!("{question:?} takes text"),
        })
    }
    fn summary(&self, rows: &[(&str, String)]) {
        let text = rows.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n");
        self.said.lock().unwrap().push((Say::Ok, text));
    }
}

/// A core with an Anthropic key account (no key yet) and Telegram: its operations, the calls
/// made, and a terminal window where a sign-in to Anthropic asks for the key.
struct Fake {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    /// What the window got back.
    window: Arc<Mutex<Vec<Value>>>,
}

impl Fake {
    fn start(dir: &Path) -> Fake {
        let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let window: Arc<Mutex<Vec<Value>>> = Arc::default();
        let state = Arc::new(Mutex::new((String::new(), String::new(), false)));
        let key = Arc::new(tokio::sync::Notify::new());

        let control = UnixListener::bind(dir.join("control.sock")).unwrap();
        let (log, key_in) = (calls.clone(), key.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = control.accept().await {
                let (log, state, key) = (log.clone(), state.clone(), key_in.clone());
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let req: Value = serde_json::from_str(&BufReader::new(read).lines().next_line().await.unwrap().unwrap()).unwrap();
                    // A new token, as `august` gets one; any other is a previous core's.
                    let early = match (req["op"].as_str(), req["token"].as_str()) {
                        (Some("cli_open"), _) => Some(json!({"result": {"exec": [], "token": "t0k3n"}})),
                        (_, Some("t0k3n")) => None,
                        _ => Some(json!({"error": "unknown token"})),
                    };
                    if let Some(reply) = early {
                        write.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
                        return;
                    }
                    let (op, p) = (req["call"].as_str().unwrap().to_string(), req["params"].clone());
                    log.lock().unwrap().push((op.clone(), p.clone()));
                    if op == "login" {
                        key.notified().await;
                    }
                    let reply = {
                        let mut s = state.lock().unwrap();
                        match op.as_str() {
                            "status" => json!({"result": {"provider": s.0, "model": s.1, "workspace": "/w"}}),
                            "accounts" => json!({"result": [
                                {"id": "anthropic", "label": "Anthropic", "providers": ["anthropic"], "status": if s.2 { "connected" } else { "none" }},
                                {"id": "telegram", "label": "Telegram", "providers": [], "status": "none"},
                            ]}),
                            "login" => {
                                s.2 = true;
                                json!({"result": {"who": null}})
                            }
                            "models" => json!({"result": [{"id": "claude-a"}, {"id": "claude-b"}]}),
                            "model_set" => {
                                let (p, m) = p["model"].as_str().unwrap().split_once(':').unwrap();
                                *s = (p.into(), if m.is_empty() { "claude-a".into() } else { m.into() }, s.2);
                                json!({"result": {"provider": s.0, "model": s.1}})
                            }
                            _ => json!({"error": format!("unknown {op}")}),
                        }
                    };
                    write.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
                });
            }
        });

        let terminal = UnixListener::bind(dir.join("august.sock")).unwrap();
        let got = window.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = terminal.accept().await {
                let (got, key) = (got.clone(), key.clone());
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    assert_eq!(lines.next_line().await.unwrap().unwrap(), r#"{"type":"hello"}"#);
                    write.write_all(b"{\"type\":\"hello\",\"thread\":\"1\"}\n").await.unwrap();
                    let ask = json!({"type": "send", "id": "1", "text": "🔑 Send your API key for Anthropic.", "buttons": [], "files": []});
                    write.write_all(format!("{ask}\n").as_bytes()).await.unwrap();
                    while let Ok(Some(line)) = lines.next_line().await {
                        got.lock().unwrap().push(serde_json::from_str(&line).unwrap());
                        key.notify_one();
                    }
                });
            }
        });
        Fake { calls, window }
    }

    fn calls(&self, op: &str) -> Vec<Value> {
        self.calls.lock().unwrap().iter().filter(|(o, _)| o == op).map(|(_, p)| p.clone()).collect()
    }
}

fn core(dir: &Path) -> Core {
    Core::new(dir.join("control.sock"), "t0k3n".into())
}

#[tokio::test]
async fn walks_through_provider_sign_in_model_and_connections() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::start(dir.path());
    let ui = Script::new(vec![
        Some(Answer::Pick(0)),                    // provider: Anthropic
        Some(Answer::Typed("sk-1".into())),       // its key, asked in the window
        Some(Answer::Typed("claude-next".into())), // a model the list doesn't have
        Some(Answer::Pick(1)),                    // Telegram: skip
    ]);
    wizard(&core(dir.path()), ui.clone(), &dir.path().join("august.sock")).await.unwrap();

    let asked = ui.asked.lock().unwrap().clone();
    assert_eq!(asked[0].1, ["Anthropic"]);
    assert_eq!(asked[1].0, "Send your API key for Anthropic.");
    assert_eq!(asked[2].1, ["Keep claude-a", "claude-b"]);
    assert_eq!(asked[3], ("Connect Telegram?".into(), vec!["Connect".into(), "Skip".into()]));
    // The key went to the window the sign-in asked in, and that is the thread it ran in.
    assert_eq!(fake.window.lock().unwrap()[0], json!({"type": "text", "text": "sk-1"}));
    assert_eq!(fake.calls("login"), [json!({"account": "anthropic", "thread": {"messenger": "cli", "id": "1"}})]);
    assert_eq!(fake.calls("model_set"), [json!({"model": "anthropic:"}), json!({"model": "anthropic:claude-next"})]);
    let said = ui.said.lock().unwrap().clone();
    assert!(said.iter().any(|(k, t)| *k == Say::Ok && t.contains("Model: anthropic · claude-next") && t.contains("Signed in: Anthropic")), "{said:?}");
}

#[tokio::test]
async fn keeping_the_provider_in_use_goes_on_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::start(dir.path());
    let core = core(dir.path());
    core.call("model_set", json!({"model": "anthropic:claude-a"})).await.unwrap();
    let ui = Script::new(vec![Some(Answer::Pick(0)), None]); // keep it, then step away
    wizard(&core, ui.clone(), &dir.path().join("august.sock")).await.unwrap();

    let asked = ui.asked.lock().unwrap().clone();
    assert_eq!(asked[0].1[0], "Keep anthropic · claude-a");
    assert!(asked[1].0.starts_with("Which model of anthropic?"), "{asked:?}");
    assert_eq!(fake.calls("model_set").len(), 1);
}

#[tokio::test]
async fn stepping_away_pauses_without_changing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::start(dir.path());
    let ui = Script::new(vec![None]);
    wizard(&core(dir.path()), ui.clone(), &dir.path().join("august.sock")).await.unwrap();

    assert!(fake.calls("model_set").is_empty() && fake.calls("login").is_empty());
    assert!(ui.said.lock().unwrap().iter().any(|(_, t)| t.contains("Setup paused")));
}

#[tokio::test]
async fn a_restarted_core_hands_out_a_new_token() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::start(dir.path());
    let core = Core::new(dir.path().join("control.sock"), "from-the-core-before".into());
    let ui = Script::new(vec![None]);
    wizard(&core, ui.clone(), &dir.path().join("august.sock")).await.unwrap();

    assert_eq!(ui.asked.lock().unwrap()[0].1, ["Anthropic"]);
    assert_eq!(*core.token.lock().unwrap(), "t0k3n");
    assert!(!fake.calls("accounts").is_empty());
}

#[tokio::test]
async fn adds_setup_to_the_august_program() {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake.wait_until("the cli command", |f| f.manifest().is_some_and(|m| m["cli"][0]["name"] == "setup")).await;
    let manifest = fake.manifest().unwrap();
    let exec: Vec<String> = serde_json::from_value(manifest["cli"][0]["exec"].clone()).unwrap();
    assert_eq!(exec[1..], ["run"]);
    assert!(Path::new(&exec[0]).is_file());
}
