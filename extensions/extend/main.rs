//! `extend`: the agent writes extensions for August itself (`save_extension`); the core
//! starts it and marks it as the agent's. `install_extension` clones one from git instead;
//! `extensions` lists them, turns them on and off, and updates those from git.
//! It brings the TypeScript SDK (`extensions/sdk-ts`): the host that runs an `index.ts` under
//! bun, its types, and the guide, which ships as the `writing-extensions` skill with the
//! wire protocol (`PROTOCOL.md`, a copy of august-core's) for extensions in any language.

mod git;
#[cfg(test)]
mod tests;

use anyhow::bail;
use august_ext::{August, str_arg};
use serde_json::json;
use git::git;
use std::path::{Path, PathBuf};

const HOST: &str = include_str!("../sdk-ts/host.ts");
const TYPES: &str = include_str!("../sdk-ts/august.d.ts");
const GUIDE: &str = include_str!("../sdk-ts/guide.md");
const PROTOCOL: &str = include_str!("PROTOCOL.md");

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve(August::new()).await;
}

async fn serve(august: August) {
    august.needs(&["admin"]);
    let dir = PathBuf::from(august.env("AUGUST_EXTENSIONS").unwrap_or_default());
    let home = dir.display().to_string();
    let skills = dir.join(".runtime/skills/writing-extensions");
    let host = dir.join(".runtime/ts/host.ts");
    if let Err(e) = write_sdk(&host, &dir, &skills) {
        eprintln!("could not write the TypeScript SDK: {e:#}");
    }
    let (me, root) = (august.clone(), dir.clone());
    august.register_tool(
        "save_extension",
        "Create or replace an extension that adds tools, slash commands or hooks to August \
         itself, in any language. Load the `writing-extensions` skill first for the API. \
         `files`: every file by path. With `extension.json` ({command, env, setup}) it runs \
         anything — `setup` steps build it or install what it needs, August runs them; don't \
         build by hand. Without it, `index.ts` runs with the TypeScript SDK; `code` is a \
         shortcut for a lone index.ts. The extension is set up and started right away; the \
         result is what it registered, or the error and its log to fix. It must describe \
         itself (summary).",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "lowercase-with-dashes"},
                "code": {"type": "string", "description": "TypeScript: contents of index.ts"},
                "files": {"type": "object", "additionalProperties": {"type": "string"},
                          "description": "File contents by path inside the extension's folder: extension.json and the rest, or TypeScript modules around index.ts"}
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        move |input, _| {
            let (august, folder, host) = (me.clone(), root.join(str_arg(&input, "name")), host.clone());
            async move {
                let name = str_arg(&input, "name");
                if !valid_name(name) {
                    bail!("extension names use lowercase letters, digits, `-` and `_` (max 64)");
                }
                let text = |(p, c): (&String, &serde_json::Value)| Some((p.clone(), c.as_str()?.to_string()));
                let mut files = match input["files"].as_object().map(|f| f.iter().map(text).collect::<Option<Vec<_>>>()) {
                    Some(Some(files)) => files,
                    Some(None) => bail!("every entry of `files` must be the file's text"),
                    None => vec![],
                };
                if let Some(code) = input["code"].as_str() {
                    files.retain(|(p, _)| p != "index.ts");
                    files.push(("index.ts".into(), code.into()));
                }
                let has = |path: &str| files.iter().any(|(p, _)| p == path);
                if !has("extension.json") {
                    if !has("index.ts") {
                        bail!("give `files` with an `extension.json` that says how to run it, or a TypeScript `index.ts` (`code`)");
                    }
                    let command = json!({"command": ["bun", "run", host.display().to_string(), "index.ts", name]});
                    files.push(("extension.json".into(), serde_json::to_string_pretty(&command)?));
                }
                for (path, _) in &files {
                    let inside = std::path::Path::new(path).components().all(|c| matches!(c, std::path::Component::Normal(_)));
                    if path.is_empty() || !inside {
                        bail!("`{path}`: paths are relative and stay inside the extension's folder");
                    }
                }
                std::fs::create_dir_all(&folder)?;
                for (path, content) in &files {
                    let file = folder.join(path);
                    std::fs::create_dir_all(file.parent().unwrap_or(&folder))?;
                    std::fs::write(&file, content)?;
                }
                let status = start(&august, name).await.map_err(|e| anyhow::anyhow!("saved, but {e:#}"))?;
                let saved: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
                Ok(format!("saved {} in {} and started it.\n{status}", saved.join(", "), folder.display()))
            }
        },
    );
    let (me, root) = (august.clone(), dir.clone());
    august.register_tool(
        "install_extension",
        "Install an extension from a git repository: cloned into August's extensions, set up \
         (its `extension.json` `setup` steps) and started. Update it later with `extensions` \
         action `update`.",
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The repository to clone"},
                "name": {"type": "string", "description": "lowercase-with-dashes; by default the repository's name"}
            },
            "required": ["url"],
            "additionalProperties": false
        }),
        move |input, _| {
            let (august, root) = (me.clone(), root.clone());
            async move {
                let url = str_arg(&input, "url");
                let guess = url.trim_end_matches('/').rsplit(['/', ':']).next().unwrap_or_default().trim_end_matches(".git").to_lowercase();
                let name = match str_arg(&input, "name") {
                    "" => guess.as_str(),
                    name => name,
                };
                if !valid_name(name) {
                    bail!("`{name}` can't be an extension's name: give `name` (lowercase letters, digits, `-` and `_`, max 64)");
                }
                if url.is_empty() || url.starts_with('-') {
                    bail!("`url` must be a git repository");
                }
                let folder = root.join(name);
                if folder.exists() {
                    bail!("`{name}` is already installed in {}; update it with `extensions` action `update`", folder.display());
                }
                git(&root, &["clone", "--depth", "1", "--", url, name]).await?;
                if !folder.join("extension.json").is_file() {
                    std::fs::remove_dir_all(&folder)?;
                    bail!("{url} has no `extension.json` at its root, so it isn't an August extension; nothing was installed");
                }
                let rev = git(&folder, &["rev-parse", "--short", "HEAD"]).await?;
                let status = start(&august, name).await.map_err(|e| anyhow::anyhow!("installed in {}, but {e:#}", folder.display()))?;
                Ok(format!("installed {url} ({rev}) in {} and started it.\n{status}", folder.display()))
            }
        },
    );
    let (me, root) = (august.clone(), dir.clone());
    august.register_tool(
        "extensions",
        "List August's extensions with their state and what they register, enable or disable \
         one by name (disabled stays off across restarts), or update one installed from git.",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "enable", "disable", "update"]},
                "name": {"type": "string", "description": "The extension, for enable, disable and update"}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        move |input, _| {
            let (august, root) = (me.clone(), root.clone());
            async move {
                let (action, name) = (str_arg(&input, "action"), str_arg(&input, "name"));
                match action {
                    "list" => {}
                    "update" if valid_name(name) => {
                        let home = PathBuf::from(august.env("AUGUST_HOME").unwrap_or_default());
                        return git::update(&|op, params| august.call(op, params), &home, &root, name).await;
                    }
                    "enable" | "disable" if !name.is_empty() => {
                        august.call(&format!("extension_{action}"), json!({"name": name})).await?;
                    }
                    "enable" | "disable" | "update" => bail!("`{action}` needs a `name`"),
                    _ => bail!("unknown action `{action}`"),
                }
                Ok(serde_json::to_string(&august.call("extensions", json!({})).await?)?)
            }
        },
    );
    // The model is told which extensions of the user's are running, each turn, so it
    // knows its behavior may come from them (and from one it saved a minute ago).
    let me = august.clone();
    august.on("before_turn", move |data, _| {
        let (august, home) = (me.clone(), home.clone());
        async move {
            let list = august.call("extensions", json!({})).await?;
            let lines: Vec<String> = list
                .as_array()
                .into_iter()
                .flatten()
                .filter(|e| e["state"] == "running" && e["origin"] != "default")
                .map(|e| {
                    let name = e["name"].as_str().unwrap_or_default();
                    if let Some(summary) = e["summary"].as_str().filter(|s| !s.is_empty()) {
                        return format!("- {name}: {summary}");
                    }
                    let parts: Vec<String> = [("tools", ""), ("commands", "/"), ("hooks", "")]
                        .iter()
                        .filter_map(|(key, prefix)| {
                            let names: Vec<String> = e[key].as_array()?.iter().filter_map(|n| Some(format!("{prefix}{}", n.as_str()?))).collect();
                            (!names.is_empty()).then(|| format!("{key}: {}", names.join(", ")))
                        })
                        .collect();
                    format!("- {name} ({})", parts.join("; "))
                })
                .collect();
            if lines.is_empty() {
                return Ok(None);
            }
            let system = format!(
                "{}\n\nExtensions the user or you installed are running and change how you and this chat \
                 behave. When something happens that you didn't do (a message, a new session, a blocked \
                 tool), check them first (`extensions` lists what each does in detail, their code is in {}), not August's source:\n{}",
                data["system"].as_str().unwrap_or_default(),
                home,
                lines.join("\n")
            );
            Ok(Some(json!({"system": system})))
        }
    });
    august.run().await;
}

