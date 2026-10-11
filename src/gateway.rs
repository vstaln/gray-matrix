//! The Matrix daemon: `/sync` long-poll -> durable inbox -> isolated gray
//! turns -> `send/m.room.message` replies out. REST is stateless, so unlike
//! the IRC bridge there is no connection to babysit: delivery is one HTTP
//! PUT and a failure is retried or failed by the worker itself.
//!
//! The `since` cursor is persisted after each batch so a restart replays
//! at most the last batch, and the inbox row id (`mx:<event_id>`) dedupes
//! that replay. The very first sync only captures the cursor — backlog is
//! never turned into turns.
//!
//! Admission mirrors the other bridges: ourselves never turn; the sender's
//! mxid must be on `allowed_users`; outside a DM or a `free_response_rooms`
//! room the message must mention the bot (`m.mentions` or a textual mxid).
//! An un-admitted DMer gets a pairing reply instead of silence.
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::matrix::{Rest, TransportError};
use crate::runner::RunError;
use crate::store::Store;

/// The queue file next to the config.
pub fn open_store(config_path: &Path) -> Result<Store, String> {
    Store::new(&Store::default_path(config_path))
}

/// Where the persistent `/sync` cursor lives.
const SINCE_KEY: &str = "sync_since";
/// Where the learned DM room set lives (JSON array of room ids).
const DIRECT_KEY: &str = "direct_rooms";

/// Reconnect backoff after a poll failure: 1s doubling to a minute.
fn poll_backoff(attempt: u64) -> Duration {
    let secs = (1u64 << attempt.min(6)).min(60);
    Duration::from_secs(secs)
}

fn log_failure(what: &str, room: &str, error: &TransportError) {
    let detail = match error {
        TransportError::Http(code, detail) => format!("HTTP {code}: {detail}"),
        TransportError::Net(detail) => format!("network: {detail}"),
        TransportError::RateLimited(after) => format!("rate limited, retry after {after:?}s"),
        TransportError::Invalid(detail) => format!("not sent: {detail}"),
        TransportError::Auth(_) => "unauthorized".to_string(),
        TransportError::Forbidden(_) => "forbidden".to_string(),
    };
    eprintln!("[matrix] {what} failed in room {room}: {detail}");
}

struct Daemon {
    config: Value,
    config_path: PathBuf,
    store: Store,
    rest: Rest,
    /// Our own mxid — self-sent events never turn.
    own_id: String,
    /// Allowlisted room ids (aliases resolved at boot).
    rooms: HashSet<String>,
    /// Subset of `rooms` that needs no mention.
    free_rooms: HashSet<String>,
    /// DM room ids learned from `m.direct` and `is_direct` invites.
    direct_rooms: std::sync::RwLock<HashSet<String>>,
    /// The room lifecycle notices go to.
    home_room: String,
    /// `gray cmd` dispatch: `!name`/`/name`/`<bot>: name` prefixes.
    commands: crate::commands::Commands,
}

