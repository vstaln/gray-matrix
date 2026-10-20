//! Isolated `gray -p --json` child per turn.
//! One conversation home per sha256(conversation); explicit session pointers
//! only — never guess a shared session id.
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Cap on one NDJSON stdout line (matches gray's own wire limit).
const LINE_CAP: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub enum RunError {
    Busy,
    Timeout(u64),
    /// The turn used all `max_requests` provider requests and was stopped.
    RequestLimit(u64),
    Provider(String),
    Spawn(String),
    Protocol(String),
    Exit(Option<i32>),
    Incomplete,
    Empty,
    Io(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy => f.write_str("This conversation is busy"),
            Self::Timeout(_) => f.write_str("gray turn timed out"),
            Self::RequestLimit(n) => write!(f, "gray turn hit its {n}-request limit"),
            Self::Provider(reason) => f.write_str(reason),
            Self::Spawn(_) => f.write_str("cannot start gray agent"),
            Self::Protocol(_) => {
                f.write_str("Invalid agent JSON output; upgrade gray to a compatible version")
            }
            Self::Exit(Some(n)) => write!(
                f,
                "gray exited with code {n}; not retrying possible side effects"
            ),
            Self::Exit(None) => {
                f.write_str("gray exited without a status; not retrying possible side effects")
            }
            Self::Incomplete => f.write_str(
                "Agent did not return a completed result; actions may already have occurred",
            ),
            Self::Empty => f.write_str("Agent returned no final answer"),
            Self::Io(_) => f.write_str("gray turn failed; check connectivity"),
        }
    }
}
impl std::error::Error for RunError {}

pub fn failure_notice(error: &RunError) -> String {
    const TAIL: &str = "Some steps may already have run. Send a message to try again.";
    match error {
        RunError::Busy => format!("That failed: this conversation is busy. {TAIL}"),
        RunError::Timeout(secs) => {
            format!(
                "Gave up: the turn ran past its {} limit. {TAIL}",
                human_secs(*secs)
            )
        }
        RunError::RequestLimit(n) => {
            format!("That failed: the turn hit its {n}-request limit. {TAIL}")
        }
        RunError::Provider(reason) => format!("That failed: {}. {TAIL}", cap_reason(reason)),
        RunError::Spawn(_) => {
            "That failed: gray could not start. Send a message to try again.".to_string()
        }
        RunError::Exit(Some(code)) => format!("That failed: gray exited with code {code}. {TAIL}"),
        RunError::Exit(None) => format!("That failed: gray stopped without an exit code. {TAIL}"),
        RunError::Protocol(_) => format!("That failed: gray's output could not be read. {TAIL}"),
        RunError::Incomplete => format!("That failed: gray did not finish the reply. {TAIL}"),
        RunError::Empty => format!("That failed: gray returned no answer. {TAIL}"),
        RunError::Io(reason) => format!("That failed: {reason}. {TAIL}"),
    }
}

fn human_secs(secs: u64) -> String {
    if secs > 0 && secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

fn cap_reason(reason: &str) -> String {
    let line = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 300 {
        let cut: String = line.chars().take(300).collect();
        format!("{cut}…")
    } else {
        line
    }
}

struct Consume {
    terminal: Option<Value>,
    protocol_ok: bool,
}

/// Held while a turn runs on a conversation home.
struct ProcessGroup {
    pid: u32,
    #[cfg(windows)]
    job: Option<JobGuard>,
}

impl ProcessGroup {
    #[cfg(unix)]
    fn new(pid: u32) -> Self {
        Self { pid }
    }

    #[cfg(windows)]
    fn new(pid: u32) -> Self {
        Self {
            pid,
            job: assign_job(pid),
        }
    }

    #[cfg(unix)]
    fn disarm(&mut self) {
        self.pid = 0;
    }

    #[cfg(windows)]
    fn disarm(&mut self) {
        self.pid = 0;
        self.job = None;
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if self.pid > 0 {
            kill_process_tree(self.pid);
        }
    }
}

#[cfg(unix)]
fn kill_process_tree(pid: u32) {
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

#[cfg(windows)]
fn kill_process_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", pid.to_string().as_str(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}


#[cfg(windows)]
struct JobGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
unsafe impl Send for JobGuard {}

#[cfg(windows)]
impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            TerminateJobObject(self.0, 1);
            CloseHandle(self.0);
        }
    }
}

