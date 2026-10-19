//! Interactive setup wizard.
//! Credentials stay in the wizard; the model never receives the password.
use serde_json::{json, Value};
use std::path::Path;

/// Terminal IO. The real implementation talks to stdin/stderr; tests inject
/// a scripted fake. `print_line` goes to user-visible output (never logs).
pub trait Prompter {
    fn is_terminal(&self) -> bool;
    fn prompt(&mut self, text: &str) -> Result<String, String>;
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String>;
    fn confirm(&mut self, text: &str) -> Result<bool, String>;
    fn print_line(&mut self, text: &str);
}

/// Real terminal prompter (hidden input via `stty -echo`, no new deps).
pub struct Tty;

impl Prompter for Tty {
    fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    fn prompt(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let res = without_echo(|| std::io::stdin().read_line(&mut line));
        let _ = std::io::stderr().write_all(b"\n");
        res.map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim().to_string())
    }
    fn confirm(&mut self, text: &str) -> Result<bool, String> {
        let answer = self.prompt(text)?.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
    fn print_line(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// Echo off for one read. Ctrl-C would otherwise kill setup with the
/// terminal still silent, so SIGINT restores it first (tcsetattr is
/// async-signal-safe).
#[cfg(unix)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
    extern "C" fn restore_and_exit(_: libc::c_int) {
        if let Some(t) = SAVED.get() {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
        unsafe { libc::_exit(130) };
    }
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| term);
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    let handler = restore_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let old = unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &quiet);
        libc::signal(libc::SIGINT, handler)
    };
    let out = read();
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        libc::signal(libc::SIGINT, old);
    }
    out
}

/// Windows echo off for one read: clear ENABLE_ECHO_INPUT on the console
/// input buffer. A console control handler puts the mode back before the
/// default Ctrl-C processing kills setup, same shape as the unix version.
#[cfg(windows)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    use std::ffi::c_void;
    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;
    const STD_INPUT_HANDLE: Dword = -10i32 as Dword;
    const ENABLE_ECHO_INPUT: Dword = 0x0004;
    extern "system" {
        fn GetStdHandle(which: Dword) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut Dword) -> Bool;
        fn SetConsoleMode(handle: Handle, mode: Dword) -> Bool;
        fn SetConsoleCtrlHandler(
            handler: Option<extern "system" fn(Dword) -> Bool>,
            add: Bool,
        ) -> Bool;
    }
    static SAVED: std::sync::OnceLock<Dword> = std::sync::OnceLock::new();
    extern "system" fn restore(_: Dword) -> Bool {
        if let Some(&mode) = SAVED.get() {
            unsafe {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
            }
        }
        0
    }
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if input.is_null() {
        return read();
    }
    let mut mode: Dword = 0;
    if unsafe { GetConsoleMode(input, &mut mode) } == 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| mode);
    unsafe {
        SetConsoleMode(input, saved & !ENABLE_ECHO_INPUT);
        SetConsoleCtrlHandler(Some(restore), 1);
    }
    let out = read();
    unsafe {
        SetConsoleMode(input, saved);
        SetConsoleCtrlHandler(Some(restore), 0);
    }
    out
}

#[cfg(not(any(unix, windows)))]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    read()
}