impl Daemon {
    /// `require_mention` config flag; default true (hermes semantics).
    fn require_mention(&self) -> bool {
        self.config
            .get("require_mention")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    /// Matrix user ids are case-insensitive on the localpart only in some
    /// server impls; exact match is the safe comparison.
    fn user_admitted(&self, mxid: &str) -> bool {
        !mxid.is_empty()
            && self
                .config
                .get("allowed_users")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).any(|u| u == mxid))
                .unwrap_or(false)
    }

    fn is_direct(&self, room_id: &str) -> bool {
        self.direct_rooms
            .read()
            .map(|d| d.contains(room_id))
            .unwrap_or(false)
    }

    /// Persisted + in-memory union of DM room ids.
    fn mark_direct(&self, room_id: &str) {
        let mut all: HashSet<String> = self
            .direct_rooms
            .read()
            .map(|d| d.clone())
            .unwrap_or_default();
        if !all.insert(room_id.to_string()) {
            return;
        }
        if let Ok(mut w) = self.direct_rooms.write() {
            w.insert(room_id.to_string());
        }
        let _ = self.store.meta_set(
            DIRECT_KEY,
            &Value::Array(all.into_iter().map(Value::String).collect()).to_string(),
        );
    }

    /// One inbound timeline event: admission, commands, enqueue.
    async fn on_event(&self, room_id: &str, event: &Value) {
        let sender = event.get("sender").and_then(Value::as_str).unwrap_or("");
        if sender.is_empty() || sender == self.own_id {
            return;
        }
        let content = event.get("content").cloned().unwrap_or(Value::Null);
        // Edits carry `m.relates_to.rel_type == "m.replace"`; a redacted or
        // replaced message is never a fresh turn.
        if content
            .pointer("/m.relates_to/rel_type")
            .and_then(Value::as_str)
            == Some("m.replace")
        {
            return;
        }
        let msgtype = content.get("msgtype").and_then(Value::as_str).unwrap_or("");
        let mut text = content
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match msgtype {
            "m.text" => {}
            "m.emote" => text = format!("* {sender} {}", text.trim()),
            // m.notice is bot output (and media/other msgtypes carry no
            // prompt text); answering either invites loops.
            _ => return,
        }
        // Reply bodies quote the original above a blank line; drop that.
        let text = crate::text::strip_reply_fallback(text.trim())
            .trim()
            .to_string();
        if text.is_empty() {
            return;
        }
        let event_id = event
            .get("event_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if event_id.is_empty() {
            return;
        }

        let dm = self.is_direct(room_id);
        if !dm && !self.rooms.contains(room_id) {
            // Not an allowlisted room and not a DM: silence.
            return;
        }
        if dm && !self.user_admitted(sender) {
            // Only a DM hears how to get in; room strangers get silence.
            if let Some(reply) = crate::pairing::reply_for(&self.store, sender, true) {
                let html = crate::text::md_to_html(&reply.text);
                if let Err(e) = self.rest.send_text(room_id, &reply.text, &html).await {
                    log_failure("pairing reply", room_id, &e);
                }
            }
            return;
        }
        if !self.user_admitted(sender) {
            return;
        }
        if !dm && self.require_mention() && !self.free_rooms.contains(room_id) {
            let mentioned = content
                .pointer("/m.mentions/user_ids")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .any(|id| id == self.own_id)
                })
                .unwrap_or(false)
                || text.contains(&self.own_id);
            if !mentioned {
                return;
            }
        }

        let conversation = crate::session::conversation_key(room_id, sender, dm);

        // Bare commands never reach the model.
        if matches!(text.trim(), "/new" | "/reset") {
            let home = crate::session::conversation_home(&self.config_path, &conversation);
            let note = match crate::session::reset_home(&home, crate::store::now_secs()) {
                Ok(()) => "Session reset — the next message starts fresh.",
                Err(_) => "Could not reset this session.",
            };
            let html = crate::text::md_to_html(note);
            if let Err(e) = self.rest.send_text(room_id, note, &html).await {
                log_failure("reset reply", room_id, &e);
            }
            return;
        }

        // Headless `gray cmd` surface: `!cmd`, `/cmd`, `bot: cmd`. A hit
        // answers inline and never reaches the queue; an unknown name is
        // an ordinary prompt.
        if let Some((name, args)) = crate::commands::parse_prefixed(&text, &self.own_id) {
            if let Some(outcome) = self
                .commands
                .dispatch(&self.config, &self.config_path, &conversation, &name, &args)
                .await
            {
                match outcome {
                    crate::commands::Outcome::Text(reply) => {
                        for chunk in
                            crate::text::split_message(&reply, crate::matrix::MAX_MESSAGE_CHARS)
                        {
                            let html = crate::text::md_to_html(&chunk);
                            if let Err(e) = self.rest.send_text(room_id, &chunk, &html).await {
                                log_failure("command reply", room_id, &e);
                                break;
                            }
                        }
                    }
                    crate::commands::Outcome::Prompt { prompt, fallback } => {
                        let capacity = self
                            .config
                            .get("queue_capacity")
                            .and_then(Value::as_u64)
                            .unwrap_or(1000);
                        let id = format!("mx:{event_id}:cmd");
                        match self.store.enqueue(
                            &id,
                            room_id,
                            sender,
                            &conversation,
                            &prompt,
                            capacity,
                        ) {
                            Ok(true) => {
                                if !fallback.trim().is_empty() {
                                    let html = crate::text::md_to_html(&fallback);
                                    let _ = self.rest.send_text(room_id, &fallback, &html).await;
                                }
                            }
                            Ok(false) => {}
                            Err(e) => {
                                let _ = self
                                    .rest
                                    .send_text(
                                        room_id,
                                        "Queue full; try again later.",
                                        "Queue full; try again later.",
                                    )
                                    .await;
                                eprintln!("[matrix] command turn enqueue failed: {e}");
                            }
                        }
                    }
                }
                return;
            }
        }

        let capacity = self
            .config
            .get("queue_capacity")
            .and_then(Value::as_u64)
            .unwrap_or(1000);
        let id = format!("mx:{event_id}");
        match self
            .store
            .enqueue(&id, room_id, sender, &conversation, &text, capacity)
        {
            Ok(true) => eprintln!("[matrix] queued {id} from {sender} in {room_id}"),
            Ok(false) => {} // replayed event — already queued or done
            Err(e) => {
                let html = crate::text::md_to_html("Queue full; try again later.");
                let _ = self
                    .rest
                    .send_text(room_id, "Queue full; try again later.", &html)
                    .await;
                eprintln!("[matrix] enqueue failed: {e}");
            }
        }
    }

    /// One claimed inbox item end to end. A failed send re-queues; a
    /// generated answer retries delivery instead of regenerating.
    async fn work(&self) -> Result<bool, String> {
        let Some(item) = self.store.claim()? else {
            return Ok(false);
        };
        // Typing indicator is best-effort; a dead homeserver must not stall
        // the turn.
        let _ = self
            .rest
            .typing(&item.target, &self.own_id, true, 30_000)
            .await;
        let outcome = crate::runner::run_gray(
            &self.config,
            &self.config_path,
            &item.conversation,
            &item.text,
            None,
        )
        .await;
        let _ = self.rest.typing(&item.target, &self.own_id, false, 0).await;
        match outcome {
            Ok(answer) => {
                let chunks = crate::text::split_message(&answer, crate::matrix::MAX_MESSAGE_CHARS);
                let mut attempts = 0u32;
                let mut sent = 0usize;
                'deliver: loop {
                    for chunk in chunks.iter().skip(sent) {
                        match self
                            .rest
                            .send_text(&item.target, chunk, &crate::text::md_to_html(chunk))
                            .await
                        {
                            Ok(_) => sent += 1,
                            Err(e) if e.is_retryable() && attempts < 5 => {
                                attempts += 1;
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                continue 'deliver;
                            }
                            Err(e) => {
                                log_failure("delivery", &item.target, &e);
                                if sent == 0 {
                                    self.store.fail(&item.id, "delivery failed")?;
                                } else {
                                    self.store.fail(&item.id, "delivery partially failed")?;
                                }
                                break 'deliver;
                            }
                        }
                    }
                    if sent == chunks.len() {
                        self.store.complete(&item.id, &answer)?;
                    }
                    break 'deliver;
                }
            }
            Err(RunError::Busy) => {
                self.store.requeue(&item.id)?;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                let note = e.to_string();
                let html = crate::text::md_to_html(&note);
                if let Err(e) = self.rest.send_text(&item.target, &note, &html).await {
                    log_failure("error reply", &item.target, &e);
                }
                self.store.fail(&item.id, &e.to_string())?;
            }
        }
        Ok(true)
    }
}

