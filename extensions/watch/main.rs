//! `watch`: restarts extensions whose files changed and tells the home thread, in one message. It looks at
//! every extension the core lists that isn't disabled: a user extension's folder in
//! `$AUGUST_EXTENSIONS`, a default one's `august-ext-<name>` binary next to this one. Those
//! installed from git are pulled now and then, so an update upstream lands the same way.

use august_ext::August;
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ME: &str = "watch";
/// How often git checkouts are pulled, in looks.
const PULL_EVERY: u32 = 1200;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let exe = std::env::current_exe().unwrap_or_default();
    serve(August::new(), Duration::from_secs(3), exe.parent().unwrap_or(Path::new(".")).to_path_buf()).await;
}

async fn serve(august: August, every: Duration, bin: PathBuf) {
    august.needs(&["admin", "messaging"]);
    august.describe(
        "Restarts an extension when its files change, and says so in the home thread",
        "Every few seconds it checks the files of each enabled extension (a user extension's \
         folder, a default one's binary). Once changes have settled it restarts those extensions \
         and posts one `🔄 Updated and restarted: ...` message (and why any failed) to the home thread. \
         Extensions installed from git are `git pull --ff-only`ed about once an hour.",
    );
    let dir = PathBuf::from(august.env("AUGUST_EXTENSIONS").unwrap_or_default());
    let a = august.clone();
    tokio::spawn(async move {
        let (mut seen, mut changed) = (HashMap::<String, u64>::new(), BTreeSet::<String>::new());
        for look in 0u32.. {
            let list = a.call("extensions", json!({})).await.unwrap_or_default();
            let names = list.as_array().into_iter().flatten().filter(|e| e["state"] != "disabled").filter_map(|e| e["name"].as_str());
            let mut moving = false;
            for name in names.filter(|n| *n != ME).map(String::from).collect::<Vec<_>>() {
                let folder = dir.join(&name);
                let src = if folder.is_dir() { folder } else { bin.join(format!("august-ext-{name}")) };
                if look > 0 && look % PULL_EVERY == 0 && src.join(".git").exists() {
                    git_pull(&src).await;
                }
                let now = fingerprint(&src);
                if seen.insert(name.clone(), now).is_some_and(|before| before != now) {
                    changed.insert(name);
                    moving = true;
                }
            }
            // ponytail: waits until nothing changed for one look, so a build that relinks
            // every default binary is one restart and one message; a slower writer may restart twice.
            if !moving && !changed.is_empty() {
                restart(&a, std::mem::take(&mut changed)).await;
            }
            tokio::time::sleep(every).await;
        }
    });
    august.run().await;
}

/// Restarts `names` and tells the home thread in one message.
async fn restart(august: &August, names: BTreeSet<String>) {
    let (mut done, mut failed) = (Vec::new(), Vec::new());
    for name in names {
        match august.call("extension_enable", json!({"name": name})).await {
            Ok(_) => done.push(format!("`{name}`")),
            Err(e) => failed.push(format!("⚠️ `{name}` changed, but didn't restart: {e:#}")),
        }
    }
    let mut lines = failed;
    if !done.is_empty() {
        lines.insert(0, format!("🔄 Updated and restarted: {}", done.join(", ")));
    }
    if let Err(e) = august.call("send", json!({"thread": "home", "message": {"text": lines.join("\n")}})).await {
        eprintln!("could not tell the home thread: {e:#}");
    }
}

/// Pulls a git checkout without prompting; a failure is only logged.
async fn git_pull(dir: &Path) {
    let out = tokio::process::Command::new("git").args(["pull", "--ff-only", "-q"]).current_dir(dir).env("GIT_TERMINAL_PROMPT", "0").output().await;
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!("git pull in {} failed: {}", dir.display(), String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => eprintln!("git pull in {} failed: {e}", dir.display()),
    }
}

/// A hash of the size and modification time of every file under `src` (or of `src` itself),
/// skipping what the core skips: hidden folders, `target`, `node_modules`, `__pycache__`.
fn fingerprint(src: &Path) -> u64 {
    fn walk(path: &Path, h: &mut DefaultHasher) {
        let Ok(meta) = std::fs::metadata(path) else { return };
        if meta.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(path).into_iter().flatten().filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().to_string();
                let skip = name.starts_with('.') || ["target", "node_modules", "__pycache__"].contains(&name.as_str());
                if !(skip && e.path().is_dir()) {
                    walk(&e.path(), h);
                }
            }
        } else {
            (path, meta.len(), meta.modified().ok()).hash(h);
        }
    }
    let mut h = DefaultHasher::new();
    walk(src, &mut h);
    h.finish()
}

#[cfg(test)]
mod tests;
