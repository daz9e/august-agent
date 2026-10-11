//! `discord`: August in Discord, as a bot. The gateway for what happens, the REST API for
//! the rest. Discord renders Markdown itself, so text goes out as is; buttons are message
//! components, slash commands the command menu, files uploads (images shown inline),
//! reactions both ways, edits for streaming. An allowlist of user ids (others are turned
//! away); in servers a message is addressed when the bot is mentioned or replied to, or
//! is in a thread the bot opened. Every channel, thread and DM is a thread of its own (its
//! channel id).
//!
//! Connecting is the account `discord` (`/login discord`, `august connect discord`): the
//! bot token from the Developer Portal, then the owner pairs by messaging the bot. The
//! token is a secret, the allowed user ids a setting (`allowed`); `DISCORD_BOT_TOKEN` and
//! `DISCORD_ALLOWED_USERS` (comma-separated) stand in for them. `DISCORD_API_BASE` points
//! at another API root (or a test double); the gateway's address comes from the API.

mod api;
mod gateway;

use anyhow::{Context, Result, anyhow, bail};
use api::{Api, segment};
use async_trait::async_trait;
use august_ext::messenger::{
    Action, Attachment, Button, Capabilities, CommandSpec, Description, FileKind, InboundKind, Messenger, OutMessage, Place,
    PlaceKind, Quote, User, parse_command,
};
use august_ext::{August, Login, Signed, Thread};
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const ID: &str = "discord";
/// Discord's limit for a message's content; Markdown goes out as is.
const MAX_LEN: usize = 2000;
/// Components: up to 5 rows of up to 5 buttons, labels up to 80 characters.
const ROWS: usize = 5;
const ROW_LEN: usize = 5;
const MAX_LABEL: usize = 80;
/// Files in one message; more go in the messages after it.
const MAX_FILES: usize = 10;
/// How long pairing waits for the owner's message.
const PAIR_WAIT: Duration = Duration::from_secs(180);
/// Messages whose own reaction is remembered (to take it back), the latest ones.
const REACTED_KEPT: usize = 256;
/// What the bot asks for when it is added to a server: view channels, send messages (and
/// in threads), create public threads, embed links, attach files, read the history, add
/// reactions, manage messages (to pin).
const PERMISSIONS: u64 = 1 << 10 | 1 << 11 | 1 << 38 | 1 << 35 | 1 << 14 | 1 << 15 | 1 << 16 | 1 << 6 | 1 << 13;

// ---------------------------------------------------------------- inbound parsing

#[derive(Clone)]
struct Bot {
    id: String,
    username: String,
    /// The application's id: its slash commands and invite link.
    app: String,
}

/// A channel as far as August cares: its Discord type, the place it is, and whether it is
/// a thread the bot opened (everything in it is addressed to August).
#[derive(Clone, Debug, Default)]
struct Channel {
    kind: u64,
    place: Place,
    mine: bool,
}

const DM: u64 = 1;
const THREADS: [u64; 3] = [10, 11, 12];
const FORUMS: [u64; 2] = [15, 16];

fn channel_of(c: &Value, bot: &Bot) -> Channel {
    let kind = c["type"].as_u64().unwrap_or(0);
    let title = c["name"].as_str().map(str::to_string);
    let thread = THREADS.contains(&kind);
    let place = match kind {
        DM => Place::default(),
        5 => Place { kind: PlaceKind::Channel, parent: None, title },
        _ if thread => Place { kind: PlaceKind::Thread, parent: c["parent_id"].as_str().map(str::to_string), title },
        _ => Place { kind: PlaceKind::Group, parent: None, title },
    };
    Channel { kind, place, mine: thread && c["owner_id"].as_str() == Some(&bot.id) }
}

/// What came in to `thread` (a channel id) at `place` from `user`.
#[derive(Debug)]
struct Event {
    thread: String,
    place: Place,
    user: User,
    kind: InboundKind,
}

#[derive(Debug)]
enum Parsed {
    Ignore,
    /// Sender is not on the allowlist: `(channel, message, user id, their name)`.
    Deny(String, String, String, String),
    Event(Event),
}

fn display_name(u: &Value) -> String {
    let user = u["username"].as_str().unwrap_or("");
    match u["global_name"].as_str() {
        Some(g) if !g.is_empty() && g != user => format!("{g} (@{user})"),
        _ => format!("@{user}"),
    }
}