fn config_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// Rooms the `rooms` config allowlist plus `free_response_rooms` and
/// `home_room` resolve to. Unresolvable aliases are dropped with a warning
/// (fail closed: an unknown room simply never matches).
async fn resolve_rooms(rest: &Rest, config: &Value) -> (HashSet<String>, HashSet<String>, String) {
    let mut rooms = HashSet::new();
    let mut free = HashSet::new();
    let resolve = |list: &str| -> Vec<String> {
        config
            .get(list)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    for room in resolve("rooms") {
        match rest.resolve_room(&room).await {
            Ok(id) => {
                rooms.insert(id);
            }
            Err(e) => eprintln!("[matrix] could not resolve configured room: {e}"),
        }
    }
    for room in resolve("free_response_rooms") {
        match rest.resolve_room(&room).await {
            Ok(id) => {
                // A free-response room is admitted by definition.
                rooms.insert(id.clone());
                free.insert(id);
            }
            Err(e) => eprintln!("[matrix] could not resolve free-response room: {e}"),
        }
    }
    let home_cfg = config
        .get("home_room")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| resolve("rooms").into_iter().next())
        .unwrap_or_default();
    let home_room = if home_cfg.is_empty() {
        String::new()
    } else {
        match rest.resolve_room(&home_cfg).await {
            Ok(id) => id,
            Err(e) => {
                eprintln!("[matrix] could not resolve home room: {e}");
                String::new()
            }
        }
    };
    (rooms, free, home_room)
}

/// `m.direct` account data: `{ "@user:server": ["!room:server", ...] }`.
/// Fold every listed room into the direct set.
fn apply_direct_account_data(daemon: &Daemon, sync: &Value) {
    let Some(direct) = sync
        .get("account_data")
        .and_then(|a| a.get("events"))
        .and_then(Value::as_array)
        .and_then(|events| {
            events
                .iter()
                .find(|e| e.get("type").and_then(Value::as_str) == Some("m.direct"))
        })
        .and_then(|e| e.get("content"))
        .and_then(Value::as_object)
    else {
        return;
    };
    for rooms in direct.values() {
        if let Some(list) = rooms.as_array() {
            for room in list.iter().filter_map(Value::as_str) {
                daemon.mark_direct(room);
            }
        }
    }
}

