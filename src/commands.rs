//! Headless `gray cmd` surface over text prefixes.
//!
//! A room message shaped like `!name args`, `/name args`, or
//! `<bot>: name args` is dispatched to `gray cmd run` inside the
//! conversation's own GRAY_HOME — the same isolation a turn gets — and the
//! resulting JSON object renders as an ordinary room reply. A parse miss
//! or an unknown name falls through to the normal prompt path, so prose
//! is never eaten and a gray too old for `cmd` degrades gracefully.
//!
//! v1 is text-only per GRAY-CMD-CONTRACT: `kind:"ui"` results post their
//! `fallback_text` (the numbered-menu render meant for component-less
//! platforms); `cmd interact` is unreachable until a component surface
//! exists.
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `cmd list` cache lifetime — the registry only moves on a gray upgrade.
const LIST_TTL: Duration = Duration::from_secs(300);
/// Hard budget on one `cmd` call: list and run are headless and quick.
const CMD_TIMEOUT: Duration = Duration::from_secs(60);
/// Registry rows marked `slow` (compaction, plugin calls, updates) get
/// this window instead — a room reply has no platform deadline.
const SLOW_TIMEOUT: Duration = Duration::from_secs(120);

/// The v1 text command set. When `cmd list` cannot be fetched — an older
/// gray without the verb, a spawn failure — only these are dispatched, so
/// a mistyped `!anything` still lands on the prompt path as before.
const FALLBACK: &[&str] = &[
    "help", "model", "usage", "new", "sessions", "cron", "skills",
];

/// `[a-z][a-z0-9_-]*` — the command-name shape the contract allows.
fn name_valid(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A leading mention for the bot, stripped: `@bot:server`, `bot:server`
/// or the bare localpart, followed by `:` `,` whitespace or end. The next
/// character must be a real separator so `bot2:` never strips `bot`.
fn strip_mention<'a>(text: &'a str, own_id: &str) -> Option<&'a str> {
    let bare = own_id.strip_prefix('@').unwrap_or(own_id);
    let localpart = bare.split(':').next().unwrap_or("");
    // Longest candidate first so `bot:server:` resolves against the id,
    // not the localpart plus a stray colon.
    let mut candidates = vec![own_id, bare];
    if localpart != bare {
        candidates.push(localpart);
    }
    for cand in candidates {
        if cand.is_empty() {
            continue;
        }
        let Some(rest) = text.strip_prefix(cand) else {
            continue;
        };
        let rest = if let Some(r) = rest.strip_prefix([':', ',']) {
            r
        } else if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            rest
        } else {
            continue;
        };
        return Some(rest.trim_start());
    }
    None
}

/// `!name args` · `/name args` · `<bot>: name args` → `(name, args)`.
/// Anything else is a normal prompt and returns `None`.
pub fn parse_prefixed(text: &str, own_id: &str) -> Option<(String, String)> {
    let t = text.trim();
    let body: &str = if let Some(rest) = strip_mention(t, own_id) {
        // `bot: name` as well as `bot: !name`.
        rest.strip_prefix(['!', '/'])
            .map(str::trim_start)
            .unwrap_or(rest)
    } else {
        match t.as_bytes().first() {
            Some(b'!') | Some(b'/') => t[1..].trim_start(),
            _ => return None,
        }
    };
    let end = body.find(char::is_whitespace).unwrap_or(body.len());
    let (name, args) = body.split_at(end);
    if !name_valid(name) {
        return None;
    }
    Some((name.to_string(), args.trim().to_string()))
}

/// One `cmd list` snapshot: known names plus aliases, and each name's
/// scope so `run` picks the right GRAY_HOME.
#[derive(Clone, Default)]
struct Registry {
    names: HashSet<String>,
    /// canonical name or alias -> "conversation" | "global" | ...
    scopes: HashMap<String, String>,
    /// Names (and aliases) whose runs can outlast the normal timeout.
    slow: HashSet<String>,
}

struct Cached {
    at: Instant,
    registry: Registry,
}

/// The daemon's handle on the command surface: just the registry cache.
/// Every spawn derives its env from config exactly like a turn child.
pub struct Commands {
    cache: tokio::sync::Mutex<Option<Cached>>,
}

