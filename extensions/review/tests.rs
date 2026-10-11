//! The background review against a fake core: after a turn that is due, it forks the
//! conversation and tells the chat what the fork saved.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::turn_ctx;
use serde_json::json;

#[tokio::test]
async fn the_chat_hears_once_of_each_skill_the_review_changed() {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake.set_settings(json!({"memory_every": 0, "skills_after": 1}));
    fake.on("turn_start", |_| Ok(json!(7)));
    fake.on("turn_wait", |_| {
        let call = |name: &str, input| json!({"name": name, "input": input, "isError": false});
        Ok(json!({"status": "ok", "toolCalls": [
            call("save_skill", json!({"name": "deploy"})),
            call("edit_skill", json!({"name": "backup", "action": "write_file", "file": "references/hosts.md"})),
            call("edit_skill", json!({"name": "deploy", "action": "patch"})),
        ]}))
    });
    let turn = json!({"id": 1, "conversation": "thread", "show": true, "meta": {}});
    fake.event("turn_end", json!({"status": "ok", "toolCalls": 3}), turn_ctx("1", turn)).await.unwrap();

    let sent = fake.wait_sent("💾").await;
    assert_eq!(sent.text, "💾 Skill `backup`, `deploy` updated");
}