// taskkill /T needs the parent alive to walk the tree; a Job Object
// terminates the tree even after the leader has exited.
#[cfg(windows)]
fn assign_job(pid: u32) -> Option<JobGuard> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };
    unsafe {
        let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
        if proc.is_null() {
            return None;
        }
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        let ok = !job.is_null() && AssignProcessToJobObject(job, proc) != 0;
        CloseHandle(proc);
        if ok {
            Some(JobGuard(job))
        } else {
            if !job.is_null() {
                CloseHandle(job);
            }
            None
        }
    }
}

type Pipe = std::sync::Arc<tokio::sync::Mutex<Option<tokio::process::ChildStdin>>>;

fn live() -> &'static std::sync::Mutex<std::collections::HashMap<String, Pipe>> {
    static LIVE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Pipe>>> =
        std::sync::OnceLock::new();
    LIVE.get_or_init(Default::default)
}

struct Live {
    conversation: String,
    pipe: Pipe,
}

impl Live {
    fn register(conversation: &str, stdin: Option<tokio::process::ChildStdin>) -> Self {
        let pipe: Pipe = std::sync::Arc::new(tokio::sync::Mutex::new(stdin));
        if let Ok(mut map) = live().lock() {
            map.insert(conversation.to_string(), pipe.clone());
        }
        Live {
            conversation: conversation.to_string(),
            pipe,
        }
    }

    fn end(&self) {
        if let Ok(mut map) = live().lock() {
            if map
                .get(&self.conversation)
                .is_some_and(|p| std::sync::Arc::ptr_eq(p, &self.pipe))
            {
                map.remove(&self.conversation);
            }
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.end();
    }
}

pub fn is_live(conversation: &str) -> bool {
    live()
        .lock()
        .map(|m| m.contains_key(conversation))
        .unwrap_or(false)
}

pub async fn steer(conversation: &str, text: &str) -> bool {
    use tokio::io::AsyncWriteExt;
    let pipe = live()
        .lock()
        .ok()
        .and_then(|m| m.get(conversation).cloned());
    let Some(pipe) = pipe else {
        return false;
    };
    let line = format!("{}\n", serde_json::json!({"type": "steer", "text": text}));
    let mut guard = pipe.lock().await;
    let Some(stdin) = guard.as_mut() else {
        return false;
    };
    stdin.write_all(line.as_bytes()).await.is_ok() && stdin.flush().await.is_ok()
}

/// Single-quote shell escaping for launcher argv words.
#[cfg(unix)]
fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// gray's PATH inside the child: the real PATH plus the gray binary's dir
/// and the usual user locations, so a runit/systemd PATH still finds it.
pub(crate) fn child_path(
    current: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
    gray_bin: &Path,
) -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = gray_bin.parent() {
        dirs.push(dir.to_path_buf());
    }
    if let Some(home) = home {
        #[cfg(unix)]
        dirs.push(home.join(".local/bin"));
        // %USERPROFILE%\.cargoin is where rustup puts cargo on Windows.
        dirs.push(home.join(".cargo/bin"));
    }
    if let Some(current) = current {
        for part in std::env::split_paths(&current) {
            if !dirs.contains(&part) {
                dirs.push(part);
            }
        }
    }
    if dirs.is_empty() {
        #[cfg(windows)]
        return std::ffi::OsString::new();
        #[cfg(not(windows))]
        return std::ffi::OsString::from("/usr/bin:/bin");
    }
    std::env::join_paths(&dirs).unwrap_or_default()
}

