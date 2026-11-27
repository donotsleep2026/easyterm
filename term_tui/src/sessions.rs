//! The saved-SSH list the picker renders.
//!
//! The list comes from the **daemon** (`ClientMethod::SessionList`), not from a
//! file this process reads: the daemon owns `ssh.json`/`settings.toml` and is
//! the one that expands a host when it is opened, so asking it is the only way
//! the rows a user clicks are guaranteed to be the rows it can open. That also
//! keeps the TUI working against a daemon whose config dir is nowhere near this
//! process.
//!
//! Nothing here handles credentials: an entry carries its `config_id` and
//! display fields, and opening it sends the id back.

use std::time::Duration;

use term_client::Client;
use term_proto::SessionEntryWire;

/// How long the one-shot list connection may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);

/// Ask the daemon for its saved hosts.
pub async fn load(addr: &str) -> Result<Vec<SessionEntryWire>, String> {
    let client = match tokio::time::timeout(LIST_TIMEOUT, Client::connect(addr)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Err(format!("daemon unreachable: {e}")),
        Err(_) => return Err("daemon connect timed out".to_string()),
    };
    let entries = match tokio::time::timeout(LIST_TIMEOUT, client.session_list()).await {
        Ok(Ok(entries)) => entries,
        Ok(Err(e)) => return Err(format!("session list failed: {e}")),
        Err(_) => return Err("session list timed out".to_string()),
    };
    log::info!("daemon listed {} saved host(s)", entries.len());
    Ok(entries)
}