/// Resolve the gray binary (`GRAY_BIN` env or `PATH` lookup).
fn resolve_gray(explicit: &str) -> Option<String> {
    let candidate = explicit.trim();
    if candidate.is_empty() {
        return None;
    }
    if candidate.contains('/') || candidate.contains('\\') {
        let path = Path::new(candidate);
        if path.is_file() && is_executable(path) {
            return Some(candidate.to_string());
        }
        return None;
    }
    // Executables carry extensions on Windows (`gray` is `gray.exe`).
    let names: Vec<String> = {
        #[cfg(windows)]
        {
            [".exe", ".cmd", ".bat", ".com", ".ps1", ""]
                .iter()
                .map(|ext| format!("{candidate}{ext}"))
                .collect()
        }
        #[cfg(not(windows))]
        {
            vec![candidate.to_string()]
        }
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for name in &names {
            let full = dir.join(name);
            if full.is_file() && is_executable(&full) {
                return Some(full.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn absolutize(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn shellexpand_home(raw: &str) -> String {
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = crate::config::user_home() {
            return if raw == "~" {
                home.to_string_lossy().into_owned()
            } else {
                home.join(&raw[2..]).to_string_lossy().into_owned()
            };
        }
    }
    raw.to_string()
}

/// Split a comma-separated prompt answer into trimmed non-empty entries.
fn csv(answer: &str) -> Vec<String> {
    answer
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Dedupe keeping order, case-insensitively (mxids).
fn merge(prior: Vec<String>, extra: &[String]) -> Vec<String> {
    let mut out = prior;
    for e in extra {
        if !out.iter().any(|u| u.eq_ignore_ascii_case(e)) {
            out.push(e.clone());
        }
    }
    out
}

/// Normalize a homeserver answer: bare hosts get https://.
fn normalize_homeserver(raw: &str) -> Result<String, String> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Err("Homeserver URL is required".to_string());
    }
    let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    if !crate::config::homeserver_valid(&Value::String(with_scheme.clone())) {
        return Err("Homeserver must be an http(s) base URL".to_string());
    }
    Ok(with_scheme)
}

/// The wizard: homeserver, credentials, rooms, the allowlist.
/// A live `whoami`/`login` proves the answers before anything is called done.
pub async fn run(path: &Path, io: &mut dyn Prompter) -> Result<bool, String> {
    if !io.is_terminal() {
        return Err("Setup needs a terminal for hidden password input".to_string());
    }
    if path.exists() && !io.confirm("Replace existing configuration? [y/N] ")? {
        return Ok(false);
    }
    // Gray's binary and home are resolved silently (env, then the
    // conventional defaults) — derived facts, not questions.
    let gray = resolve_gray(&std::env::var("GRAY_BIN").unwrap_or_else(|_| "gray".to_string()))
        .ok_or_else(|| "Install gray first; executable not found".to_string())?;
    let gray_home = absolutize(Path::new(&shellexpand_home(
        &std::env::var("GRAY_HOME").unwrap_or_else(|_| "~/.gray".to_string()),
    )));

    // Whoever the old config allowed stays allowed: the wizard only adds.
    let prior = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object);
    let prior_allowed: Vec<String> = prior
        .as_ref()
        .and_then(|p| p.get("allowed_users"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    for line in [
        "Connect gray to a Matrix homeserver (matrix.org, your own Synapse, ...).",
        "Plain rooms only — end-to-end encrypted rooms are not supported.",
        "The answers are checked against the live server before saving.",
    ] {
        io.print_line(line);
    }

    let homeserver_answer = io.prompt("Homeserver URL (e.g. https://matrix.org): ")?;
    let homeserver = normalize_homeserver(&homeserver_answer)?;

    // Auth: an existing access token, or password login for one.
    io.print_line(
        "Authentication: paste an existing access token, or leave it blank \
         to log in with the account password (the password is never stored).",
    );
    let token_answer = io.prompt_hidden("Access token (blank = password login): ")?;
    let (access_token, user_id) = if token_answer.trim().is_empty() {
        let user = io.prompt("Matrix user (@bot:server.org or bot): ")?;
        let user = user.trim().to_string();
        if user.is_empty() {
            return Err("A Matrix user is required".to_string());
        }
        let password = io.prompt_hidden("Password: ")?;
        if password.is_empty() {
            return Err("Password is empty".to_string());
        }
        let (token, mxid) = crate::matrix::login(&homeserver, &user, &password)
            .await
            .map_err(|e| format!("Login failed: {e}"))?;
        (token, mxid)
    } else {
        let token = token_answer.trim().to_string();
        let rest = crate::matrix::Rest::new(&homeserver, &token);
        let mxid = rest
            .whoami()
            .await
            .map_err(|e| format!("Token check failed: {e}"))?;
        (token, mxid)
    };
    io.print_line(&format!("Authenticated as {user_id}."));

    let rooms_answer = io.prompt(
        "Rooms the bot may answer in, comma-separated (!id:server or #alias:server): ",
    )?;
    let rooms = csv(&rooms_answer);
    if rooms.is_empty()
        || !rooms
            .iter()
            .all(|r| crate::config::room_valid(&Value::String(r.clone())))
    {
        return Err(
            "Rooms must be Matrix room IDs (!id:server) or aliases (#room:server)".to_string(),
        );
    }

    io.print_line(
        "Access control: only allowlisted Matrix IDs can trigger the bot. Anyone \
         else who DMs it gets a pairing code the owner can approve.",
    );
    let allowed_answer = io.prompt(
        "Allowed users (comma-separated @you:server; first is usually yours — empty admits nobody): ",
    )?;
    let allowed = merge(prior_allowed, &csv(&allowed_answer));
    if !allowed
        .iter()
        .all(|u| crate::config::user_id_valid(&Value::String(u.clone())))
    {
        return Err("Allowed users must be Matrix user IDs (@user:server)".to_string());
    }

    let default_home = rooms[0].clone();
    let home_answer = io.prompt(&format!("Home room for cron/notices [{default_home}]: "))?;
    let home_room = if home_answer.trim().is_empty() {
        default_home
    } else {
        let h = home_answer.trim().to_string();
        if !crate::config::room_valid(&Value::String(h.clone())) {
            return Err("Home room must be a Matrix room ID or alias".to_string());
        }
        h
    };

    io.print_line(
        "In shared rooms the bot answers only when mentioned; DMs and \
         free-response rooms always turn.",
    );
    let free_answer = io.prompt(
        "Free-response rooms (no mention needed), comma-separated [none]: ",
    )?;
    let free_rooms = csv(&free_answer);
    if !free_rooms
        .iter()
        .all(|r| crate::config::room_valid(&Value::String(r.clone())))
    {
        return Err("Free-response rooms must be Matrix room IDs or aliases".to_string());
    }

    // `path.parent` is the state dir; like the other adapters, the saved
    // config holds absolute paths or `save_config` validation rejects it.
    let workdir = path
        .parent()
        .map(|p| absolutize(p).to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let mut saved = json!({
        "homeserver": homeserver,
        "user_id": user_id,
        "access_token": access_token,
        "rooms": rooms,
        "home_room": home_room,
        "require_mention": true,
        "allowed_users": allowed,
        "gray_bin": absolutize(Path::new(&gray)).to_string_lossy(),
        "gray_home": gray_home.to_string_lossy(),
        "workdir": workdir,
        "session_reset": {
            "mode": "both",
            "idle_minutes": 1_440,
            "at_hour": 4
        }
    });
    if !free_rooms.is_empty() {
        saved["free_response_rooms"] = Value::Array(
            free_rooms.into_iter().map(Value::String).collect(),
        );
    }
    // A re-run replaces only what the wizard asked about: limits and other
    // hand-tuned keys survive (unless they no longer validate).
    if let Some(mut merged) = prior {
        for (k, v) in saved.as_object().into_iter().flatten() {
            if k != "session_reset" || merged.get(k).is_none() {
                merged[k] = v.clone();
            }
        }
        if crate::config::validate_config(&merged).is_ok() {
            saved = merged;
        }
    }
    crate::config::save_config(path, &saved)?;
    io.print_line("Configuration saved privately.");

    // Prove it before anyone is told it worked: whoami plus a room
    // membership check via the doctor.
    crate::doctor::doctor(&saved).await.map_err(|e| {
        format!("Saved, but the doctor disagreed: {e}. Fix it, then run `gray matrix doctor` and `gray matrix install`.")
    })?;
    io.print_line("Doctor verified the configuration.");
    Ok(true)
}