/// Run one isolated text turn and return the agent's final answer.
pub async fn run_gray(
    config: &Value,
    config_path: &Path,
    conversation: &str,
    prompt: &str,
    timeout_secs: Option<u64>,
) -> Result<String, RunError> {
    let timeout_secs = match timeout_secs {
        Some(0) => return Err(RunError::Io("Timeout must be positive".to_string())),
        Some(n) => n,
        None => config
            .get("timeout_seconds")
            .and_then(Value::as_u64)
            .unwrap_or(600),
    };
    if prompt.trim().is_empty() || prompt.chars().count() > 32000 {
        return Err(RunError::Io(
            "Prompt must contain 1-32000 characters".to_string(),
        ));
    }

    let home = crate::session::conversation_home(config_path, conversation);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&home)
        .map_err(|_| RunError::Io("cannot create conversation home".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    }

    // Non-blocking per-conversation lock, held for the whole turn. The
    // descriptor is close-on-exec (unix) / non-inheritable (Windows), so a
    // concurrently spawned gray child can never keep the lock alive after
    // this runner future has returned; dropping the file unlocks it.
    let lock_file = std::fs::File::create(home.join("run.lock"))
        .map_err(|_| RunError::Io("cannot lock conversation".to_string()))?;
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(RunError::Busy),
        Err(_) => return Err(RunError::Io("cannot lock conversation".to_string())),
    }
    let _lock = lock_file;

    // Snapshot the provider config into the isolated home.
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut provider: Value = std::fs::read(Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| RunError::Io("gray provider configuration is missing".to_string()))?;
    if let Some(model) = config
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.trim().is_empty())
    {
        provider["model"] = Value::String(model.to_string());
    }
    crate::config::atomic_json(&home.join("config.json"), &provider).map_err(RunError::Io)?;

    // Dedicated workdir/profile: the matrix tools plus the minimal builtin
    // set — no lockfile plugins that might start another gateway.
    let work = home.join("work");
    #[allow(unused_mut)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&work)
        .or_else(|e| if work.is_dir() { Ok(()) } else { Err(e) })
        .map_err(|_| RunError::Io("cannot create work directory".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700));
    }
    // A reinstall under the running service reads as "<path> (deleted)";
    // the launcher must exec the replacement at <path>.
    let exe = std::env::current_exe()
        .map(|p| {
            let p = p.to_string_lossy();
            p.strip_suffix(" (deleted)").unwrap_or(&p).to_string()
        })
        .unwrap_or_else(|_| "gray-matrix".to_string());
    let abs_config = config_path
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| config_path.to_string_lossy().into_owned());
    // What gray's `sidecar:` spawner can exec here: an sh script on unix, a
    // batch file on Windows (std routes .cmd through cmd.exe).
    #[cfg(windows)]
    let (launcher, script) = (
        home.join("matrix-sidecar.cmd"),
        format!("@echo off\r\n\"{exe}\" sidecar --config \"{abs_config}\"\r\n"),
    );
    #[cfg(not(windows))]
    let (launcher, script) = (
        home.join("matrix-sidecar"),
        format!(
            "#!/bin/sh
exec {} sidecar --config {}
",
            shell_word(&exe),
            shell_word(&abs_config)
        ),
    );
    std::fs::write(&launcher, &script)
        .map_err(|_| RunError::Io("cannot write launcher".to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| RunError::Io("cannot protect launcher".to_string()))?;
    }
    // The profile parser unquotes but does not unescape; forward slashes
    // keep a Windows path legible to it (and CreateProcess accepts them).
    #[cfg(windows)]
    let launcher_wire = launcher.to_string_lossy().replace('\\', "/");
    #[cfg(not(windows))]
    let launcher_wire = launcher.to_string_lossy().into_owned();
    let profile = format!(
        "plugins:
  - builtin: tools-minimal
  - sidecar: {}
",
        serde_json::to_string(&launcher_wire).unwrap_or_default()
    );
    std::fs::write(work.join("gray.yml"), profile)
        .map_err(|_| RunError::Io("cannot write profile".to_string()))?;
    // Bound project discovery at the dedicated work directory.
    let _ = std::fs::create_dir(work.join(".git"));

    let state_path = home.join("session.json");
    let mut state: Value = std::fs::read(&state_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if !state.is_object() {
        state = Value::Object(Default::default());
    }
    let sessions = home.join("sessions");
    let has_session = state
        .get("session_id")
        .and_then(Value::as_str)
        .is_some_and(|sid| !sid.is_empty())
        || sessions
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
    let now = crate::store::now_secs();
    if crate::session::ResetPolicy::from_config(config)
        .reset_reason(&state, has_session, now)
        .is_some()
    {
        crate::session::reset_home(&home, now).map_err(RunError::Io)?;
        state = serde_json::json!({
            "generation": crate::store::uuid_hex(),
            "last_activity": now
        });
    }
    if state
        .get("generation")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        state["generation"] = Value::String(crate::store::uuid_hex());
    }
    state["last_activity"] = serde_json::json!(now);
    crate::config::atomic_json(&state_path, &state).map_err(RunError::Io)?;
    let generation = state
        .get("generation")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if gray_bin.is_empty() {
        return Err(RunError::Io("gray executable is missing".to_string()));
    }
    let max_requests = config
        .get("max_requests")
        .and_then(Value::as_u64)
        .unwrap_or(200);
    let mut args: Vec<String> = vec![
        gray_bin.to_string(),
        "-p".to_string(),
        prompt.to_string(),
        "--json".to_string(),
        "--max-requests".to_string(),
        max_requests.to_string(),
    ];
    let session_id = state
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(sid) = &session_id {
        args.push("--session".to_string());
        args.push(sid.clone());
    }

    let mut cmd = tokio::process::Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.current_dir(&work);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    // Own process group so a timeout kill reaches grandchildren too:
    // setpgid + SIGKILL on unix, a fresh process group + taskkill /T on Windows.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        let Some(k_str) = k.to_str() else { continue };
        if k_str.starts_with("GRAY_")
            || k_str.starts_with("MATRIX_")
            || k_str.starts_with("OPENAI_")
        {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("GRAY_HOME", &home);
    if !gray_home.is_empty() {
        cmd.env("GRAY_USER_HOME", gray_home);
    }
    cmd.env(
        "PATH",
        child_path(
            std::env::var_os("PATH"),
            crate::config::user_home(),
            Path::new(gray_bin),
        ),
    );
    cmd.env("GRAY_SKILLS_ONLY", "1");
    cmd.env("GRAY_STDIN_STEER", "1");
    // Tool narration is safe to show; raw model reasoning is not.
    cmd.env("GRAY_SHOW_REASONING", "0");
    cmd.env("GRAY_MAX_WALL_SECS", (timeout_secs.max(1)).to_string());

    let mut child = cmd
        .spawn()
        .map_err(|_| RunError::Spawn("spawn failed".to_string()))?;
    let pid = child.id().unwrap_or(0);
    let mut group = ProcessGroup::new(pid);
    let live = Live::register(conversation, child.stdin.take());

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunError::Spawn("no stdout".to_string()))?;
    let consume = consume_ndjson(stdout, &state_path, &generation, &mut state, &live);
    let outcome: Result<Consume, RunError> =
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), consume).await {
            Ok(r) => r,
            Err(_) => {
                // Timeout: kill the whole process tree.
                if pid != 0 {
                    kill_process_tree(pid);
                }
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
                group.disarm();
                return Err(RunError::Timeout(timeout_secs));
            }
        };
    let status = child.wait().await;
    group.disarm();
    let status = status.map_err(|_| RunError::Spawn("wait failed".to_string()))?;
    let consumed = outcome?;

    // gray reports its request cap as a generic error row; the accounting
    // beside it says whether the cap is what stopped the turn.
    if let Some(row) = consumed.terminal.as_ref().filter(|r| r["type"] == "error") {
        if row["accounting"]["requests"].as_u64() >= Some(max_requests) {
            return Err(RunError::RequestLimit(max_requests));
        }
        if let Some(reason) = row
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.trim().is_empty())
        {
            return Err(RunError::Provider(reason.to_string()));
        }
    }
    if !status.success() {
        return Err(RunError::Exit(status.code()));
    }
    if !consumed.protocol_ok {
        return Err(RunError::Protocol(
            "Invalid agent JSON output; upgrade gray to a compatible version".to_string(),
        ));
    }
    let Some(final_row) = consumed.terminal else {
        return Err(RunError::Incomplete);
    };
    if final_row.get("type").and_then(Value::as_str) != Some("result")
        || final_row
            .get("session_id")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(RunError::Incomplete);
    }
    let answer = match final_row.get("text").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => Ok(t.to_string()),
        _ => Err(RunError::Empty),
    };
    // Drop the lock before the async state machine is suspended again.
    drop(_lock);
    answer
}

