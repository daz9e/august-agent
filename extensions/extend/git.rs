//! Starting an extension and updating one installed from git. `extend` uses it, and so does
//! `update`, also as `august update` in a terminal: `call` runs one of the core's operations
//! (through the extension, or over the socket `august` hands a command).

use anyhow::bail;
use serde_json::{Value, json};
use std::path::Path;

/// Starts `name` (set up first if it is new or changed), or says why it didn't with its log
/// (under `home`, August's home). One that doesn't describe itself is turned off again.
pub async fn start<C, R>(call: &C, home: &Path, name: &str) -> anyhow::Result<String>
where
    C: Fn(&'static str, Value) -> R,
    R: Future<Output = anyhow::Result<Value>>,
{
    let status = match call("extension_enable", json!({"name": name})).await {
        Ok(status) => status,
        Err(e) => {
            let log = call("extension_logs", json!({"name": name, "lines": 30})).await.unwrap_or_default();
            let path = home.join(format!("logs/extensions/{name}.log"));
            bail!("it failed to start:\n{e:#}\n\nIts log ({}):\n{}", path.display(), log.as_str().unwrap_or_default());
        }
    };
    let list = call("extensions", json!({})).await?;
    let undescribed = list.as_array().into_iter().flatten().any(|e| e["name"] == name && e["summary"].as_str().unwrap_or_default().is_empty());
    if undescribed {
        call("extension_disable", json!({"name": name})).await?;
        bail!("it doesn't describe itself, so it was turned off: give it a summary (`august.describe(summary, details)`, or `summary` in its ready message)");
    }
    Ok(status.as_str().unwrap_or_default().to_string())
}

/// Pulls `name` in `root` (August's extensions) and restarts it if it changed.
pub async fn update<C, R>(call: &C, home: &Path, root: &Path, name: &str) -> anyhow::Result<String>
where
    C: Fn(&'static str, Value) -> R,
    R: Future<Output = anyhow::Result<Value>>,
{
    let folder = root.join(name);
    if !folder.join(".git").exists() {
        bail!("`{name}` wasn't installed from git, so there is nothing to update it from");
    }
    let before = git(&folder, &["rev-parse", "--short", "HEAD"]).await?;
    git(&folder, &["pull", "--ff-only"]).await?;
    let after = git(&folder, &["rev-parse", "--short", "HEAD"]).await?;
    if before == after {
        return Ok(format!("`{name}` is up to date ({after})"));
    }
    let status = start(call, home, name).await.map_err(|e| anyhow::anyhow!("updated `{name}` to {after}, but {e:#}"))?;
    Ok(format!("updated `{name}` {before} → {after} and restarted it.\n{status}"))
}

/// Runs git in `dir` without ever prompting; its trimmed output, or its error.
pub async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("git").args(args).current_dir(dir).env("GIT_TERMINAL_PROMPT", "0").output().await?;
    if !out.status.success() {
        bail!("git {} failed:\n{}", args[0], String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
