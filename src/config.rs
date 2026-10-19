//! Private config load/save/validate.
//! Errors never contain config values.
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Default config path: ~/.config/gray-matrix/config.json
/// The user's home directory — `$HOME` on unix, `USERPROFILE` on Windows,
/// the same places gray itself resolves.
pub fn user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Default config path.
pub fn default_path() -> PathBuf {
    user_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/gray-matrix/config.json")
}

/// Write JSON (pretty + trailing newline) atomically: parents 0700,
/// temp file + rename, replacement mode 0600.
pub fn atomic_json(path: &Path, data: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            // Mode bits are a unix concept; Windows ACLs already default
            // a fresh user dir to owner-only.
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .map_err(|_| "cannot create config directory".to_string())?;
        }
    }
    let mut text =
        serde_json::to_string_pretty(data).map_err(|_| "cannot encode config".to_string())?;
    text.push('\n');
    let tmp_path = path.with_extension("tmp");
    // Born 0600: the access token is never readable by others, not even briefly.
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        use std::io::Write;
        opts.open(&tmp_path)
            .and_then(|mut f| f.write_all(text.as_bytes()))
            .map_err(|_| "cannot write config".to_string())?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "cannot protect config".to_string())?;
    }
    fs::rename(&tmp_path, path).map_err(|_| "cannot replace config".to_string())?;
    Ok(())
}

fn int_in(data: &Value, key: &str, low: u64, high: u64) -> Result<(), String> {
    if let Some(v) = data.get(key) {
        let ok = v.as_u64().is_some_and(|n| (low..=high).contains(&n));
        if !ok {
            return Err(format!("{key} must be an integer between {low} and {high}"));
        }
    }
    Ok(())
}

/// A Matrix user id: `@localpart:server.name`, no spaces/control.
pub fn user_id_valid(v: &Value) -> bool {
    match v.as_str() {
        Some(s) => {
            s.starts_with('@')
                && s.len() <= 255
                && s[1..].contains(':')
                && s.bytes().all(|b| b.is_ascii_graphic())
        }
        None => false,
    }
}

/// A room reference: `!id:server` room id or `#alias:server` room alias.
/// Aliases are resolved to ids before matching or sending.
pub fn room_valid(v: &Value) -> bool {
    match v.as_str() {
        Some(s) => {
            (s.starts_with('!') || s.starts_with('#'))
                && s.len() <= 255
                && s[1..].contains(':')
                && s.bytes().all(|b| b.is_ascii_graphic() && b != b' ')
        }
        None => false,
    }
}

/// The homeserver base URL: http(s) only, no spaces, no query/fragment.
pub fn homeserver_valid(v: &Value) -> bool {
    match v.as_str() {
        Some(s) => {
            (s.starts_with("https://") || s.starts_with("http://"))
                && s.len() <= 512
                && s.bytes()
                    .all(|b| b.is_ascii_graphic() && b != b'?' && b != b'#')
        }
        None => false,
    }
}

/// Validate a config object. Error texts never echo config values.
pub fn validate_config(data: &Value) -> Result<(), String> {
    if !data.is_object() {
        return Err("Configuration must be an object".to_string());
    }
    // The access token may live in MATRIX_ACCESS_TOKEN instead of the file;
    // the config stands without it so a tokenless file still parses.
    match data.get("access_token").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => {}
        Some(_) => return Err("Access token is empty; run setup".to_string()),
        None => {
            if std::env::var("MATRIX_ACCESS_TOKEN")
                .map(|t| t.trim().is_empty())
                .unwrap_or(true)
            {
                return Err("Access token is missing; run setup".to_string());
            }
        }
    }
    if !data.get("homeserver").is_some_and(homeserver_valid) {
        return Err("homeserver must be an http(s) base URL".to_string());
    }
    if !user_id_valid(&data["user_id"]) {
        return Err("user_id must be the bot's Matrix ID (@bot:server)".to_string());
    }
    match data.get("rooms").and_then(Value::as_array) {
        Some(a) if !a.is_empty() && a.iter().all(room_valid) => {}
        _ => {
            return Err(
                "rooms must be a non-empty array of room IDs (!id:server) or aliases (#a:server)"
                    .to_string(),
            )
        }
    }
    // Inbound is allowlisted-only: the array may be empty (nobody admitted
    // yet — every DM then gets a pairing reply), but it must be an array.
    match data.get("allowed_users").and_then(Value::as_array) {
        Some(a) if a.iter().all(user_id_valid) => {}
        _ => {
            return Err("allowed_users must be an array of Matrix user IDs (@u:server)".to_string())
        }
    }
    if let Some(home) = data.get("home_room") {
        if !room_valid(home) {
            return Err("home_room must be a room ID or alias".to_string());
        }
    }
    if let Some(free) = data.get("free_response_rooms") {
        let ok = free.as_array().is_some_and(|a| a.iter().all(room_valid));
        if !ok {
            return Err("free_response_rooms must be an array of room IDs or aliases".to_string());
        }
    }
    if data
        .get("require_mention")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err("require_mention must be true or false".to_string());
    }
    for key in ["gray_bin", "gray_home", "workdir"] {
        let ok = data
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| Path::new(s).is_absolute());
        if !ok {
            return Err(format!("{key} must be an absolute path"));
        }
    }
    if data
        .get("restart_notification")
        .is_some_and(|v| !v.is_boolean())
    {
        return Err("restart_notification must be true or false".to_string());
    }
    int_in(data, "timeout_seconds", 1, 86400)?;
    int_in(data, "concurrency", 1, 16)?;
    int_in(data, "max_requests", 1, 1000)?;
    int_in(data, "queue_capacity", 1, 100000)?;
    if data
        .get("busy_input_mode")
        .is_some_and(|v| !matches!(v.as_str(), Some("queue" | "steer")))
    {
        return Err("busy_input_mode must be queue or steer".to_string());
    }
    if let Some(policy) = data.get("session_reset") {
        if !policy.is_object() {
            return Err("session_reset must be an object".to_string());
        }
        if let Some(mode) = policy.get("mode") {
            let ok = mode
                .as_str()
                .is_some_and(|m| matches!(m, "none" | "idle" | "daily" | "both"));
            if !ok {
                return Err("session_reset.mode must be none, idle, daily, or both".to_string());
            }
        }
        if let Some(minutes) = policy.get("idle_minutes") {
            let ok = minutes.as_u64().is_some_and(|n| (1..=10_080).contains(&n));
            if !ok {
                return Err("session_reset.idle_minutes must be between 1 and 10080".to_string());
            }
        }
        if let Some(hour) = policy.get("at_hour") {
            let ok = hour.as_u64().is_some_and(|n| n <= 23);
            if !ok {
                return Err("session_reset.at_hour must be between 0 and 23".to_string());
            }
        }
    }
    Ok(())
}

/// The access token: config first, MATRIX_ACCESS_TOKEN second. Never logged.
pub fn access_token(data: &Value) -> Option<String> {
    data.get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("MATRIX_ACCESS_TOKEN")
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        })
}

/// Load + validate. Any IO/parse failure maps to the setup hint.
pub fn load_config(path: &Path) -> Result<Value, String> {
    let data: Value = fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "Configuration missing or invalid; run setup".to_string())?;
    validate_config(&data)?;
    Ok(data)
}

/// Validate + atomic save.
pub fn save_config(path: &Path, data: &Value) -> Result<(), String> {
    validate_config(data)?;
    atomic_json(path, data)
}