/// Drain the child's NDJSON stdout: protocol 1 rows, one turn, terminal
/// `result`/`error`, session pointer pinned into `state`.
async fn consume_ndjson(
    stdout: tokio::process::ChildStdout,
    state_path: &Path,
    generation: &str,
    state: &mut Value,
    live: &Live,
) -> Result<Consume, RunError> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stdout).lines();
    let mut turn_id: Option<String> = None;
    let mut terminal: Option<Value> = None;
    let mut protocol_ok = true;
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|_| RunError::Io("cannot read agent output".to_string()))?;
        let Some(line) = line else { break };
        if line.len() > LINE_CAP {
            protocol_ok = false;
            continue;
        }
        let row: Value = match serde_json::from_str(&line) {
            Ok(Value::Object(map)) => Value::Object(map),
            _ => {
                protocol_ok = false;
                continue;
            }
        };
        if row.get("protocol").and_then(Value::as_i64) != Some(1) {
            protocol_ok = false;
            continue;
        }
        let row_turn = row.get("turn_id").and_then(Value::as_str).unwrap_or("");
        if row_turn.is_empty() || (turn_id.is_some() && turn_id.as_deref() != Some(row_turn)) {
            protocol_ok = false;
            continue;
        }
        turn_id = Some(row_turn.to_string());
        // Data after the terminal row is a protocol violation, not a
        // second result.
        if terminal.is_some() {
            protocol_ok = false;
            continue;
        }
        if let Some(sid) = row.get("session_id").and_then(Value::as_str) {
            if session_id_valid(sid) {
                // `/new` advances the on-disk generation while an old child
                // may still be finishing. Never let that child resurrect its
                // predecessor's pointer.
                if !crate::session::generation_is_current(state_path, generation) {
                    protocol_ok = false;
                    continue;
                }
                let pinned = state
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !pinned.is_empty() && pinned != sid {
                    protocol_ok = false;
                    continue;
                }
                state["session_id"] = Value::String(sid.to_string());
                let _ = crate::config::atomic_json(state_path, state);
            } else {
                protocol_ok = false;
                continue;
            }
        }
        match row.get("type").and_then(Value::as_str) {
            Some("result") | Some("error") => {
                live.end();
                terminal = Some(row);
            }
            Some("progress") | Some("text") | Some("retract") => {}
            _ => {
                protocol_ok = false;
            }
        }
    }
    Ok(Consume {
        terminal,
        protocol_ok,
    })
}

