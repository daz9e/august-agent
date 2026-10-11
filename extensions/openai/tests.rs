//! Built-in OpenAI-compatible services against a fake core and a fake API: each is a provider
//! out of the box, keyed by its own variable, and `endpoints` overrides its fields.

#[path = "../fake_http.rs"]
mod fake_http;

use super::serve;
use august_ext::FakeAugust;
use fake_http::{FakeHttp, Resp};
use serde_json::json;

#[tokio::test]
async fn presets_are_providers_and_endpoints_override_them() {
    // Together-style: a bare array instead of `{"data": [...]}`.
    let api = FakeHttp::start(|_| Resp::json(json!([{"id": "llama-4"}]))).await;
    let (fake, august) = FakeAugust::new(&[("GROQ_API_KEY", "gsk-test")]);
    fake.set_settings(json!({"endpoints": {"groq": {"base_url": api.url}}}));
    tokio::spawn(serve(august));
    let manifest = fake.started().await;

    let ids: Vec<_> = manifest["providers"].as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
    for id in ["openai", "groq", "gemini", "deepseek", "openrouter"] {
        assert!(ids.contains(&id), "{id} in {ids:?}");
    }
    let groq = manifest["accounts"].as_array().unwrap().iter().find(|a| a["id"] == "groq").unwrap();
    assert_eq!(groq["label"], "Groq");

    let models = fake.request("models", json!({"provider": "groq"})).await.unwrap();
    assert_eq!(models[0]["id"], "llama-4");
    assert_eq!(api.requests()[0].header("authorization"), "Bearer gsk-test");
}
