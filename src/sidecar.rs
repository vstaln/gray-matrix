//! NDJSON protocol 1.1 sidecar.
//! Manifest answers without config; delivery failures never echo secrets.
use serde_json::{json, Value};
use std::path::Path;
use std::sync::LazyLock;

/// Static manifest (`matrix` 0.1.x, protocol 1.1, no commands).
pub static MANIFEST: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "name": "matrix",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "commands": [],
        "hooks": ["prompt/context"],
        "tools": [{
            "name": "matrix_send",
            "label": "Matrix Send",
            "description": "Send a markdown message to a Matrix room as m.text with an org.matrix.custom.html formatted body. Room defaults to the configured home room. Only plain rooms — encrypted rooms are not supported.",
            "parameters": {
                "type": "object",
                "properties": {
                    "content": {"type": "string", "description": "Message text (markdown; rendered as Matrix HTML)"},
                    "room": {"type": "string", "description": "Room ID (!id:server) or alias (#room:server); default: the configured home room"}
                },
                "required": ["content"]
            }
        }]
    })
});

const PROMPT_CONTEXT: &str = "Matrix tools: matrix_send posts markdown to a Matrix room (room defaults to the home room) as m.text with an org.matrix.custom.html formatted body; replies are chunked at 8000 characters. Plain rooms only — E2EE is not supported, and encrypted rooms cannot be read or written. Never send secrets.";

fn is_error() -> Value {
    json!({
        "content": "Matrix delivery failed. Run gray-matrix doctor; do not blindly retry partial sends.",
        "is_error": true
    })
}

/// A specific, fixable failure. `detail` must never contain config values.
fn not_sent(detail: impl std::fmt::Display) -> Value {
    json!({
        "content": format!("Not sent: {detail}. Nothing reached Matrix; fix it and call again."),
        "is_error": true
    })
}

/// Dispatch one protocol method. Never echoes config/token text: failures
/// describe the caller's own input or a sanitized reason only.
pub async fn dispatch(method: &str, params: &Value, config_path: &Path) -> Value {
    if method == "plugin/manifest" {
        return MANIFEST.clone();
    }
    if method == "prompt/context" {
        return json!({"text": PROMPT_CONTEXT});
    }
    if method == "tool/call" {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        return match name {
            "matrix_send" => send_text(params, config_path).await,
            _ => is_error(),
        };
    }
    json!({"error": "Unsupported method"})
}

async fn send_text(params: &Value, config_path: &Path) -> Value {
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let text = args.get("content").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return not_sent("content is empty");
    }
    let config = match crate::config::load_config(config_path) {
        Ok(c) => c,
        Err(_) => return is_error(),
    };
    let fallback = config
        .get("home_room")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            config
                .get("rooms")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let room = args
        .get("room")
        .and_then(Value::as_str)
        .filter(|r| !r.trim().is_empty())
        .unwrap_or(&fallback);
    if !crate::config::room_valid(&Value::String(room.to_string())) {
        return not_sent("room must be a Matrix room ID (!id:server) or alias (#room:server)");
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crate::matrix::standalone_send(&config, room, text),
    )
    .await
    {
        Ok(Ok(msg)) => json!({"content": msg}),
        Ok(Err(e)) => not_sent(e),
        Err(_) => is_error(),
    }
}

/// The sidecar loop: one JSON request per stdin line, one `{"id", "result"}`
/// reply per line on stdout. `plugin/shutdown` and EOF exit.
pub fn serve(config_path: &Path) -> ! {
    use std::io::{BufRead, Write};
    const LIMIT: usize = 256 * 1024;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("sidecar runtime builds");
    loop {
        let mut buf: Vec<u8> = Vec::new();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => std::process::exit(1),
        };
        if n == 0 {
            std::process::exit(0);
        }
        if buf.len() > LIMIT {
            std::process::exit(1);
        }
        let text = match String::from_utf8(buf) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !req.is_object() {
            continue;
        }
        if req.get("method").and_then(Value::as_str) == Some("plugin/shutdown") {
            std::process::exit(0);
        }
        let id = match req.get("id") {
            Some(Value::Number(_)) => req.get("id").cloned().unwrap(),
            _ => continue,
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = match req.get("params") {
            Some(Value::Object(_)) => req.get("params").cloned().unwrap(),
            _ => json!({}),
        };
        let result = rt.block_on(dispatch(method, &params, config_path));
        let row = json!({"id": id, "result": result});
        if writeln!(out, "{row}").is_err() {
            std::process::exit(1);
        }
        if out.flush().is_err() {
            std::process::exit(1);
        }
    }
}
