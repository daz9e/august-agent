//! `watch` against a fake core: a changed extension is restarted and announced in the home
//! thread; untouched and disabled ones are left alone, and a failed restart is reported.

use super::serve;
use august_ext::FakeAugust;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn a_changed_extension_is_restarted_and_announced() {
    let tmp = tempfile::tempdir().unwrap();
    let (exts, bin) = (tmp.path().join("extensions"), tmp.path().join("bin"));
    for name in ["weather", "quiet", "off"] {
        std::fs::create_dir_all(exts.join(name)).unwrap();
        std::fs::write(exts.join(name).join("index.ts"), "v1").unwrap();
    }
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("august-ext-telegram"), "bin v1").unwrap();

    let (fake, august) = FakeAugust::new(&[("AUGUST_EXTENSIONS", exts.to_str().unwrap())]);
    fake.on("extensions", |_| {
        Ok(json!([
            {"name": "weather", "state": "running"}, {"name": "quiet", "state": "running"},
            {"name": "off", "state": "disabled"}, {"name": "telegram", "state": "running"}, {"name": "watch", "state": "running"}
        ]))
    });
    fake.on("extension_enable", |p| if p["name"] == "telegram" { anyhow::bail!("it crashed") } else { Ok(json!("running")) });
    tokio::spawn(serve(august, Duration::from_millis(10), bin.clone()));
    fake.started().await;
    fake.wait_call("extensions", 3).await;

    std::fs::write(exts.join("weather").join("index.ts"), "v2 longer").unwrap();
    std::fs::write(exts.join("off").join("index.ts"), "v2 longer").unwrap();
    std::fs::write(bin.join("august-ext-telegram"), "bin v2 longer").unwrap();
    fake.wait_sent("`weather` updated and restarted").await;
    fake.wait_sent("`telegram` changed, but didn't restart: it crashed").await;
    let n = fake.calls("extensions").len();
    fake.wait_call("extensions", n + 3).await;

    let restarted: Vec<_> = fake.calls("extension_enable").iter().map(|p| p["name"].clone()).collect();
    assert_eq!(restarted.len(), 2, "{restarted:?}");
    assert!(restarted.contains(&json!("weather")) && restarted.contains(&json!("telegram")));
    assert!(fake.calls("send").iter().all(|p| p["thread"] == "home"));
}