impl Commands {
    pub fn new() -> Self {
        Self {
            cache: tokio::sync::Mutex::new(None),
        }
    }

    /// The live registry, fetching `cmd list` at most once per TTL. A
    /// failed fetch substitutes the v1 fallback set — and is itself
    /// cached, so a gray without `cmd` is not re-probed per message.
    async fn registry(&self, config: &Value, home: &Path) -> Registry {
        let mut guard = self.cache.lock().await;
        if let Some(cached) = guard.as_ref() {
            if cached.at.elapsed() < LIST_TTL {
                return cached.registry.clone();
            }
        }
        let registry = match list_commands(config, home).await {
            Some(r) => r,
            None => fallback_registry(),
        };
        let out = registry.clone();
        *guard = Some(Cached {
            at: Instant::now(),
            registry,
        });
        out
    }

    /// `Some(answer)` when `name` is a real command — `None` leaves the
    /// message on the normal prompt path.
    pub async fn dispatch(
        &self,
        config: &Value,
        config_path: &Path,
        conversation: &str,
        name: &str,
        args: &str,
    ) -> Option<Outcome> {
        let home = crate::session::conversation_home(config_path, conversation);
        let _ = prepare_home(config, &home);
        let registry = self.registry(config, &home).await;
        if !registry.names.contains(name) {
            return None;
        }
        // Conversation commands run in this room's home; global commands
        // in the user's main home (the contract's scope routing).
        let scope = registry
            .scopes
            .get(name)
            .map(String::as_str)
            .unwrap_or("conversation");
        let gray_home = crate::matrix::cfg_str(config, "gray_home");
        let run_home = if scope == "global" && !gray_home.is_empty() {
            PathBuf::from(gray_home)
        } else {
            home
        };
        Some(run_command(config, &run_home, name, args, registry.slow.contains(name)).await)
    }
}

/// The fallback set as a registry; everything is conversation-scoped.
fn fallback_registry() -> Registry {
    let mut registry = Registry::default();
    for name in FALLBACK {
        registry.names.insert(name.to_string());
        registry
            .scopes
            .insert(name.to_string(), "conversation".to_string());
    }
    registry
}

/// `cmd list` envelope -> registry. `None` when this gray cannot answer
/// the verb at all (envelope not ok, or no JSON on the wire).
fn registry_from(out: &Value) -> Option<Registry> {
    if out.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let commands = out.pointer("/result/commands").and_then(Value::as_array)?;
    let mut registry = Registry::default();
    for entry in commands {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let scope = entry
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("conversation");
        let slow = entry.get("slow").and_then(Value::as_bool).unwrap_or(false);
        registry.names.insert(name.to_string());
        registry.scopes.insert(name.to_string(), scope.to_string());
        if slow {
            registry.slow.insert(name.to_string());
        }
        for alias in entry
            .get("aliases")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            registry.names.insert(alias.to_string());
            registry
                .scopes
                .entry(alias.to_string())
                .or_insert_with(|| scope.to_string());
            if slow {
                registry.slow.insert(alias.to_string());
            }
        }
    }
    if registry.names.is_empty() {
        None
    } else {
        Some(registry)
    }
}

async fn list_commands(config: &Value, home: &Path) -> Option<Registry> {
    let out = cmd_json(config, home, &["list".to_string()]).await.ok()?;
    registry_from(&out)
}

/// `cmd run` argv: the name, then the flat args string as one word — the
/// contract's "one free-form args option per command".
fn run_argv(name: &str, args: &str) -> Vec<String> {
    let mut argv = vec!["run".to_string(), name.to_string()];
    if !args.is_empty() {
        argv.push(args.to_string());
    }
    argv
}

/// What a `cmd run` answer becomes: room text, or a prompt the caller
/// queues as a turn in this conversation.
#[derive(Debug)]
pub enum Outcome {
    Text(String),
    Prompt { prompt: String, fallback: String },
}