/// Pending invites in `rooms.invite`: auto-join when the inviter is on the
/// allowlist, or when the invite is `is_direct` (a DM is how pairing starts,
/// and admission still gates what the inviter can do).
async fn handle_invites(daemon: &Daemon, sync: &Value) {
    let Some(invites) = sync.pointer("/rooms/invite").and_then(Value::as_object) else {
        return;
    };
    for room_id in invites.keys() {
        let Some(member) = invites[room_id]
            .get("invite_state")
            .and_then(|s| s.get("events"))
            .and_then(Value::as_array)
            .and_then(|events| {
                events.iter().find(|e| {
                    e.get("type").and_then(Value::as_str) == Some("m.room.member")
                        && e.get("state_key").and_then(Value::as_str) == Some(&daemon.own_id)
                })
            })
        else {
            continue;
        };
        let inviter = member.get("sender").and_then(Value::as_str).unwrap_or("");
        let is_direct = member
            .pointer("/content/is_direct")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !is_direct && !daemon.user_admitted(inviter) {
            eprintln!("[matrix] ignoring invite to {room_id} from unadmitted user");
            continue;
        }
        match daemon.rest.join(room_id).await {
            Ok(joined) => {
                eprintln!("[matrix] joined {joined} on invite from {inviter}");
                if is_direct {
                    daemon.mark_direct(&joined);
                }
            }
            Err(e) => log_failure("invite join", room_id, &e),
        }
    }
}

/// One `/sync` response: direct-room data, invites, then timeline events.
async fn apply_sync(daemon: &Daemon, sync: &Value) {
    apply_direct_account_data(daemon, sync);
    handle_invites(daemon, sync).await;
    let Some(joined) = sync.pointer("/rooms/join").and_then(Value::as_object) else {
        return;
    };
    for (room_id, room) in joined {
        let Some(events) = room.pointer("/timeline/events").and_then(Value::as_array) else {
            continue;
        };
        for event in events {
            if event.get("type").and_then(Value::as_str) == Some("m.room.message") {
                daemon.on_event(room_id, event).await;
            }
        }
    }
}

