use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

pub const STEERING: &str = "Steering the current turn.";
pub const IDLE_STEER: &str = "Nothing running — sent as a new message.";
pub const STEER_USAGE: &str = "Usage: /steer <text>";
pub const STOPPED: &str = "Stopped.";
pub const NOTHING_RUNNING: &str = "Nothing running.";
pub const BUSY_RETRY_SECS: f64 = 2.0;
const ACK_EVERY_SECS: f64 = 30.0;

pub fn steer_mode(config: &Value) -> bool {
    config.get("busy_input_mode").and_then(Value::as_str) == Some("steer")
}

pub fn command<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let t = text.trim();
    let body = t.strip_prefix('/').or_else(|| t.strip_prefix('!'))?;
    let rest = body.strip_prefix(name)?;
    if rest.is_empty() {
        return Some("");
    }
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

pub fn page_number(args: &str) -> usize {
    if args.trim().is_empty() {
        1
    } else {
        args.trim().parse().unwrap_or(0)
    }
}

#[derive(Default)]
pub struct Acks(Mutex<HashMap<String, f64>>);

impl Acks {
    pub fn queued(&self, conversation: &str, ahead: i64, now: f64) -> Option<String> {
        if ahead <= 0 {
            return None;
        }
        let mut seen = self.0.lock().ok()?;
        if seen
            .get(conversation)
            .is_some_and(|t| now - t < ACK_EVERY_SECS)
        {
            return None;
        }
        seen.insert(conversation.to_string(), now);
        Some(format!("Queued — {ahead} ahead."))
    }
}

fn boundary(text: &str, limit: usize) -> usize {
    let mut cut = limit.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    if cut == 0 {
        cut = text
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(text.len());
    }
    cut
}

pub fn paginate(text: &str, limit: usize, max_lines: usize) -> Vec<String> {
    let mut pieces: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        let mut rest = line;
        while rest.len() > limit {
            let cut = boundary(rest, limit);
            pieces.push(&rest[..cut]);
            rest = &rest[cut..];
        }
        pieces.push(rest);
    }
    let mut pages: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut lines = 0usize;
    let mut fence: Option<String> = None;
    for piece in pieces {
        if lines > 0 && (cur.len() + 1 + piece.len() > limit || lines >= max_lines) {
            if fence.is_some() {
                cur.push_str("\n```");
            }
            pages.push(std::mem::take(&mut cur));
            lines = 0;
            if let Some(lang) = &fence {
                cur = format!("```{lang}");
                lines = 1;
            }
        }
        if lines > 0 {
            cur.push('\n');
        }
        cur.push_str(piece);
        lines += 1;
        if let Some(marker) = piece.trim_start().strip_prefix("```") {
            fence = match fence {
                None => Some(marker.trim().to_string()),
                Some(_) => None,
            };
        }
    }
    if !cur.trim().is_empty() || pages.is_empty() {
        pages.push(cur);
    }
    pages
}

pub struct Pager {
    limit: usize,
    max_lines: usize,
    prefix: &'static str,
    pages: Mutex<HashMap<String, Vec<String>>>,
}

impl Pager {
    pub fn new(limit: usize, max_lines: usize, prefix: &'static str) -> Self {
        Pager {
            limit,
            max_lines,
            prefix,
            pages: Mutex::new(HashMap::new()),
        }
    }

    fn render(&self, pages: &[String], index: usize) -> String {
        let total = pages.len();
        if index + 1 < total {
            format!(
                "{}\n\npage {}/{total} — {}page {}",
                pages[index].trim_end(),
                index + 1,
                self.prefix,
                index + 2
            )
        } else {
            format!("{}\n\npage {}/{total}", pages[index].trim_end(), index + 1)
        }
    }

    pub fn first(&self, conversation: &str, text: &str) -> String {
        let pages = paginate(text, self.limit, self.max_lines);
        if pages.len() < 2 {
            return text.to_string();
        }
        let out = self.render(&pages, 0);
        if let Ok(mut all) = self.pages.lock() {
            all.insert(conversation.to_string(), pages);
        }
        out
    }

    pub fn page(&self, conversation: &str, number: usize) -> String {
        let Ok(all) = self.pages.lock() else {
            return "Nothing to page.".to_string();
        };
        let Some(pages) = all.get(conversation) else {
            return "Nothing to page.".to_string();
        };
        if number == 0 || number > pages.len() {
            return format!("No page {number} — pages 1-{}.", pages.len());
        }
        self.render(pages, number - 1)
    }
}

pub fn help_suffix(prefix: &str) -> String {
    format!(
        "\n\nGateway: {prefix}steer <text> — steer the running turn · {prefix}stop — cancel the running turn · {prefix}page N — page N of the last long reply"
    )
}

pub async fn cancellable<F: std::future::Future>(
    fut: F,
    cancelled: impl Fn() -> bool,
) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    loop {
        tokio::select! {
            out = &mut fut => return Some(out),
            _ = tick.tick() => {
                if cancelled() {
                    return None;
                }
            }
        }
    }
}