/// `git::start` through this extension.
async fn start(august: &August, name: &str) -> anyhow::Result<String> {
    let home = PathBuf::from(august.env("AUGUST_HOME").unwrap_or_default());
    git::start(&|op, params| august.call(op, params), &home, name).await
}

/// Writes the TypeScript host and its types at `host`, and the guide as the
/// `writing-extensions` skill in `skills`, with the protocol and the types beside it.
fn write_sdk(host: &Path, dir: &Path, skills: &Path) -> anyhow::Result<()> {
    let rt = host.parent().unwrap_or(dir);
    std::fs::create_dir_all(rt)?;
    std::fs::write(host, HOST)?;
    std::fs::write(rt.join("august.d.ts"), TYPES)?;
    std::fs::create_dir_all(skills)?;
    let about = "How to extend August itself with extensions in any language, with the TypeScript SDK or the bare protocol (tools, slash commands, hooks, several files and processes); read before `save_extension`";
    let guide = GUIDE.replace("{host}", &host.display().to_string());
    let text = format!("---\nname: writing-extensions\ndescription: {about}\n---\nExtensions live in {}.\n\n{guide}", dir.display());
    std::fs::write(skills.join("SKILL.md"), text)?;
    std::fs::write(skills.join("PROTOCOL.md"), PROTOCOL)?;
    std::fs::write(skills.join("august.d.ts"), TYPES)?;
    Ok(())
}