/// The daemon: lock + pidfile, workers, sync loop, graceful stop.
pub async fn run(config_path: &Path) -> Result<(), String> {
    let dir = config_dir(config_path);
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|_| "cannot create config directory".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    // daemon.json: `status` and lifecycle read it; removed on every exit.
    let pid_path = dir.join("daemon.json");
    {
        let stamp = serde_json::json!({
            "pid": std::process::id(),
            "started_at": crate::store::now_secs()
        });
        let _ = crate::config::atomic_json(&pid_path, &stamp);
    }
    struct RemovePid(PathBuf);
    impl Drop for RemovePid {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove_pid = RemovePid(pid_path);

    // One daemon per config: two would fight over the same cursor.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gateway.lock"))
        .map_err(|_| "cannot open gateway lock".to_string())?;
    use std::os::unix::io::AsRawFd;
    let locked = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        return Err("Another gateway is running".to_string());
    }
    let _lock = lock_file;

    let config = crate::config::load_config(config_path)?;
    let rest = Rest::from_config(&config).ok_or("matrix access token is missing")?;
    let store = open_store(config_path)?;
    let interrupted = store.recover().unwrap_or(0);

    // Prove the token before the first long-poll: a dead token is a setup
    // problem, not a connectivity one.
    let own_id = rest
        .whoami()
        .await
        .map_err(|_| "Matrix authentication failed; run setup".to_string())?;
    let (rooms, free_rooms, home_room) = resolve_rooms(&rest, &config).await;
    if rooms.is_empty() {
        eprintln!("[matrix] warning: no configured room resolved; only DMs will turn");
    }

    let mut direct_set: HashSet<String> = store
        .meta_get(DIRECT_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default();
    // Configured DMs (if the wizard recorded them) count too.
    if let Some(a) = config.get("direct_rooms").and_then(Value::as_array) {
        for r in a.iter().filter_map(Value::as_str) {
            direct_set.insert(r.to_string());
        }
    }

    let daemon = Arc::new(Daemon {
        config: config.clone(),
        config_path: config_path.to_path_buf(),
        store,
        rest,
        own_id,
        rooms,
        free_rooms,
        direct_rooms: std::sync::RwLock::new(direct_set),
        home_room,
        commands: crate::commands::Commands::new(),
    });

    // Boot notice is computed now (it records lifecycle state) and posted
    // after the first successful sync.
    let gray_bin = Path::new(crate::matrix::cfg_str(&daemon.config, "gray_bin"));
    let mut notice = if crate::lifecycle::enabled(&daemon.config) {
        crate::lifecycle::boot_via(gray_bin, &dir, interrupted).await
    } else {
        let _ = crate::lifecycle::boot(&dir);
        None
    };

    // Workers drain the inbox; the sync loop only ever enqueues.
    let workers = daemon
        .config
        .get("concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 16);
    let stop = Arc::new(tokio::sync::Notify::new());
    let mut handles = Vec::new();
    for _ in 0..workers {
        let d = daemon.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            loop {
                match d.work().await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => eprintln!("[matrix] worker error: {e}"),
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                    _ = stop.notified() => break,
                }
            }
        }));
    }

    let shutdown = Arc::new(tokio::sync::Notify::new());
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {},
                _ = int.recv() => {},
            }
            shutdown.notify_waiters();
        });
    }

    // The poll loop. `since` starts where the last run's cursor left off;
    // every synced batch is acknowledged by persisting the next cursor.
    let mut since: Option<String> = daemon.store.meta_get(SINCE_KEY).ok().flatten();
    let mut backoff_attempt: u64 = 0;
    // The very first sync only establishes the cursor — replaying the
    // backlog as turns would surprise everyone in the room.
    let mut first = since.is_none();
    let stop_reason: Result<(), String> = 'outer: loop {
        let poll = daemon.rest.sync(
            since.as_deref(),
            if first {
                0
            } else {
                crate::matrix::LONG_POLL_MS
            },
        );
        let sync = tokio::select! {
            sync = poll => sync,
            _ = shutdown.notified() => break 'outer Ok(()),
        };
        match sync {
            Ok(data) => {
                backoff_attempt = 0;
                let next = data
                    .get("next_batch")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if first {
                    first = false;
                    apply_direct_account_data(&daemon, &data);
                } else {
                    apply_sync(&daemon, &data).await;
                }
                if let Some(next) = next {
                    since = Some(next.clone());
                    let _ = daemon.store.meta_set(SINCE_KEY, &next);
                }
                // Boot notice once, on the first live cursor.
                if let Some(text) = notice.take() {
                    if !daemon.home_room.is_empty() {
                        let html = crate::text::md_to_html(&text);
                        if let Err(e) = daemon.rest.send_text(&daemon.home_room, &text, &html).await
                        {
                            log_failure("boot notice", &daemon.home_room, &e);
                        }
                    }
                }
            }
            Err(TransportError::RateLimited(after)) => {
                let wait = Duration::from_secs_f64(after.unwrap_or(5.0).clamp(1.0, 300.0));
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {},
                    _ = shutdown.notified() => break 'outer Ok(()),
                }
            }
            Err(error @ (TransportError::Auth(_) | TransportError::Forbidden(_))) => {
                eprintln!("[matrix] {error}; fix the access token (setup) — stopping");
                break 'outer Err("Matrix authentication failed".to_string());
            }
            Err(e) => {
                let wait = poll_backoff(backoff_attempt);
                backoff_attempt = backoff_attempt.saturating_add(1);
                eprintln!("[matrix] sync failed: {e}; retrying in {}s", wait.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {},
                    _ = shutdown.notified() => break 'outer Ok(()),
                }
            }
        }
    };

    // Goodbye notices are plain sends, bounded.
    let gray_bin = Path::new(crate::matrix::cfg_str(&daemon.config, "gray_bin"));
    let goodbye = crate::lifecycle::stop_via(gray_bin, &dir).await;
    if crate::lifecycle::enabled(&daemon.config) {
        let running: Vec<String> = daemon
            .store
            .running_targets()
            .unwrap_or_default()
            .into_iter()
            .map(|i| i.target)
            .collect();
        let targets = crate::lifecycle::shutdown_targets(&running, &daemon.home_room, &goodbye);
        let send = async {
            for (target, text) in targets {
                let html = crate::text::md_to_html(text);
                let _ = daemon.rest.send_text(&target, text, &html).await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(6), send).await;
    }

    eprintln!("[matrix] shutting down");
    stop.notify_waiters();
    for h in handles {
        h.abort();
    }
    // If shutdown never ran inside the loop (auth failure path), still
    // record a clean exit so the next boot does not report a crash.
    if std::fs::read(dir.join(crate::lifecycle::STATE_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|s| s.get("state").and_then(Value::as_str).map(str::to_string))
        .as_deref()
        == Some("running")
    {
        let restart = crate::lifecycle::restart_requested(&dir);
        crate::lifecycle::mark_stopped(&dir, restart);
    }
    stop_reason
}