/// `cmd run <name> [args]` -> the answer. A gray too old for `cmd` writes
/// clap usage to stderr instead of an envelope; say so plainly.
async fn run_command(config: &Value, home: &Path, name: &str, args: &str, slow: bool) -> Outcome {
    let timeout = if slow { SLOW_TIMEOUT } else { CMD_TIMEOUT };
    match cmd_json_t(config, home, &run_argv(name, args), timeout).await {
        Ok(out) => render_result(&out),
        Err(e) if e == "gray cmd returned no result" => Outcome::Text(
            "This gray does not have `cmd` yet; send the request as a normal message.".to_string(),
        ),
        Err(e) => Outcome::Text(e),
    }
}

/// One `cmd` envelope reduced to its room action. `kind:"ui"` degrades to
/// `fallback_text`; `kind:"prompt"` hands back a turn; errors post their
/// `error.message`.
pub fn render_result(out: &Value) -> Outcome {
    if out.get("ok").and_then(Value::as_bool) == Some(true) {
        let result = out.get("result").cloned().unwrap_or(Value::Null);
        if result.get("kind").and_then(Value::as_str) == Some("prompt") {
            let prompt = result
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if prompt.is_empty() {
                return Outcome::Text("gray answered an empty prompt.".to_string());
            }
            let fallback = result
                .get("fallback_text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Outcome::Prompt { prompt, fallback };
        }
        let field = match result.get("kind").and_then(Value::as_str) {
            Some("ui") => "fallback_text",
            _ => "text",
        };
        let text = result
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        Outcome::Text(if text.is_empty() {
            "Done.".to_string()
        } else {
            text.to_string()
        })
    } else {
        let message = out
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("command failed")
            .trim();
        Outcome::Text(if message.is_empty() {
            "command failed".to_string()
        } else {
            message.to_string()
        })
    }
}

/// Give a conversation home the shape `cmd` expects: the directory plus
/// the provider config snapshot a turn would have left there. Best-effort
/// — `cmd run` reports its own failure when this cannot complete.
fn prepare_home(config: &Value, home: &Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(home)
        .map_err(|_| "cannot create conversation home".to_string())?;
    let _ = std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700));
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    if gray_home.is_empty() {
        return Ok(());
    }
    let provider_path = Path::new(gray_home).join("config.json");
    if provider_path == home.join("config.json") {
        return Ok(());
    }
    if let Ok(bytes) = std::fs::read(&provider_path) {
        if let Ok(provider) = serde_json::from_slice::<Value>(&bytes) {
            crate::config::atomic_json(&home.join("config.json"), &provider)?;
        }
    }
    Ok(())
}

/// `GRAY_HOME=<home> gray cmd <argv>` — one JSON object on stdout, with a
/// turn child's env discipline: GRAY_*/MATRIX_* scrubbed, PATH re-derived.
async fn cmd_json(config: &Value, home: &Path, argv: &[String]) -> Result<Value, String> {
    cmd_json_t(config, home, argv, CMD_TIMEOUT).await
}