/// The user of an interaction: a member's in a server, the user's in a DM.
fn interaction_user(d: &Value) -> &Value {
    if d["member"]["user"].is_object() { &d["member"]["user"] } else { &d["user"] }
}

/// A message, new or `edited`, in channel `ch`.
fn parse_message(m: &Value, edited: bool, bot: &Bot, allowed: &[String], ch: &Channel) -> Parsed {
    let (Some(channel), Some(user)) = (m["channel_id"].as_str(), m["author"]["id"].as_str()) else {
        return Parsed::Ignore; // an edit that only unfurled a link, ...
    };
    if m["author"]["bot"] == true || user == bot.id {
        return Parsed::Ignore;
    }
    let files = attachments(m);
    let Some(text) = m["content"].as_str().filter(|t| !t.is_empty() || !files.is_empty()) else {
        return Parsed::Ignore; // stickers, joins, pins, ...
    };

    let tags = [format!("<@{}>", bot.id), format!("<@!{}>", bot.id)];
    let mentioned = tags.iter().any(|t| text.contains(t.as_str()));
    let cleaned = tags.iter().fold(text.to_string(), |t, tag| t.replace(tag.as_str(), "")).trim().to_string();
    let replied_to_bot = m["referenced_message"]["author"]["id"].as_str() == Some(&bot.id);
    let addressed = ch.kind == DM || mentioned || replied_to_bot || ch.mine;
    let command = files.is_empty().then(|| parse_command(&cleaned, Some(&bot.username))).flatten();
    let id = m["id"].as_str().unwrap_or("").to_string();
    if !allowed.iter().any(|a| a == user) {
        // Strangers chatting in a server aren't turned away, only ignored.
        let deny = !edited && (addressed || command.is_some());
        return if deny { Parsed::Deny(channel.into(), id, user.into(), display_name(&m["author"])) } else { Parsed::Ignore };
    }

    let kind = match command {
        Some(_) if edited => return Parsed::Ignore,
        Some((name, args)) => InboundKind::Command { name, args },
        None if edited => InboundKind::Edited { id, text: cleaned },
        None => {
            let r = &m["referenced_message"];
            let reply_to = r["id"].as_str().map(|id| Quote {
                id: id.to_string(),
                text: r["content"].as_str().unwrap_or("").chars().take(1000).collect(),
                mine: r["author"]["id"].as_str() == Some(&bot.id),
            });
            InboundKind::Message { id, text: cleaned, files, reply_to, addressed }
        }
    };
    Parsed::Event(Event {
        thread: channel.into(),
        place: ch.place.clone(),
        user: User { id: user.into(), name: display_name(&m["author"]) },
        kind,
    })
}

/// A reaction added or `removed` (taken back) in channel `ch`.
fn parse_reaction(r: &Value, removed: bool, bot: &Bot, allowed: &[String], ch: &Channel) -> Parsed {
    let (Some(channel), Some(message), Some(user)) = (r["channel_id"].as_str(), r["message_id"].as_str(), r["user_id"].as_str()) else {
        return Parsed::Ignore;
    };
    if user == bot.id || !allowed.iter().any(|a| a == user) {
        return Parsed::Ignore;
    }
    let emoji = if removed { String::new() } else { r["emoji"]["name"].as_str().unwrap_or("").to_string() };
    let u = &r["member"]["user"];
    let name = if u.is_object() { display_name(u) } else { user.to_string() };
    Parsed::Event(Event {
        thread: channel.into(),
        place: ch.place.clone(),
        user: User { id: user.into(), name },
        kind: InboundKind::Reaction { message: message.into(), emoji },
    })
}

