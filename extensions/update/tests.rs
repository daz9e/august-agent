//! `update` against a fake core and a fake installer: the release goes into the folder of
//! `august`, then each enabled extension installed from git is pulled and restarted.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::json;
use std::path::Path;

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap().status.success();
    assert!(ok, "git {args:?}");
}

fn commit(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", "."]);
    git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "c"]);
}

struct Setup {
    fake: FakeAugust,
    bin: tempfile::TempDir,
    home: tempfile::TempDir,
    _source: tempfile::TempDir,
}

/// `update` with `bin` holding an `august`, and extensions `weather` (from git, with a new
/// commit upstream), `notes` (from git, disabled) and `local` (not from git); `installer` is
/// the release.
async fn update(installer: &str) -> Setup {
    let (bin, home, source) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(bin.path().join("august"), "old").unwrap();
    git(source.path(), &["init", "-q"]);
    commit(source.path(), "extension.json", r#"{"command": ["./run"]}"#);
    for name in ["weather", "notes"] {
        git(home.path(), &["clone", "-q", source.path().to_str().unwrap(), name]);
    }
    commit(source.path(), "run", "new");
    std::fs::create_dir(home.path().join("local")).unwrap();

    let (fake, august) = FakeAugust::new(&[("AUGUST_EXTENSIONS", home.path().to_str().unwrap())]);
    fake.on("extensions", |_| {
        Ok(json!([{"name": "weather", "state": "running", "summary": "Weather"}, {"name": "notes", "state": "disabled"},
                  {"name": "local", "state": "running"}, {"name": "tools", "state": "running", "origin": "default"}]))
    });
    fake.on("extension_enable", |_| Ok(json!("running")));
    tokio::spawn(serve(august, bin.path().to_path_buf(), installer.into()));
    fake.started().await;
    Setup { fake, bin, home, _source: source }
}

#[tokio::test]
async fn a_new_release_is_installed_extensions_from_git_updated_and_august_restarted() {
    let s = update(r#"printf new > "$AUGUST_INSTALL_DIR/august"; echo '  august-core abc'"#).await;
    let said = s.fake.command("update", "", ctx("1")).await.unwrap().unwrap();
    assert_eq!(std::fs::read_to_string(s.bin.path().join("august")).unwrap(), "new");
    assert!(said.contains("default extensions: updated (august-core abc)"), "{said}");
    assert!(said.contains("updated `weather`") && said.contains("restarts"), "{said}");
    assert_eq!(std::fs::read_to_string(s.home.path().join("weather/run")).unwrap(), "new");
    assert!(!s.home.path().join("notes/run").exists());
    assert_eq!(s.fake.calls("extension_enable"), [json!({"name": "weather"})]);
}

#[tokio::test]
async fn the_same_release_restarts_nothing() {
    let s = update("true").await;
    let said = s.fake.tool("update", json!({}), ctx("1")).await.unwrap();
    assert!(said.contains("up to date") && !said.contains("restarts"), "{said}");
}

#[tokio::test]
async fn a_failed_install_stops_the_update() {
    let s = update("echo no network >&2; false").await;
    let err = s.fake.tool("update", json!({}), ctx("1")).await.unwrap_err();
    assert!(err.to_string().contains("no network"), "{err}");
    assert!(s.fake.calls("extension_enable").is_empty());
}
