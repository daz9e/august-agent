//! `extend` against a fake core: it brings the TypeScript host, the guide and the protocol,
//! and an extension saved as TypeScript gets an `extension.json` that runs it through that host.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::{Value, json};

async fn extend() -> (FakeAugust, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let (fake, august) = FakeAugust::new(&[("AUGUST_EXTENSIONS", dir.path().to_str().unwrap())]);
    tokio::spawn(serve(august));
    fake.started().await;
    (fake, dir)
}

#[tokio::test]
async fn a_typescript_extension_runs_through_the_host_it_brings() {
    let (fake, dir) = extend().await;
    let host = dir.path().join(".runtime/ts/host.ts");
    assert!(std::fs::read_to_string(&host).unwrap().contains("August extension host"));

    let code = "export default (august) => august.describe('Weather', 'Weather tool');";
    fake.tool("save_extension", json!({"name": "weather", "code": code}), ctx("1")).await.unwrap();
    let folder = dir.path().join("weather");
    assert_eq!(std::fs::read_to_string(folder.join("index.ts")).unwrap(), code);
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(folder.join("extension.json")).unwrap()).unwrap();
    assert_eq!(manifest["command"], json!(["bun", "run", host.display().to_string(), "index.ts", "weather"]));
    assert_eq!(fake.calls("extension_enable"), [json!({"name": "weather"})]);
}

#[tokio::test]
async fn the_guide_is_a_skill_that_points_at_the_host() {
    let (_fake, dir) = extend().await;
    let skill = std::fs::read_to_string(dir.path().join(".runtime/skills/writing-extensions/SKILL.md")).unwrap();
    assert!(skill.starts_with("---\nname: writing-extensions\n"));
    assert!(skill.contains(&dir.path().join(".runtime/ts/host.ts").display().to_string()) && !skill.contains("{host}"));
    let beside = |file: &str| std::fs::read_to_string(dir.path().join(".runtime/skills/writing-extensions").join(file)).unwrap();
    assert!(beside("august.d.ts").contains("declare module \"august\""));
    assert!(beside("PROTOCOL.md").contains("# The August extension protocol"));
}

#[tokio::test]
async fn a_typescript_extension_may_have_many_files() {
    let (fake, dir) = extend().await;
    let files = json!({"index.ts": "import { w } from './lib/w.ts'; export default w;", "lib/w.ts": "export const w = () => {};"});
    fake.tool("save_extension", json!({"name": "multi", "files": files}), ctx("1")).await.unwrap();
    let folder = dir.path().join("multi");
    assert!(folder.join("lib/w.ts").is_file());
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(folder.join("extension.json")).unwrap()).unwrap();
    assert_eq!(manifest["command"][3], "index.ts");
}

#[tokio::test]
async fn other_languages_bring_their_own_extension_json() {
    let (fake, _dir) = extend().await;
    let err = fake.tool("save_extension", json!({"name": "py", "files": {"main.py": "print()"}}), ctx("1")).await.unwrap_err();
    assert!(err.to_string().contains("extension.json"), "{err}");
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap().status.success();
    assert!(ok, "git {args:?}");
}

/// A git repository holding an extension, with one commit.
fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    commit(repo.path(), files);
    repo
}

fn commit(dir: &std::path::Path, files: &[(&str, &str)]) {
    for (path, text) in files {
        std::fs::write(dir.join(path), text).unwrap();
    }
    git(dir, &["add", "."]);
    git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "c"]);
}

#[tokio::test]
async fn an_extension_installed_from_git_is_cloned_started_and_updated() {
    let (fake, dir) = extend().await;
    let source = repo(&[("extension.json", r#"{"command": ["./run"]}"#)]);
    let url = format!("file://{}/weather.git", source.path().display());
    std::fs::rename(source.path().join(".git"), source.path().join("weather.git")).unwrap();
    git(source.path(), &["--git-dir", "weather.git", "config", "core.bare", "true"]);

    fake.tool("install_extension", json!({"url": url}), ctx("1")).await.unwrap();
    let folder = dir.path().join("weather");
    assert!(folder.join("extension.json").is_file());
    assert_eq!(fake.calls("extension_enable"), [json!({"name": "weather"})]);

    let err = fake.tool("install_extension", json!({"url": url}), ctx("1")).await.unwrap_err();
    assert!(err.to_string().contains("already installed"), "{err}");

    let said = fake.tool("extensions", json!({"action": "update", "name": "weather"}), ctx("1")).await.unwrap();
    assert!(said.contains("up to date"), "{said}");
    let work = tempfile::tempdir().unwrap();
    git(work.path(), &["clone", "-q", &url, "."]);
    commit(work.path(), &[("run", "new")]);
    git(work.path(), &["push", "-q"]);
    fake.tool("extensions", json!({"action": "update", "name": "weather"}), ctx("1")).await.unwrap();
    assert_eq!(std::fs::read_to_string(folder.join("run")).unwrap(), "new");
    assert_eq!(fake.calls("extension_enable").len(), 2);
}

#[tokio::test]
async fn a_repository_without_extension_json_is_not_installed() {
    let (fake, dir) = extend().await;
    let source = repo(&[("README.md", "hi")]);
    let err = fake.tool("install_extension", json!({"url": source.path().to_str().unwrap(), "name": "readme"}), ctx("1")).await.unwrap_err();
    assert!(err.to_string().contains("extension.json"), "{err}");
    assert!(!dir.path().join("readme").exists());
    assert!(fake.calls("extension_enable").is_empty());
}
