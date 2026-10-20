//! Doctor preflight checks with safe errors.
//! Every failure is a category string; config values never surface.
use serde_json::Value;

/// Production entry point: checks `config` locally, then verifies the token
/// with `whoami` and resolves the configured rooms (membership reported,
/// not enforced — an invited-but-unjoined room still deserves a note).
pub async fn doctor(config: &Value) -> Result<(), String> {
    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if !is_executable(gray_bin) {
        return Err("gray executable is missing or not executable".to_string());
    }
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let provider: Value = std::fs::read(std::path::Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "gray provider configuration is missing or invalid".to_string())?;
    if provider
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("gray has no configured model".to_string());
    }
    let Some(rest) = crate::matrix::Rest::from_config(config) else {
        return Err("matrix access token is not configured".to_string());
    };
    let mxid = rest
        .whoami()
        .await
        .map_err(|e| format!("authentication failed: {e}"))?;
    let configured = config.get("user_id").and_then(Value::as_str).unwrap_or("");
    if !configured.is_empty() && configured != mxid {
        return Err("access token belongs to a different account than user_id".to_string());
    }
    let joined = rest.joined_rooms().await.unwrap_or_default();
    let rooms = config
        .get("rooms")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut unjoined = 0usize;
    for room in rooms.iter().filter_map(Value::as_str) {
        match rest.resolve_room(room).await {
            Ok(id) if joined.contains(&id) => {}
            Ok(_) => unjoined += 1,
            Err(e) => return Err(format!("a configured room did not resolve: {e}")),
        }
    }
    println!("Matrix whoami, token check, and room resolution verified.");
    if unjoined > 0 {
        println!(
            "Note: {unjoined} configured room(s) are not joined — the bot only answers rooms it is in."
        );
    }
    println!("Sync, sending, and provider generation were not tested.");
    Ok(())
}

/// `os.access(path, X_OK)` equivalent: a real file with any exec bit set.
fn is_executable(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
