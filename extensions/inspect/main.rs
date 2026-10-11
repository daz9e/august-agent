//! What an extension offers, in full: `august inspect <name>` in the terminal and
//! `/inspect <name>` in a chat show its state, tools (with their parameters), slash commands,
//! hooks, the events it emits, its permissions, jobs, prompt sections and accounts. Without a
//! name, a list of the extensions to pick from.

use august_ext::August;
use august_ext::client::Client;
use serde_json::Value;

/// The core's `extensions`, `tools` and `commands`, fetched by `call`.
async fn report<F, R>(name: &str, call: F) -> anyhow::Result<String>
where
    F: Fn(&'static str) -> R,
    R: Future<Output = anyhow::Result<Value>>,
{
    let all = call("extensions").await?;
    let all = all.as_array().cloned().unwrap_or_default();
    let names = || all.iter().filter_map(|e| e["name"].as_str()).collect::<Vec<_>>().join(", ");
    if name.is_empty() {
        return Ok(format!("Usage: inspect <extension>\nExtensions: {}", names()));
    }
    let Some(e) = all.iter().find(|e| e["name"] == name) else {
        return Ok(format!("No extension `{name}`. Extensions: {}", names()));
    };
    let mut out = format!("{name} — {}", e["state"].as_str().unwrap_or("?"));
    if let Some(error) = e["error"].as_str() {
        out += &format!(": {error}");
    }
    if let Some(detail) = e["health"]["detail"].as_str() {
        out += &format!(" ({detail})");
    }
    for key in ["summary", "details"] {
        if let Some(text) = e[key].as_str().filter(|s| !s.is_empty()) {
            out += &format!("\n{text}");
        }
    }

    let owned = |list: Value| -> Vec<Value> { list.as_array().into_iter().flatten().filter(|x| x["owner"] == name).cloned().collect() };
    let tools = owned(call("tools").await?);
    if !tools.is_empty() {
        out += "\n\nTools:";
        for t in tools {
            out += &format!("\n  {} — {}", t["name"].as_str().unwrap_or("?"), t["description"].as_str().unwrap_or(""));
            let required = &t["parameters"]["required"];
            for (p, schema) in t["parameters"]["properties"].as_object().into_iter().flatten() {
                let kind = schema["type"].as_str().unwrap_or("any");
                let must = if required.as_array().is_some_and(|r| r.iter().any(|r| r == p)) { ", required" } else { "" };
                out += &format!("\n      {p} ({kind}{must}) {}", schema["description"].as_str().unwrap_or(""));
                out.truncate(out.trim_end().len());
            }
        }
    }
    let commands = owned(call("commands").await?);
    if !commands.is_empty() {
        out += "\n\nCommands:";
        for c in commands {
            out += &format!("\n  /{} — {}", c["name"].as_str().unwrap_or("?"), c["description"].as_str().unwrap_or(""));
        }
    }
    if let Some(events) = e["events"].as_array().filter(|a| !a.is_empty()) {
        out += "\n\nEmits:";
        for ev in events {
            let observe = if ev["observe"] == true { " (observe)" } else { "" };
            out += &format!("\n  {}{observe} — {}", ev["name"].as_str().unwrap_or("?"), ev["description"].as_str().unwrap_or(""));
        }
    }
    for (key, label) in [("hooks", "Hooks"), ("needs", "Needs"), ("takes", "Takes"), ("sections", "Prompt sections"), ("accounts", "Accounts")] {
        let items: Vec<&str> = e[key].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        if !items.is_empty() {
            out += &format!("\n\n{label}: {}", items.join(", "));
        }
    }
    Ok(out)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("run") {
        // `august inspect <name>`: the core hands this program its socket.
        let client = tokio::sync::Mutex::new(Client::from_env().await?);
        let name = args.get(1).map(String::as_str).unwrap_or("");
        let text = report(name, |op| {
            let client = &client;
            async move { client.lock().await.call(op, serde_json::json!({})).await }
        })
        .await?;
        println!("{text}");
        return Ok(());
    }
    serve(August::new()).await;
    Ok(())
}

async fn serve(august: August) {
    august.needs(&["admin"]);
    august.describe(
        "`august inspect <extension>` and /inspect: what an extension offers",
        "Shows an extension's state, tools with their parameters, slash commands, hooks, emitted \
         events, permissions, jobs, prompt sections and accounts, as the core reports them.",
    );
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "august-ext-inspect".into());
    august.register_cli("inspect", "Show what an extension offers: tools, commands, hooks, events", &[exe, "run".into()]);
    let a = august.clone();
    august.register_command("inspect", "Show what an extension offers: /inspect <name>", move |args, _| {
        let a = a.clone();
        async move { report(args.trim(), |op| a.call(op, serde_json::json!({}))).await.map(Some) }
    });
    august.run().await;
}

#[cfg(test)]
mod tests;
