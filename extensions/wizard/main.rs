//! `wizard`: `august setup`, setting August up in the terminal with menus. Two halves in one
//! binary:
//! - as an extension (no arguments) it only adds `setup` to the `august` program;
//! - as `run` (what `august setup` runs) it installs August as a background service
//!   (`august serve`), then walks through a model provider, a model and connections
//!   (messengers, services): arrow-key menus with type-to-filter, hidden key input, spinners.
//!
//! It calls the core over its control socket with the token `august` hands it. A sign-in
//! asks its questions in a thread, so for one it opens a window of the `terminal` extension
//! (`august.sock` next to the control socket) and answers what arrives there with menus.
//! In a chat, `/onboard` (the `commands` extension) walks the same steps.

#[cfg(test)]
mod tests;
mod ui;

use anyhow::{Context, Result, bail};
use august_ext::August;
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Steps of the walk: the service, then the three `wizard` takes.
pub const STEPS: usize = 4;

/// What the user picked in a menu: an option, or (where allowed) what they typed.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Pick(usize),
    Typed(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Say {
    Info,
    Ok,
    Fail,
}

/// The terminal as the walk sees it. `select` and `input` block until answered; `None`:
/// the user stepped away (Esc, Ctrl-C) or `stop` was set.
pub trait Ui: Send + Sync + 'static {
    fn step(&self, n: usize, title: &str);
    fn say(&self, kind: Say, text: &str);
    /// A spinner with `text` until the next call (`None`: just stop it).
    fn busy(&self, text: Option<&str>);
    fn select(&self, question: &str, options: &[String], custom: bool, stop: &AtomicBool) -> Option<Answer>;
    fn input(&self, question: &str, secret: bool, stop: &AtomicBool) -> Option<String>;
    fn summary(&self, rows: &[(&str, String)]);
}

/// The running core, reached as the extension that owns `august setup`.
pub struct Core {
    pub socket: PathBuf,
    pub token: std::sync::Mutex<String>,
}

/// The core went away (or came back as a new one) during a call.
const RESTARTED: &str = "unknown token";

impl Core {
    pub fn new(socket: PathBuf, token: String) -> Core {
        Core { socket, token: token.into() }
    }

    /// One operation, on its own connection (a sign-in may take minutes). A token dies with
    /// the core that gave it, so after a restart this takes a new one and tries once more.
    pub async fn call(&self, op: &str, params: Value) -> Result<Value> {
        let token = self.token.lock().unwrap().clone();
        match self.request(json!({"token": token, "call": op, "params": params})).await {
            Err(e) if e.to_string() == RESTARTED || e.downcast_ref::<std::io::Error>().is_some() => {
                self.renew().await?;
                let token = self.token.lock().unwrap().clone();
                self.request(json!({"token": token, "call": op, "params": params})).await
            }
            other => other,
        }
    }

