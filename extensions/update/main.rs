//! `update`: `/update`, the `update` tool and `august update` bring August up to date. First
//! the core and its default extensions, from the latest release (the installer ships them
//! together, so they always match); then every enabled extension installed from git, the way
//! `extend` updates one. When the core or the defaults changed, August is restarted to run
//! them: from a chat a few seconds later, in a terminal after asking.

#[path = "../extend/git.rs"]
mod git;
#[cfg(test)]
mod tests;

use anyhow::bail;
use august_ext::August;
use august_ext::client::Client;
use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const INSTALLER: &str = "curl -fsSL https://raw.githubusercontent.com/daz9e/august-agent/main/scripts/install.sh | sh -s -- --no-prompt";

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let bin = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
    if std::env::args().nth(1).as_deref() == Some("run") {
        // `august update`: here in the terminal, reaching the core over its socket.
        let client = tokio::sync::Mutex::new(Client::from_env().await?);
        let call = |op, params| {
            let client = &client;
            async move { client.lock().await.call(op, params).await }
        };
        let home = match std::env::var("AUGUST_HOME") {
            Ok(home) => PathBuf::from(home),
            Err(_) => PathBuf::from(std::env::var("HOME")?).join(".august"),
        };
        let (report, changed) = update(&call, &home, &home.join("extensions"), &bin, INSTALLER).await?;
        println!("{report}");
        if changed {
            print!("Restart August now to run the new version? [Y/n] ");
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if answer.trim().to_lowercase().starts_with('n') {
                println!("Pick up the new version with: august restart");
            } else {
                std::process::Command::new(bin.join("august")).arg("restart").status()?;
            }
        }
        return Ok(());
    }
    serve(August::new(), bin, INSTALLER.into()).await;
    Ok(())
}

/// `bin`: where `august` and the default extensions are; `installer`: the shell command that
/// installs the latest release into `$AUGUST_INSTALL_DIR`.
async fn serve(august: August, bin: PathBuf, installer: String) {
    august.needs(&["admin"]);
    august.describe(
        "/update, `update` and `august update`: update the core, the default extensions and those installed from git",
        "Installs the latest release of the core and its default extensions, updates every enabled \
         extension installed from git, and restarts August when the core or the defaults changed.",
    );
    let exe = bin.join("august-ext-update").display().to_string();
    august.register_cli("update", "Update the core, the default extensions and those installed from git", &[exe, "run".into()]);
    let run = move |august: August| {
        let (bin, installer) = (bin.clone(), installer.clone());
        async move {
            let home = PathBuf::from(august.env("AUGUST_HOME").unwrap_or_default());
            let root = PathBuf::from(august.env("AUGUST_EXTENSIONS").unwrap_or_default());
            let (mut report, changed) = update(&|op, params| august.call(op, params), &home, &root, &bin, &installer).await?;
            if changed {
                // Later, from its own process group, so this reply gets out before the restart ends us.
                let august_bin = bin.join("august").display().to_string().replace('\'', r"'\''");
                tokio::process::Command::new("sh")
                    .args(["-c", &format!("sleep 5; '{august_bin}' restart")])
                    .process_group(0)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()?;
                report += "\nAugust restarts in a few seconds to run the new version.";
            }
            Ok(report)
        }
    };
    let (me, go) = (august.clone(), run.clone());
    august.register_tool(
        "update",
        "Update August: the core and its default extensions to the latest release, then the \
         extensions installed from git. August restarts by itself afterwards if needed.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
        move |_, _| go(me.clone()),
    );
    let me = august.clone();
    august.register_command("update", "Update the core, the default extensions and those installed from git", move |_, _| {
        let fut = run(me.clone());
        async move { fut.await.map(Some) }
    });
    august.run().await;
}

/// Installs the latest release into `bin` (unless this is a checkout) and updates the enabled
/// extensions in `root` installed from git: the report, and whether the core or the defaults
/// changed (and August needs a restart).
async fn update<C, R>(call: &C, home: &Path, root: &Path, bin: &Path, installer: &str) -> anyhow::Result<(String, bool)>
where
    C: Fn(&'static str, Value) -> R,
    R: Future<Output = anyhow::Result<Value>>,
{
    // 1, 2. The core and the default extensions: one release.
    let mut report = Vec::new();
    let mut changed = false;
    if !bin.join("august").is_file() || bin.join("../../Cargo.toml").is_file() {
        report.push(format!("core and default extensions: built from a checkout ({}), update it with git", bin.display()));
    } else {
        let before = fingerprint(bin);
        let dir = bin.display().to_string().replace('\'', r"'\''");
        let out = august_ext::sh(&format!("export AUGUST_INSTALL_DIR='{dir}'; {installer}"), bin, Duration::from_secs(540)).await?;
        if !out.status.success() {
            let said = [out.stdout, out.stderr].concat();
            bail!("the installer failed, nothing was updated:\n{}", String::from_utf8_lossy(&said).trim());
        }
        changed = fingerprint(bin) != before;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let versions: Vec<&str> = stdout.lines().map(str::trim).filter(|l| l.starts_with("august-")).collect();
        report.push(format!("core and default extensions: {} ({})", if changed { "updated" } else { "up to date" }, versions.join(", ")));
    }

    // 3. Extensions installed from git.
    let list = call("extensions", json!({})).await?;
    for e in list.as_array().into_iter().flatten().filter(|e| e["state"] != "disabled") {
        let Some(name) = e["name"].as_str().filter(|n| root.join(n).join(".git").exists()) else { continue };
        report.push(git::update(call, home, root, name).await.unwrap_or_else(|e| format!("`{name}` failed: {e:#}")));
    }
    Ok((report.join("\n"), changed))
}

/// A hash of `august` and the default extensions' binaries, to see whether an install changed them.
fn fingerprint(bin: &Path) -> u64 {
    let mut files: Vec<PathBuf> = std::fs::read_dir(bin).into_iter().flatten().flatten().map(|e| e.path()).collect();
    files.retain(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "august" || n.starts_with("august-ext-")));
    files.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for f in files {
        (f.file_name(), std::fs::read(&f).ok()).hash(&mut h);
    }
    h.finish()
}