/// Files attached to a message, by their content type; a recorded voice message is a voice
/// note. The handle is the file's url.
fn attachments(m: &Value) -> Vec<Attachment> {
    let voice = m["flags"].as_u64().is_some_and(|f| f & 1 << 13 != 0);
    let list = m["attachments"].as_array().map(Vec::as_slice).unwrap_or_default();
    list.iter()
        .filter_map(|a| {
            let mime = a["content_type"].as_str().map(|m| m.split(';').next().unwrap_or(m).trim().to_string());
            let kind = match mime.as_deref().unwrap_or("") {
                _ if voice => FileKind::Voice,
                m if m.starts_with("image/") => FileKind::Image,
                m if m.starts_with("audio/") => FileKind::Audio,
                m if m.starts_with("video/") => FileKind::Video,
                _ => FileKind::Document,
            };
            Some(Attachment {
                id: a["url"].as_str()?.to_string(),
                kind,
                name: a["filename"].as_str().map(str::to_string),
                mime,
                size: a["size"].as_u64(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------- messenger

/// The bot as connected now: its API, who it is and who may talk to it.
#[derive(Default)]
struct Discord {
    api: RwLock<Option<Api>>,
    bot: RwLock<Option<Bot>>,
    allowed: RwLock<Vec<String>>,
    /// "typing…" kept up in these channels while August works.
    typing: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// The gateway and what handles its events.
    gateway: Mutex<Option<tokio::task::AbortHandle>>,
    /// How the gateway went last: when Discord last answered, and the error since, if any.
    seen: Mutex<Option<(Instant, Option<String>)>>,
    /// While pairing: where a message from someone not yet allowed goes.
    pairing: Mutex<Option<mpsc::UnboundedSender<(String, String)>>>,
    /// Channels by id: the servers' from the gateway as they come and change, any other
    /// looked up once.
    channels: Mutex<HashMap<String, Channel>>,
    /// The DM channel with each user, by user id.
    dms: Mutex<HashMap<String, String>>,
    /// August's own reaction on recent messages, to take back when it changes.
    reacted: Mutex<VecDeque<(String, String)>>,
}

impl Discord {
    fn api(&self) -> Result<Api> {
        self.api.read().unwrap().clone().ok_or_else(|| anyhow!("Discord is not connected: /login discord"))
    }

    fn bot(&self) -> Result<Bot> {
        self.bot.read().unwrap().clone().ok_or_else(|| anyhow!("Discord is not connected: /login discord"))
    }

    fn seen(&self, error: Option<String>) {
        *self.seen.lock().unwrap() = Some((Instant::now(), error));
    }

    /// Channel `id`, looked up once.
    async fn channel(&self, id: &str) -> Channel {
        if let Some(c) = self.channels.lock().unwrap().get(id) {
            return c.clone();
        }
        let (Ok(api), Ok(bot)) = (self.api(), self.bot()) else { return Channel::default() };
        match api.get(&format!("/channels/{id}")).await {
            Ok(c) => {
                let c = channel_of(&c, &bot);
                self.channels.lock().unwrap().insert(id.to_string(), c.clone());
                c
            }
            Err(e) => {
                eprintln!("could not look up channel {id}: {e:#}");
                Channel { kind: 0, place: Place { kind: PlaceKind::Group, ..Default::default() }, mine: false }
            }
        }
    }

    /// The channel event `d` happened in: a DM unless it names a server.
    async fn channel_for(&self, d: &Value) -> Channel {
        match d["guild_id"].as_str() {
            None => Channel { kind: DM, ..Default::default() },
            Some(_) => self.channel(d["channel_id"].as_str().unwrap_or("")).await,
        }
    }

    /// The DM channel with `user`, opened once.
    async fn dm(&self, user: &str) -> Result<String> {
        if let Some(c) = self.dms.lock().unwrap().get(user) {
            return Ok(c.clone());
        }
        let c = self.api()?.call(Method::POST, "/users/@me/channels", Some(json!({"recipient_id": user}))).await?;
        let id = c["id"].as_str().context("no channel id")?.to_string();
        self.dms.lock().unwrap().insert(user.to_string(), id.clone());
        Ok(id)
    }

    /// Answers interaction `d` within the 3 s Discord gives, ahead of the events before it.
    async fn answer(&self, d: &Value) {
        let allowed = self.allowed.read().unwrap().clone();
        let (Some(body), Some(id), Some(token), Ok(api)) = (ack(d, &allowed), d["id"].as_str(), d["token"].as_str(), self.api()) else {
            return;
        };
        if let Err(e) = api.call(Method::POST, &format!("/interactions/{id}/{token}/callback"), Some(body)).await {
            eprintln!("could not answer an interaction: {e:#}");
        }
    }

    /// A slash command or a button press of an allowed user (already answered).
    async fn interaction(&self, d: &Value, allowed: &[String]) -> Option<Event> {
        let user = interaction_user(d);
        let uid = user["id"].as_str().filter(|u| allowed.iter().any(|a| a == u))?;
        let channel = d["channel_id"].as_str()?;
        let kind = match d["type"].as_u64() {
            Some(2) => {
                let (name, args) = command_of(&d["data"])?;
                InboundKind::Command { name, args }
            }
            Some(3) => InboundKind::Press { button: d["data"]["custom_id"].as_str()?.to_string() },
            _ => return None,
        };
        let place = match d["guild_id"].as_str() {
            None => Place::default(),
            Some(_) => self.channel(channel).await.place,
        };
        Some(Event { thread: channel.into(), place, user: User { id: uid.into(), name: display_name(user) }, kind })
    }

    fn remember(&self, c: &Value, bot: &Bot) {
        if let Some(id) = c["id"].as_str() {
            self.channels.lock().unwrap().insert(id.to_string(), channel_of(c, bot));
        }
    }

    /// What `t` from the gateway means for August.
    async fn handle(&self, august: &August, t: &str, d: Value) {
        let Ok(bot) = self.bot() else { return };
        let allowed = self.allowed.read().unwrap().clone();
        let parsed = match t {
            // Channels as the gateway tells them: no lookups, and renames are seen.
            "GUILD_CREATE" => {
                let threads = d["threads"].as_array().into_iter().flatten();
                for c in d["channels"].as_array().into_iter().flatten().chain(threads) {
                    self.remember(c, &bot);
                }
                return;
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "THREAD_CREATE" | "THREAD_UPDATE" => return self.remember(&d, &bot),
            "CHANNEL_DELETE" | "THREAD_DELETE" => {
                self.channels.lock().unwrap().remove(d["id"].as_str().unwrap_or(""));
                return;
            }
            "MESSAGE_CREATE" | "MESSAGE_UPDATE" => {
                parse_message(&d, t == "MESSAGE_UPDATE", &bot, &allowed, &self.channel_for(&d).await)
            }
            "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
                parse_reaction(&d, t == "MESSAGE_REACTION_REMOVE", &bot, &allowed, &self.channel_for(&d).await)
            }
            "INTERACTION_CREATE" => self.interaction(&d, &allowed).await.map_or(Parsed::Ignore, Parsed::Event),
            _ => return,
        };
        let event = match parsed {
            Parsed::Ignore => return,
            Parsed::Event(e) => e,
            Parsed::Deny(channel, message, user, name) => {
                let pairing = self.pairing.lock().unwrap().clone();
                if let Some(tx) = pairing {
                    tx.send((user, name)).ok();
                    return;
                }
                eprintln!("rejected user {user}");
                let body = json!({"content": denied(&user), "message_reference": {"message_id": message, "fail_if_not_exists": false}});
                if let Ok(api) = self.api() {
                    api.call(Method::POST, &format!("/channels/{channel}/messages"), Some(body)).await.ok();
                }
                return;
            }
        };
        eprintln!("{} ({}): {:?}", event.user.name, event.user.id, event.kind);
        let thread = Thread { messenger: ID.into(), id: event.thread };
        if let Err(e) = august.inbound(&thread, &event.place, &event.user, &event.kind).await {
            eprintln!("could not hand in a message: {e:#}");
        }
    }
}

/// A slash command's name and its `args`.
fn command_of(data: &Value) -> Option<(String, String)> {
    let options = data["options"].as_array().map(Vec::as_slice).unwrap_or_default();
    let args = options.iter().find(|o| o["name"] == "args").and_then(|o| o["value"].as_str()).unwrap_or("");
    Some((data["name"].as_str()?.to_string(), args.trim().to_string()))
}

/// The answer to interaction `d`: a stranger is told off (only they see it), a command shows
/// as typed (August answers it in the channel), a press is confirmed.
fn ack(d: &Value, allowed: &[String]) -> Option<Value> {
    let uid = interaction_user(d)["id"].as_str()?;
    if !allowed.iter().any(|a| a == uid) {
        return Some(json!({"type": 4, "data": {"content": denied(uid), "flags": 64}}));
    }
    match d["type"].as_u64() {
        Some(2) => {
            let (name, args) = command_of(&d["data"])?;
            let shown = if args.is_empty() { format!("`/{name}`") } else { format!("`/{name} {args}`") };
            Some(json!({"type": 4, "data": {"content": shown, "allowed_mentions": {"parse": []}}}))
        }
        Some(3) => Some(json!({"type": 6})),
        _ => None,
    }
}

fn denied(user: &str) -> String {
    format!("⛔ You are not authorized to use this bot.\nYour Discord user id: {user}\nThe owner can add you with /login discord.")
}

fn clip(text: &str) -> String {
    text.chars().take(MAX_LEN).collect()
}

/// `rows` as message components: rows kept (longer ones broken up), unless there are more
/// than fit; then all the buttons, packed.
fn components(rows: &[Vec<Button>]) -> Value {
    let mut rows: Vec<Vec<&Button>> = rows.iter().flat_map(|r| r.chunks(ROW_LEN).map(|c| c.iter().collect())).collect();
    if rows.len() > ROWS {
        rows = rows.concat().chunks(ROW_LEN).map(<[_]>::to_vec).collect();
    }
    rows.truncate(ROWS);
    let row = |r: &Vec<&Button>| {
        let buttons: Vec<Value> =
            r.iter().map(|b| json!({"type": 2, "style": 2, "label": b.label.chars().take(MAX_LABEL).collect::<String>(), "custom_id": b.id})).collect();
        json!({"type": 1, "components": buttons})
    };
    Value::Array(rows.iter().map(row).collect())
}

/// An emoji as a reaction's path takes it: a custom one (`<:name:id>`) as `name:id`.
fn reaction_emoji(emoji: &str) -> String {
    let inner = emoji.strip_prefix('<').and_then(|e| e.strip_suffix('>'));
    match inner {
        Some(e) => e.trim_start_matches('a').trim_start_matches(':').to_string(),
        None => emoji.to_string(),
    }
}

/// A command's name as Discord takes it (1–32 lowercase letters, digits, `-`, `_`).
fn command_name_ok(name: &str) -> bool {
    (1..=32).contains(&name.chars().count()) && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn description() -> Description {
    Description {
        id: ID.into(),
        name: "Discord".into(),
        capabilities: Capabilities {
            markdown: true,
            max_len: MAX_LEN,
            buttons: ROWS * ROW_LEN,
            edit: true,
            // Discord allows about five edits per five seconds in a channel.
            edit_interval_ms: 1000,
            files_in: true,
            files_out: true,
            images: true,
            audio_in: true,
            commands: true,
            presence: true,
            delete: true,
            reactions: true,
            reply: true,
            threads: true,
            open_thread: true,
        },
        notes: "In servers a message is addressed to August when the bot is mentioned or replied to, or is in a \
                thread it opened. Every channel, thread and DM is a thread of its own (its channel id); threads open in \
                text and forum channels (in a forum, as a post). Buttons: up to 5 rows of 5, labels up to 80 characters; \
                more rows are packed. Commands are slash commands with one optional `args` text. August has one reaction \
                per message: a new one replaces it. Voice messages come in as voice notes. Up to 10 files per message; \
                upload limits are Discord's (10 MB per file unless the server is boosted). A DM needs the person to share \
                a server with the bot."
            .into(),
        actions: vec![Action {
            name: "pin".into(),
            description: "Pin a message in the channel (the bot needs Manage Messages or Pin Messages there).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"message": {"type": "string", "description": "the message id"}},
                "required": ["message"]
            }),
        }],
    }
}

#[async_trait]
impl Messenger for Discord {
    async fn threads(&self) -> Vec<String> {
        let allowed = self.allowed.read().unwrap().clone();
        let mut out = Vec::new();
        for user in allowed {
            match self.dm(&user).await {
                Ok(c) => out.push(c),
                Err(e) => eprintln!("no DM with {user}: {e:#}"),
            }
        }
        out
    }

    async fn send(&self, channel: &str, message: &OutMessage) -> Result<String> {
        let api = self.api()?;
        let mut body = json!({
            "content": clip(&message.text),
            "components": components(&message.buttons),
            // No @everyone or role pings from what August writes.
            "allowed_mentions": {"parse": ["users"]},
        });
        if let Some(to) = &message.reply_to {
            body["message_reference"] = json!({"message_id": to, "fail_if_not_exists": false});
        }
        let path = format!("/channels/{channel}/messages");
        if message.files.is_empty() {
            let sent = api.call(Method::POST, &path, Some(body)).await?;
            return Ok(sent["id"].as_str().context("no message id")?.to_string());
        }
        // The text and buttons go with the first files, the rest of them after it.
        let mut first = None;
        for chunk in message.files.chunks(MAX_FILES) {
            let mut files = Vec::new();
            for path in chunk {
                let bytes = tokio::fs::read(path).await.with_context(|| format!("read {}", path.display()))?;
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
                files.push((name, bytes));
            }
            let sent = api.upload(Method::POST, &path, &body, &files).await?;
            first.get_or_insert(sent["id"].as_str().context("no message id")?.to_string());
            body = json!({});
        }
        Ok(first.unwrap_or_default())
    }

    async fn edit(&self, channel: &str, id: &str, message: &OutMessage) -> Result<()> {
        // Empty components remove the buttons.
        let body = json!({"content": clip(&message.text), "components": components(&message.buttons)});
        self.api()?.call(Method::PATCH, &format!("/channels/{channel}/messages/{id}"), Some(body)).await.map(drop)
    }

    async fn presence(&self, channel: &str, busy: bool) {
        let mut typing = self.typing.lock().unwrap();
        if let Some(old) = typing.remove(channel) {
            old.abort();
        }
        let Ok(api) = self.api() else { return };
        if busy {
            // Discord shows "typing…" for about ten seconds per call.
            let path = format!("/channels/{channel}/typing");
            let task = tokio::spawn(async move {
                loop {
                    api.call(Method::POST, &path, None).await.ok();
                    tokio::time::sleep(Duration::from_secs(8)).await;
                }
            });
            typing.insert(channel.to_string(), task.abort_handle());
        }
    }

    async fn delete(&self, channel: &str, id: &str) -> Result<()> {
        self.api()?.call(Method::DELETE, &format!("/channels/{channel}/messages/{id}"), None).await.map(drop)
    }

    async fn react(&self, channel: &str, id: &str, emoji: &str) -> Result<()> {
        let api = self.api()?;
        let emoji = reaction_emoji(emoji);
        let base = format!("/channels/{channel}/messages/{id}/reactions");
        let old = {
            let mut reacted = self.reacted.lock().unwrap();
            let at = reacted.iter().position(|(m, _)| m == id);
            at.and_then(|i| reacted.remove(i)).map(|(_, e)| e)
        };
        // One reaction of August's at a time: a new one replaces the old.
        if let Some(old) = old.filter(|o| *o != emoji) {
            api.call(Method::DELETE, &format!("{base}/{}/@me", segment(&old)), None).await.ok();
        }
        if emoji.is_empty() {
            return Ok(());
        }
        api.call(Method::PUT, &format!("{base}/{}/@me", segment(&emoji)), None).await?;
        let mut reacted = self.reacted.lock().unwrap();
        reacted.push_back((id.to_string(), emoji));
        if reacted.len() > REACTED_KEPT {
            reacted.pop_front();
        }
        Ok(())
    }

    async fn set_commands(&self, commands: &[CommandSpec]) -> Result<()> {
        let list: Vec<Value> = commands
            .iter()
            .filter(|c| command_name_ok(&c.name))
            .map(|c| {
                let about: String = c.description.chars().take(100).collect();
                json!({
                    "name": c.name,
                    "type": 1,
                    "description": if about.trim().is_empty() { c.name.clone() } else { about },
                    "options": [{"type": 3, "name": "args", "description": "arguments", "required": false}],
                })
            })
            .collect();
        let app = self.bot()?.app;
        self.api()?.call(Method::PUT, &format!("/applications/{app}/commands"), Some(Value::Array(list))).await.map(drop)
    }

    async fn open_thread(&self, parent: &str, title: &str) -> Result<String> {
        let name: String = title.chars().take(100).collect();
        // A forum's thread is a post: it needs a first message.
        let body = if FORUMS.contains(&self.channel(parent).await.kind) {
            json!({"name": name, "message": {"content": name}})
        } else {
            json!({"name": name, "type": 11})
        };
        let thread = self.api()?.call(Method::POST, &format!("/channels/{parent}/threads"), Some(body)).await?;
        let id = thread["id"].as_str().context("no thread id")?.to_string();
        self.channels.lock().unwrap().insert(id.clone(), channel_of(&thread, &self.bot()?));
        Ok(id)
    }

    async fn action(&self, channel: &str, name: &str, args: Value) -> Result<Value> {
        match name {
            "pin" => {
                let message = args["message"].as_str().filter(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_digit()));
                let message = message.context("bad message id")?;
                self.api()?.call(Method::PUT, &format!("/channels/{channel}/messages/pins/{message}"), None).await?;
                Ok(Value::Null)
            }
            other => bail!("Discord has no action `{other}`"),
        }
    }

    async fn download(&self, file: &Attachment) -> Result<Vec<u8>> {
        if !file.id.starts_with("https://") && !file.id.starts_with("http://") {
            bail!("not a Discord attachment");
        }
        self.api()?.download(&file.id).await
    }
}

// ---------------------------------------------------------------- connection

/// The saved token (or `DISCORD_BOT_TOKEN`).
async fn token(august: &August) -> Result<Option<String>> {
    Ok(match august.env("DISCORD_BOT_TOKEN") {
        Some(t) => Some(t),
        None => august.secret("token").await?,
    })
}

/// The allowed user ids: `DISCORD_ALLOWED_USERS`, else the setting.
async fn allowed(august: &August) -> Result<Vec<String>> {
    if let Some(list) = august.env("DISCORD_ALLOWED_USERS") {
        return Ok(list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect());
    }
    Ok(serde_json::from_value(august.settings().await?["allowed"].clone()).unwrap_or_default())
}

/// Connects with `token`: checks it, offers the messenger and opens the gateway (in place
/// of a previous connection).
async fn connect(august: &August, dc: &Arc<Discord>, token: &str) -> Result<Bot> {
    let api = Api::new(august.env("DISCORD_API_BASE").as_deref(), token);
    let me = api.get("/users/@me").await.context("Discord rejected the bot token")?;
    let app = api.get("/applications/@me").await.context("could not read the bot's application")?;
    let bot = Bot {
        id: me["id"].as_str().unwrap_or("").to_string(),
        username: me["username"].as_str().unwrap_or("").to_string(),
        app: app["id"].as_str().unwrap_or("").to_string(),
    };
    *dc.allowed.write().unwrap() = allowed(august).await?;
    if dc.allowed.read().unwrap().is_empty() {
        eprintln!("no allowed users, nobody can talk to the bot (/login discord pairs one)");
    }
    *dc.api.write().unwrap() = Some(api.clone());
    *dc.bot.write().unwrap() = Some(bot.clone());
    dc.seen(None);

    let (a, d) = (august.clone(), dc.clone());
    let task = tokio::spawn(async move {
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Value)>();
        // Events are handled in order, apart from the gateway, which keeps heartbeating;
        // interactions are answered as they come, not after what is ahead of them.
        let handler = async {
            while let Some((t, v)) = rx.recv().await {
                d.handle(&a, &t, v).await;
            }
        };
        let gateway = async {
            // The queue closes with the gateway, and the handler with it.
            let tx = tx;
            let on = |t: String, v: Value| {
                if t == "INTERACTION_CREATE" {
                    let (d, v) = (d.clone(), v.clone());
                    tokio::spawn(async move { d.answer(&v).await });
                }
                tx.send((t, v)).ok();
            };
            let why = gateway::run(&api, on, |e| d.seen(e)).await;
            eprintln!("{why}");
        };
        tokio::join!(gateway, handler);
    });
    if let Some(old) = dc.gateway.lock().unwrap().replace(task.abort_handle()) {
        old.abort();
    }
    august.register_messenger(description(), dc.clone());
    eprintln!("connected as @{}", bot.username);
    Ok(bot)
}

/// `/login discord`: the bot token (kept, or a new one), then pairing: the person to allow
/// messages the bot and is confirmed.
async fn login(august: &August, dc: &Arc<Discord>, steps: Login) -> Result<Signed> {
    let saved = token(august).await?;
    let keep = match &saved {
        Some(_) => steps.choose("A bot token is already saved. Keep it?", &["Keep", "New token"]).await? == "Keep",
        None => false,
    };
    let token = match saved.filter(|_| keep) {
        Some(t) => t,
        None => {
            steps
                .progress(
                    "1. Open https://discord.com/developers/applications and create an application\n\
                     2. Under Bot: turn on Message Content Intent, then Reset Token and copy it",
                )
                .await?;
            steps.ask("Bot token", true).await?.trim().to_string()
        }
    };
    let bot = connect(august, dc, &token).await?;
    august.set_secret("token", Some(&token)).await?;
    let who = format!("@{}", bot.username);
    let mut allowed = dc.allowed.read().unwrap().clone();
    if keep && !allowed.is_empty() && steps.choose("Allow one more person to talk to August?", &["Yes", "No"]).await? != "Yes" {
        return Ok(Signed { who, expires_at: None });
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    *dc.pairing.lock().unwrap() = Some(tx);
    let invite = format!(
        "https://discord.com/oauth2/authorize?client_id={}&scope=bot%20applications.commands&permissions={PERMISSIONS}",
        bot.app
    );
    steps
        .progress(&format!(
            "Add the bot to your server: {invite}\nThen send it a direct message, or mention it in a channel (waiting up to 3 min)…"
        ))
        .await?;
    let paired = tokio::time::timeout(PAIR_WAIT, async {
        while let Some((id, name)) = rx.recv().await {
            if steps.choose(&format!("Allow {name} (id {id}) to control August?"), &["Allow", "Skip"]).await? == "Allow" {
                return Ok(Some(id));
            }
            steps.progress("Skipped, waiting for another message…").await?;
        }
        Ok::<_, anyhow::Error>(None)
    })
    .await;
    dc.pairing.lock().unwrap().take();
    let user = paired.ok().transpose()?.flatten().ok_or_else(|| anyhow!("no message came; run /login discord again"))?;
    if !allowed.contains(&user) {
        allowed.push(user.clone());
    }
    august.call("config_set", json!({"path": "extensions.discord.settings.allowed", "value": allowed})).await?;
    *dc.allowed.write().unwrap() = allowed;
    if let Ok(dm) = dc.dm(&user).await {
        let body = json!({"content": "✅ August is connected. Message me here."});
        dc.api()?.call(Method::POST, &format!("/channels/{dm}/messages"), Some(body)).await.ok();
    }
    Ok(Signed { who, expires_at: None })
}

/// The gateway stopped while connected: Discord refused the bot (why is in the detail). No
/// answer from Discord past two heartbeats: the network or Discord, which a restart won't fix.
fn health(dc: &Discord) -> Value {
    if dc.api.read().unwrap().is_none() {
        return json!({"status": "ok", "detail": "not connected"});
    }
    let seen = dc.seen.lock().unwrap().clone();
    if dc.gateway.lock().unwrap().as_ref().is_some_and(|g| g.is_finished()) {
        let why = seen.and_then(|(_, e)| e).unwrap_or_else(|| "the gateway stopped".into());
        return json!({"status": "failed", "detail": why});
    }
    match seen {
        Some((at, Some(error))) if at.elapsed() > Duration::from_secs(90) => {
            json!({"status": "degraded", "detail": format!("no answer from Discord for {} s: {error}", at.elapsed().as_secs())})
        }
        _ => json!({"status": "ok"}),
    }
}

/// `/logout discord`: forgets the token and stops the bot (allowed users stay).
async fn logout(august: &August, dc: &Discord) -> Result<()> {
    august.set_secret("token", None).await?;
    if let Some(task) = dc.gateway.lock().unwrap().take() {
        task.abort();
    }
    dc.api.write().unwrap().take();
    dc.bot.write().unwrap().take();
    august.unregister_messenger(ID);
    Ok(())
}

#[tokio::main]
async fn main() {
    serve(August::new()).await;
}

async fn serve(august: August) {
    august.describe(
        "The Discord messenger",
        "Connects the bot to August: DMs, channels and threads become threads, with buttons, slash commands, files, voice messages, reactions and edits.",
    );
    august.needs(&["config"]);
    august.settings_schema(json!({"properties": {
        "allowed": {"type": "array", "items": {"type": "string"}, "default": [], "description": "Discord user ids allowed to talk to the bot (/login discord pairs one)"},
    }}));
    let dc = Arc::new(Discord::default());
    let d = dc.clone();
    august.health(move || {
        let d = d.clone();
        async move { Ok(health(&d)) }
    });
    let (a, d) = (august.clone(), dc.clone());
    let (b, e) = (august.clone(), dc.clone());
    august.register_login_account(
        ID,
        "Discord (bot)",
        &[],
        move |_, steps| {
            let (a, d) = (a.clone(), d.clone());
            async move { login(&a, &d, steps).await }
        },
        move |_| {
            let (a, d) = (b.clone(), e.clone());
            async move { logout(&a, &d).await }
        },
    );
    let (a, d) = (august.clone(), dc.clone());
    tokio::spawn(async move {
        match token(&a).await {
            Ok(Some(token)) => match connect(&a, &d, &token).await {
                // August may not list this extension yet: tell it once it does.
                Ok(bot) => {
                    for _ in 0..30 {
                        if a.account_update(ID, "connected", Some(&format!("@{}", bot.username))).await.is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
                Err(e) => eprintln!("{e:#}"),
            },
            Ok(None) => eprintln!("not connected: /login discord"),
            Err(e) => eprintln!("{e:#}"),
        }
    });
    august.run().await;
}

#[cfg(test)]
mod tests;