    /// A new token, the way `august setup` gets one, once the core answers again.
    async fn renew(&self) -> Result<()> {
        for _ in 0..300 {
            if let Ok(found) = self.request(json!({"op": "cli_open", "name": "setup"})).await {
                *self.token.lock().unwrap() = str(&found["token"]).to_string();
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        bail!("August stopped answering; see `august logs`")
    }

    async fn request(&self, req: Value) -> Result<Value> {
        let stream = UnixStream::connect(&self.socket).await?;
        let (read, mut write) = stream.into_split();
        write.write_all(format!("{req}\n").as_bytes()).await?;
        let line = BufReader::new(read).lines().next_line().await?.ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
        let reply: Value = serde_json::from_str(&line)?;
        match reply["error"].as_str() {
            Some(e) => bail!("{e}"),
            None => Ok(reply["result"].clone()),
        }
    }
}

fn str(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn list(v: &Value) -> Vec<Value> {
    v.as_array().cloned().unwrap_or_default()
}

/// Runs a blocking prompt off the async threads.
async fn prompt<T: Send + 'static>(ui: &Arc<dyn Ui>, f: impl FnOnce(&dyn Ui) -> T + Send + 'static) -> T {
    let ui = ui.clone();
    tokio::task::spawn_blocking(move || f(&*ui)).await.expect("a prompt panicked")
}

async fn select(ui: &Arc<dyn Ui>, question: &str, options: Vec<String>, custom: bool) -> Option<Answer> {
    let question = question.to_string();
    prompt(ui, move |ui| ui.select(&question, &options, custom, &AtomicBool::new(false))).await
}

/// The walk after the service step. `window`: the terminal's socket, for sign-ins.
pub async fn wizard(core: &Core, ui: Arc<dyn Ui>, window: &Path) -> Result<()> {
    let done = provider(core, &ui, window).await? && model(core, &ui).await? && connections(core, &ui, window).await?;
    if !done {
        ui.say(Say::Info, "Setup paused: `august setup` picks up again.");
        return Ok(());
    }
    let s = core.call("status", json!({})).await?;
    let signed: Vec<String> = list(&core.call("accounts", json!({})).await?).iter().filter(|a| a["status"] == "connected").map(|a| str(&a["label"]).to_string()).collect();
    ui.summary(&[
        ("Model", format!("{} · {}", str(&s["provider"]), str(&s["model"]))),
        ("Signed in", if signed.is_empty() { "—".into() } else { signed.join(", ") }),
        ("Workspace", str(&s["workspace"]).to_string()),
    ]);
    ui.say(Say::Info, "Chat with `august`; in it, `/help` lists commands. `august setup` runs this again.");
    Ok(())
}

/// Step 2: a model provider, signed in and in use. `false`: the user stepped away.
async fn provider(core: &Core, ui: &Arc<dyn Ui>, window: &Path) -> Result<bool> {
    ui.step(2, "Model provider");
    loop {
        let s = core.call("status", json!({})).await?;
        let accounts = list(&core.call("accounts", json!({})).await?);
        let providers: Vec<&Value> = accounts.iter().filter(|a| a["providers"].as_array().is_some_and(|p| !p.is_empty())).collect();
        let current = str(&s["provider"]);
        let keep = (!current.is_empty()).then(|| format!("Keep {current} · {}", str(&s["model"])));
        let options = keep.iter().cloned().chain(providers.iter().map(|a| labelled(a))).collect();
        let Some(Answer::Pick(i)) = select(ui, "August thinks with a model. Which provider?", options, false).await else { return Ok(false) };
        // "Keep" (first, when shown) leaves things as they are.
        let Some(a) = i.checked_sub(keep.is_some() as usize).and_then(|i| providers.get(i)) else { return Ok(true) };
        if a["status"] != "connected" && !sign_in(core, ui, window, a).await? {
            continue;
        }
        match core.call("model_set", json!({"model": format!("{}:", str(&a["providers"][0]))})).await {
            Ok(m) => {
                ui.say(Say::Ok, &format!("Using {} · {}", str(&m["provider"]), str(&m["model"])));
                return Ok(true);
            }
            Err(e) => ui.say(Say::Fail, &format!("Could not switch to it: {e:#}")),
        }
    }
}

/// An account as a menu line: its label, and how it stands.
fn labelled(a: &Value) -> String {
    match (str(&a["status"]), a["who"].as_str()) {
        ("connected", Some(who)) => format!("{}  ✓ {who}", str(&a["label"])),
        ("connected", None) => format!("{}  ✓", str(&a["label"])),
        ("expired", _) => format!("{}  ⚠ expired", str(&a["label"])),
        _ => str(&a["label"]).to_string(),
    }
}

/// Step 3: the model of the provider in use; any id may be typed.
async fn model(core: &Core, ui: &Arc<dyn Ui>) -> Result<bool> {
    ui.step(3, "Model");
    loop {
        let s = core.call("status", json!({})).await?;
        let (provider, model) = (str(&s["provider"]).to_string(), str(&s["model"]).to_string());
        if provider.is_empty() {
            ui.say(Say::Info, "No provider in use yet: skipped.");
            return Ok(true);
        }
        ui.busy(Some(&format!("Fetching the models of {provider}")));
        let listed = core.call("models", json!({"provider": provider})).await.unwrap_or_default();
        ui.busy(None);
        let ids: Vec<String> = list(&listed).iter().filter_map(|m| m["id"].as_str()).filter(|id| *id != model).map(String::from).collect();
        let options = std::iter::once(format!("Keep {model}")).chain(ids.iter().cloned()).collect();
        let picked = match select(ui, &format!("Which model of {provider}? Type to filter, or any id"), options, true).await {
            None => return Ok(false),
            Some(Answer::Pick(0)) => return Ok(true),
            Some(Answer::Pick(i)) => ids[i - 1].clone(),
            Some(Answer::Typed(id)) => id,
        };
        match core.call("model_set", json!({"model": format!("{provider}:{picked}")})).await {
            Ok(m) => {
                ui.say(Say::Ok, &format!("Using {} · {}", str(&m["provider"]), str(&m["model"])));
                return Ok(true);
            }
            Err(e) => ui.say(Say::Fail, &format!("Could not switch model: {e:#}")),
        }
    }
}

/// Step 4: messengers and other services, one question each.
async fn connections(core: &Core, ui: &Arc<dyn Ui>, window: &Path) -> Result<bool> {
    ui.step(4, "Connections");
    let accounts = list(&core.call("accounts", json!({})).await?);
    let others: Vec<&Value> = accounts.iter().filter(|a| a["providers"].as_array().is_none_or(|p| p.is_empty())).collect();
    if others.is_empty() {
        ui.say(Say::Info, "Nothing else to connect.");
    }
    for a in others {
        if a["status"] == "connected" {
            ui.say(Say::Ok, &labelled(a));
            continue;
        }
        let question = format!("Connect {}?", str(&a["label"]));
        match select(ui, &question, vec!["Connect".into(), "Skip".into()], false).await {
            None => return Ok(false),
            Some(Answer::Pick(0)) => {
                sign_in(core, ui, window, a).await?;
            }
            _ => {}
        }
    }
    Ok(true)
}

/// Signs in to account `a` from a terminal window: what the sign-in asks there becomes a
/// menu or an input. `false`: it didn't go through (told to the user).
async fn sign_in(core: &Core, ui: &Arc<dyn Ui>, window: &Path, a: &Value) -> Result<bool> {
    let label = str(&a["label"]).to_string();
    // Right after a restart the terminal may still be starting.
    let mut stream = UnixStream::connect(window).await;
    for _ in 0..50 {
        if stream.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        stream = UnixStream::connect(window).await;
    }
    let stream = stream.with_context(|| format!("signing in needs the terminal extension (no window socket at {})", window.display()))?;
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    write.write_all(b"{\"type\":\"hello\"}\n").await?;
    let hello: Value = serde_json::from_str(&lines.next_line().await?.context("the terminal closed the window")?)?;
    let thread = json!({"messenger": "cli", "id": hello["thread"]});
    let login = core.call("login", json!({"account": a["id"], "thread": thread}));
    tokio::pin!(login);
    let stop = Arc::new(AtomicBool::new(false));
    // The question being answered: its prompt, and the ids of its buttons (if a menu).
    let mut asking: Option<(tokio::task::JoinHandle<Option<Answer>>, Vec<String>)> = None;
    loop {
        tokio::select! {
            signed = &mut login => {
                stop.store(true, Ordering::Relaxed);
                if let Some((open, _)) = asking.take() {
                    open.await.ok();
                }
                return Ok(match signed {
                    Ok(v) => {
                        let who = v["who"].as_str().map(|w| format!(" as {w}")).unwrap_or_default();
                        ui.say(Say::Ok, &format!("Signed in to {label}{who}"));
                        true
                    }
                    Err(e) => {
                        ui.say(Say::Fail, &format!("{label}: {e:#}"));
                        false
                    }
                });
            }
            answer = async { (&mut asking.as_mut().unwrap().0).await }, if asking.is_some() => {
                let (_, ids) = asking.take().unwrap();
                let reply = match answer.ok().flatten() {
                    Some(Answer::Pick(i)) => json!({"type": "press", "button": ids[i]}),
                    Some(Answer::Typed(text)) => json!({"type": "text", "text": text}),
                    // Stepping away cancels the sign-in (its reply ends the loop).
                    None => json!({"type": "text", "text": "/stop"}),
                };
                write.write_all(format!("{reply}\n").as_bytes()).await?;
            }
            line = lines.next_line(), if asking.is_none() => {
                let Ok(Some(line)) = line else {
                    // August restarted under the sign-in: it can be tried again.
                    ui.say(Say::Fail, &format!("{label}: the sign-in window closed (August restarted?)"));
                    return Ok(false);
                };
                let msg: Value = serde_json::from_str(&line).unwrap_or_default();
                if msg["type"] != "send" {
                    continue;
                }
                let text = str(&msg["text"]).to_string();
                let buttons: Vec<&Value> = msg["buttons"].as_array().into_iter().flatten().flat_map(|row| row.as_array().into_iter().flatten()).collect();
                let ids = buttons.iter().map(|b| str(&b["id"]).to_string()).collect();
                let labels: Vec<String> = buttons.iter().map(|b| str(&b["label"]).to_string()).collect();
                let stop = stop.clone();
                let ui = ui.clone();
                let open = if let Some(q) = text.strip_prefix("🔑 ") {
                    let q = q.to_string();
                    tokio::task::spawn_blocking(move || ui.input(&q, true, &stop).map(Answer::Typed))
                } else if let Some(q) = text.strip_prefix("❓ ") {
                    let q = q.to_string();
                    if labels.is_empty() {
                        tokio::task::spawn_blocking(move || ui.input(&q, false, &stop).map(Answer::Typed))
                    } else {
                        tokio::task::spawn_blocking(move || ui.select(&q, &labels, false, &stop))
                    }
                } else if text.contains("paste that page's address") {
                    // Waiting for the browser; one on another device ends on a page that doesn't load.
                    let q = "Waiting for the browser… on another device? Paste the address it ends on".to_string();
                    tokio::task::spawn_blocking(move || ui.input(&q, false, &stop).map(Answer::Typed))
                } else {
                    ui.say(Say::Info, &text);
                    continue;
                };
                asking = Some((open, ids));
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("run") {
        return run(args.get(1).map(String::as_str) == Some("--served")).await;
    }
    serve(August::new()).await;
    Ok(())
}

pub async fn serve(august: August) {
    august.describe(
        "`august setup`: sets August up in the terminal, step by step, with menus",
        "Installs August as a background service, then walks through a model provider, a model and \
         connections. Sign-ins run in a window of the `terminal` extension.",
    );
    august.needs(&["admin", "models"]);
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "august-ext-wizard".into());
    august.register_cli("setup", "Run August in the background and set it up step by step", &[exe, "run".into()]);
    august.run().await;
}

/// `august setup`. `served`: the service step is done (this is the run after it).
async fn run(served: bool) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("`august setup` needs a terminal; in a chat, /onboard walks the same steps");
    }
    let ui = Arc::new(ui::Term::default());
    // The `august` next to this binary (an install keeps them together), else the one on PATH.
    let sibling = std::env::current_exe()?.with_file_name("august");
    let august = if sibling.exists() { sibling } else { PathBuf::from("august") };
    if !served {
        ui.banner();
        ui.step(1, "Background service");
        ui.busy(Some("Installing August as a service and restarting it"));
        let out = tokio::process::Command::new(&august).arg("serve").output().await.with_context(|| format!("run {}", august.display()))?;
        ui.busy(None);
        if !out.status.success() {
            ui.say(Say::Fail, String::from_utf8_lossy(&out.stderr).trim());
            bail!("`august serve` failed; see `august logs`");
        }
        ui.say(Say::Ok, "August runs in the background and starts at login");
        for line in String::from_utf8_lossy(&out.stdout).lines().filter(|l| l.contains("linger")) {
            ui.say(Say::Info, line);
        }
        // Serving restarted August, and the token this run got went with it: run again.
        let err = std::process::Command::new(&august).args(["setup", "--served"]).exec();
        bail!("could not run {}: {err}", august.display());
    }
    let socket = PathBuf::from(std::env::var("AUGUST_SOCKET").context("run by `august setup` (no AUGUST_SOCKET)")?);
    let core = Core::new(socket.clone(), std::env::var("AUGUST_TOKEN").unwrap_or_default());
    wizard(&core, ui, &socket.with_file_name("august.sock")).await
}
