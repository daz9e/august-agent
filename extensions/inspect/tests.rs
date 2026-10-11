//! `/inspect` against a fake core: it shows everything the core knows of one extension, and
//! lists them when the name is missing or wrong.

use super::serve;
use august_ext::FakeAugust;
use august_ext::fake::ctx;
use serde_json::json;

#[tokio::test]
async fn inspect_shows_what_one_extension_offers() {
    let (fake, august) = FakeAugust::new(&[]);
    tokio::spawn(serve(august));
    fake.started().await;
    fake.on("extensions", |_| {
        Ok(json!([
            {"name": "weather", "state": "running", "summary": "Forecasts", "tools": ["forecast"], "commands": ["w"],
             "hooks": ["turn_end", "shutdown"], "needs": ["messaging"], "takes": [], "sections": ["weather"], "accounts": [],
             "events": [{"name": "weather:storm", "description": "A storm is coming", "observe": true}]},
            {"name": "other", "state": "disabled"}
        ]))
    });
    fake.on("tools", |_| {
        Ok(json!([
            {"name": "forecast", "description": "Weather for a city", "owner": "weather", "parameters":
                {"type": "object", "properties": {"city": {"type": "string", "description": "Where"}, "days": {"type": "integer"}}, "required": ["city"]}},
            {"name": "read", "description": "Read a file", "owner": "tools", "parameters": {}}
        ]))
    });
    fake.on("commands", |_| Ok(json!([{"name": "w", "description": "Weather now", "owner": "weather"}, {"name": "help", "description": "", "owner": "core"}])));

    let report = fake.command("inspect", "weather", ctx("1")).await.unwrap().unwrap();
    assert_eq!(
        report,
        "weather — running\nForecasts\n\n\
         Tools:\n  forecast — Weather for a city\n      city (string, required) Where\n      days (integer)\n\n\
         Commands:\n  /w — Weather now\n\n\
         Emits:\n  weather:storm (observe) — A storm is coming\n\n\
         Hooks: turn_end, shutdown\n\nNeeds: messaging\n\nPrompt sections: weather"
    );

    let missing = fake.command("inspect", "ghost", ctx("1")).await.unwrap().unwrap();
    assert_eq!(missing, "No extension `ghost`. Extensions: weather, other");
    let bare = fake.command("inspect", "", ctx("1")).await.unwrap().unwrap();
    assert_eq!(bare, "Usage: inspect <extension>\nExtensions: weather, other");
}