fn session_id_valid(s: &str) -> bool {
    // gray names sessions with readable lowercase words
    // (`ionic-nickel-supernova`); older releases emitted canonical UUIDs.
    // Both are dash-separated lowercase [a-z0-9] runs; keep the check
    // structural so a word id never trips a protocol error.
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 64 || b[0] == b'-' || b[b.len() - 1] == b'-' {
        return false;
    }
    let mut prev_dash = false;
    for c in b {
        let dash = *c == b'-';
        if (dash && prev_dash) || !(dash || c.is_ascii_lowercase() || c.is_ascii_digit()) {
            return false;
        }
        prev_dash = dash;
    }
    true
}

#[cfg(test)]
mod session_id_tests {
    use super::session_id_valid;

    #[test]
    fn accepts_uuid_and_word_ids() {
        assert!(session_id_valid("cff0d620-d17c-456a-bb6c-d24af7ddf859"));
        assert!(session_id_valid("ionic-nickel-supernova"));
        assert!(session_id_valid("abc"));
    }

    #[test]
    fn rejects_junk() {
        for bad in [
            "",
            "-leading",
            "trailing-",
            "double--dash",
            "Upper-Case",
            "has space",
            "semi;colon",
        ] {
            assert!(!session_id_valid(bad), "{bad:?}");
        }
        assert!(!session_id_valid(&"a".repeat(65)));
    }
}