async fn cmd_json_t(
    config: &Value,
    home: &Path,
    argv: &[String],
    timeout: Duration,
) -> Result<Value, String> {
    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if gray_bin.is_empty() {
        return Err("gray executable is missing".to_string());
    }
    let mut cmd = tokio::process::Command::new(gray_bin);
    cmd.arg("cmd");
    cmd.args(argv);
    cmd.current_dir(home);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        let Some(k_str) = k.to_str() else {
            continue;
        };
        if k_str.starts_with("GRAY_")
            || k_str.starts_with("MATRIX_")
            || k_str.starts_with("OPENAI_")
        {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("GRAY_HOME", home);
    cmd.env(
        "PATH",
        crate::runner::child_path(
            std::env::var_os("PATH"),
            std::env::var_os("HOME").map(PathBuf::from),
            Path::new(gray_bin),
        ),
    );
    let child = cmd.spawn().map_err(|_| "cannot start gray".to_string())?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(_)) => return Err("gray cmd failed".to_string()),
        Err(_) => return Err("gray cmd timed out".to_string()),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).map_err(|_| "gray cmd returned no result".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "@graybot:example.org";

    fn parsed(text: &str) -> Option<(String, String)> {
        parse_prefixed(text, BOT)
    }

    #[test]
    fn parses_bang_and_slash() {
        assert_eq!(
            parsed("!model anthropic/claude-sonnet-4.6"),
            Some((
                "model".to_string(),
                "anthropic/claude-sonnet-4.6".to_string()
            ))
        );
        assert_eq!(parsed("/usage"), Some(("usage".to_string(), String::new())));
        assert_eq!(
            parsed("!help extra words"),
            Some(("help".to_string(), "extra words".to_string()))
        );
        assert_eq!(
            parsed("  !sessions  "),
            Some(("sessions".to_string(), String::new()))
        );
    }

    #[test]
    fn parses_mention_forms() {
        assert_eq!(
            parsed("@graybot:example.org: model x"),
            Some(("model".to_string(), "x".to_string()))
        );
        assert_eq!(
            parsed("graybot: usage"),
            Some(("usage".to_string(), String::new()))
        );
        assert_eq!(
            parsed("graybot:example.org: new"),
            Some(("new".to_string(), String::new()))
        );
        assert_eq!(
            parsed("graybot, sessions"),
            Some(("sessions".to_string(), String::new()))
        );
        assert_eq!(
            parsed("@graybot:example.org cron list"),
            Some(("cron".to_string(), "list".to_string()))
        );
        assert_eq!(
            parsed("graybot: !model x"),
            Some(("model".to_string(), "x".to_string()))
        );
    }

    #[test]
    fn rejects_non_commands() {
        for bad in [
            "",
            "hi there",
            "!9lives",
            "!UPPER args",
            "!_private",
            "!",
            "/",
            "//x",
            "graybot2: x",
            "bot: x",
            "graybot:",
            "graybot:: x",
        ] {
            assert_eq!(parsed(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn registry_collects_names_aliases_scopes() {
        let out = serde_json::json!({"ok": true, "result": {"commands": [
            {"name": "model", "aliases": ["m"], "scope": "conversation"},
            {"name": "gateway", "aliases": [], "scope": "global"},
            {"name": "theme", "scope": "local"}
        ]}});
        let registry = registry_from(&out).unwrap();
        for name in ["model", "m", "gateway", "theme"] {
            assert!(registry.names.contains(name), "{name}");
        }
        assert_eq!(
            registry.scopes.get("gateway").map(String::as_str),
            Some("global")
        );
        assert_eq!(
            registry.scopes.get("m").map(String::as_str),
            Some("conversation")
        );

        let bad = serde_json::json!({"ok": false, "error": {"code": "x", "message": "m"}});
        assert!(registry_from(&bad).is_none());
        let empty = serde_json::json!({"ok": true, "result": {"commands": []}});
        assert!(registry_from(&empty).is_none());
    }

    #[test]
    fn run_argv_passes_flat_args() {
        assert_eq!(run_argv("usage", ""), vec!["run", "usage"]);
        assert_eq!(
            run_argv("model", "anthropic/claude-sonnet-4.6"),
            vec!["run", "model", "anthropic/claude-sonnet-4.6"]
        );
    }

    #[test]
    fn renders_text_ui_and_errors() {
        let text = serde_json::json!({
            "ok": true,
            "result": {"kind": "text", "text": "**hi**", "close": true}
        });
        match render_result(&text) {
            Outcome::Text(t) => assert_eq!(t, "**hi**"),
            other => panic!("expected text: {other:?}"),
        }

        let ui = serde_json::json!({
            "ok": true,
            "result": {"kind": "ui", "token": "t", "document": {},
                       "fallback_text": "1. one\n2. two"}
        });
        match render_result(&ui) {
            Outcome::Text(t) => assert_eq!(t, "1. one\n2. two"),
            other => panic!("expected text: {other:?}"),
        }

        let err = serde_json::json!({
            "ok": false,
            "error": {"code": "unknown_command", "message": "no such command"}
        });
        match render_result(&err) {
            Outcome::Text(t) => assert_eq!(t, "no such command"),
            other => panic!("expected text: {other:?}"),
        }

        match render_result(&serde_json::json!({"ok": true})) {
            Outcome::Text(t) => assert_eq!(t, "Done."),
            other => panic!("expected text: {other:?}"),
        }
        match render_result(&serde_json::json!({"ok": false})) {
            Outcome::Text(t) => assert_eq!(t, "command failed"),
            other => panic!("expected text: {other:?}"),
        }
    }
}
