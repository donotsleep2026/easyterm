//! Live Lua-runtime probe: connect to a running daemon over the core protocol
//! and report what its plugin runtime actually holds.
//!
//! ```sh
//! cargo run -q -p term_client --example lua-probe -- 127.0.0.1:56900
//! ```
//!
//! Read-only: it lists the daemon's plugin catalog, asks whether the `lua`
//! feature is allowed for the presented license, and (optionally) asks the
//! runtime to rescan. Nothing here mutates session state.

use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:56900".to_string());
    let socket_token = std::env::var("EASYTERM_SOCKET_TOKEN").ok();

    let client = term_client::Client::connect(&addr)
        .await
        .map_err(|e| format!("connect {addr}: {e}"))?;
    if let Some(token) = &socket_token {
        match client.auth(token).await {
            Ok(()) => println!("auth: accepted"),
            Err(e) => println!("auth failed: {e}"),
        }
    }
    match client.feature_allowed("lua").await {
        Ok(allowed) => println!("feature lua allowed: {allowed}"),
        Err(e) => println!("feature_allowed(lua) failed: {e}"),
    }
    let (writer, mut events, _asks) = client.into_parts();

    match writer.lua_plugins().await {
        Ok(plugins) => println!("catalog: {}", serde_json::to_string_pretty(&plugins)?),
        Err(e) => println!("lua_plugins failed: {e}"),
    }

    // Ask the runtime to rescan its configured dirs and publish the catalog
    // again, then read it back: a directory that does not resolve shows up as
    // an empty catalog rather than as an error.
    if std::env::args().any(|a| a == "--reload") {
        match writer.lua_command(json!("Reload")).await {
            Ok(()) => println!("reload: accepted"),
            Err(e) => println!("reload failed: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        match writer.lua_plugins().await {
            Ok(plugins) => println!(
                "catalog after reload: {}",
                serde_json::to_string_pretty(&plugins)?
            ),
            Err(e) => println!("lua_plugins failed after reload: {e}"),
        }
    }

    // Reproduce what a frontend sends at startup: the full plugin settings
    // (including the plan gate) and then a catalog request. The answer arrives
    // on the event stream, not as a reply.
    if std::env::args().any(|a| a == "--configure") {
        let dir = std::env::var("LUA_DIR").unwrap_or_else(|_| "plugins".to_string());
        let enabled: Vec<String> = std::env::var("LUA_ENABLED")
            .unwrap_or_default()
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let config = json!({
            "enabled": true,
            "pro": std::env::var("LUA_PRO").map(|v| v != "false" && v != "0").unwrap_or(true),
            "dir": dir,
            "enabled_plugins": enabled,
            "user_btn_order": {},
        });
        match writer
            .lua_command(json!({ "Configure": { "config": config } }))
            .await
        {
            Ok(()) => println!("configure: accepted (dir={dir}, {} enabled)", enabled.len()),
            Err(e) => println!("configure failed: {e}"),
        }
        if let Err(e) = writer.lua_command(json!("ListPlugins")).await {
            println!("list plugins failed: {e}");
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1500);
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
            println!("event: {event:?}");
        }
    }

    // Drain briefly so the daemon's pushes do not look like a hang.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
    while let Ok(Some(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        println!("event: {event:?}");
    }
    Ok(())
}
