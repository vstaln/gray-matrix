//! Matrix Client-Server transport: password `/login` plus the six calls the
//! bridge needs — `whoami`, long-poll `/sync`, `send/m.room.message`,
//! `typing`, `join`, alias `/directory/room` resolution and `joined_rooms`.
//! Raw reqwest against the C-S REST API, not matrix-sdk: the surface here
//! is ~8 endpoints and an SDK would drag in the E2EE stack this bridge
//! deliberately does not support.
//!
//! Auth is `Authorization: Bearer <token>`; nothing here logs a token.
//! `/sync` semantics lifted from the Hermes adapter
//! (`plugins/platforms/matrix`): a persisted `since` cursor, a bounded
//! long-poll, `M_LIMIT_EXCEEDED` honored via `retry_after_ms`, and every
//! request capped so a wedged socket cannot freeze the loop.
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// `/sync` long-poll: the homeserver holds the request this long.
pub const LONG_POLL_MS: u64 = 30_000;
/// One outbound event's body budget (chars). The homeserver refuses events
/// whose canonical JSON exceeds ~65 KiB (M_TOO_LARGE); plain text stays far
/// under that at this bound even after HTML expansion.
pub const MAX_MESSAGE_CHARS: usize = 8000;

#[derive(Debug, Clone)]
pub enum TransportError {
    Auth(String),
    Forbidden(String),
    RateLimited(Option<f64>),
    Http(u16, String),
    Net(String),
    Invalid(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(_) => {
                f.write_str("Matrix authentication failed; check the access token")
            }
            Self::Forbidden(_) => {
                f.write_str("Matrix refused the request; the account may lack power in the room")
            }
            Self::RateLimited(_) => f.write_str("Matrix rate limit hit; backing off"),
            Self::Http(status @ (400 | 404 | 413), detail) => {
                write!(f, "Matrix rejected the request (HTTP {status}): {detail}")
            }
            Self::Http(status, _) => write!(f, "Matrix HTTP {status}"),
            Self::Net(_) => f.write_str("Matrix request failed; check connectivity"),
            Self::Invalid(msg) => f.write_str(msg),
        }
    }
}
impl std::error::Error for TransportError {}

impl TransportError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited(_) | Self::Net(_) => true,
            Self::Http(s, _) => *s == 429 || *s >= 500,
            _ => false,
        }
    }
}

/// A request that has not answered in this long is given up as a network
/// error (retried by its caller), never waited on forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Long-poll reads outlast the normal budget by the poll itself plus slack.
const POLL_TIMEOUT: Duration = Duration::from_millis(LONG_POLL_MS + 15_000);

/// Percent-encode one path segment. Room ids (`!id:server`), aliases
/// (`#a:server`) and event ids can carry `!#:+/` — anything outside the
/// unreserved set is escaped.
fn enc(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if ok {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Matrix's error body is `{errcode: "M_*", error: "human text"}` and
/// describes our request, never credentials.
fn error_detail(body: &Value) -> String {
    let text = body
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("request failed");
    match body.get("errcode").and_then(Value::as_str) {
        Some(code) => format!("{code}: {text}"),
        None => text.to_string(),
    }
}

fn retry_after(body: &Value) -> Option<f64> {
    body.get("retry_after_ms")
        .and_then(Value::as_f64)
        .map(|ms| ms / 1000.0)
}

/// Thin Client-Server API client. `base` comes from config (the homeserver);
/// every method hangs its path off `{base}/_matrix/client/v3`.
#[derive(Debug, Clone)]
pub struct Rest {
    base: String,
    client: reqwest::Client,
    /// Per-process transaction ids: unique within the access token's life.
    txn: std::sync::Arc<AtomicU64>,
}

impl Rest {
    pub fn new(homeserver: &str, token: &str) -> Self {
        use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("gray-matrix/0.1"));
        if let Ok(bearer) = HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert(AUTHORIZATION, bearer);
        }
        // Every request is bounded. Without this a pooled connection that
        // died under us (a laptop's wifi, a NAT timeout) leaves a request
        // waiting forever, and the turn's messages freeze with it.
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builds");
        Self {
            base: format!("{}/_matrix/client/v3", homeserver.trim_end_matches('/')),
            client,
            txn: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    /// A client from loaded config; `None` when setup has not run.
    pub fn from_config(config: &Value) -> Option<Self> {
        let base = config.get("homeserver").and_then(Value::as_str)?;
        let token = crate::config::access_token(config)?;
        Some(Self::new(base, &token))
    }

    fn next_txn(&self) -> String {
        let n = self.txn.fetch_add(1, Ordering::Relaxed) + 1;
        format!("g{n}x{}", std::process::id())
    }

    async fn classify(&self, resp: reqwest::Response) -> Result<Value, TransportError> {
        let code = resp.status().as_u16();
        let body = resp.json::<Value>().await.unwrap_or(Value::Null);
        if code == 401 {
            return Err(TransportError::Auth(error_detail(&body)));
        }
        if code == 403 {
            return Err(TransportError::Forbidden(error_detail(&body)));
        }
        if code == 429 {
            return Err(TransportError::RateLimited(retry_after(&body)));
        }
        if !(200..300).contains(&code) {
            return Err(TransportError::Http(code, error_detail(&body)));
        }
        Ok(body)
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, TransportError> {
        let resp = self
            .client
            .get(format!("{}{path}", self.base))
            .query(query)
            .send()
            .await
            .map_err(|_| TransportError::Net("request failed".into()))?;
        self.classify(resp).await
    }

    async fn put(&self, path: &str, body: &Value) -> Result<Value, TransportError> {
        let resp = self
            .client
            .put(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .map_err(|_| TransportError::Net("request failed".into()))?;
        self.classify(resp).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, TransportError> {
        let resp = self
            .client
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .map_err(|_| TransportError::Net("request failed".into()))?;
        self.classify(resp).await
    }

    /// `GET /sync` — the durable cursor is `since`; `timeout_ms` long-polls.
    /// The filter narrows the payload to what the bridge reads: room
    /// messages, invites, and the `m.direct` account map.
    pub async fn sync(
        &self,
        since: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Value, TransportError> {
        let filter = json!({
            "presence": {"types": []},
            "account_data": {"types": ["m.direct"]},
            "room": {
                "state": {"types": []},
                "ephemeral": {"types": []},
                "account_data": {"types": []},
                "timeline": {"types": ["m.room.message"], "limit": 50},
                "invite_state": {"types": ["m.room.member"]}
            }
        });
        let mut query = vec![
            ("timeout", timeout_ms.to_string()),
            ("filter", filter.to_string()),
        ];
        if let Some(since) = since {
            query.push(("since", since.to_string()));
        }
        let resp = self
            .client
            .get(format!("{}/sync", self.base))
            .query(&query)
            .timeout(POLL_TIMEOUT)
            .send()
            .await
            .map_err(|_| TransportError::Net("sync failed".into()))?;
        self.classify(resp).await
    }

    /// `PUT /rooms/{room}/send/m.room.message/{txn}` — one `m.text` event
    /// with a `org.matrix.custom.html` formatted body. Returns the event id.
    pub async fn send_text(
        &self,
        room_id: &str,
        body: &str,
        html: &str,
    ) -> Result<Value, TransportError> {
        if body.trim().is_empty() || body.chars().count() > MAX_MESSAGE_CHARS {
            return Err(TransportError::Invalid(
                "Message must contain 1-8000 characters".into(),
            ));
        }
        let content = if html.trim().is_empty() {
            json!({"msgtype": "m.text", "body": body})
        } else {
            json!({
                "msgtype": "m.text",
                "body": body,
                "format": "org.matrix.custom.html",
                "formatted_body": html,
            })
        };
        self.put(
            &format!(
                "/rooms/{}/send/m.room.message/{}",
                enc(room_id),
                enc(&self.next_txn())
            ),
            &content,
        )
        .await
    }

    /// `PUT /rooms/{room}/typing/{user}` — the typing bubble. Best-effort.
    pub async fn typing(
        &self,
        room_id: &str,
        user_id: &str,
        typing: bool,
        timeout_ms: u64,
    ) -> Result<(), TransportError> {
        self.put(
            &format!("/rooms/{}/typing/{}", enc(room_id), enc(user_id)),
            &json!({"typing": typing, "timeout": timeout_ms}),
        )
        .await
        .map(|_| ())
    }

    /// `POST /join/{roomIdOrAlias}` — accept an invite or join an alias.
    pub async fn join(&self, room: &str) -> Result<String, TransportError> {
        let out = self
            .post(&format!("/join/{}", enc(room)), &json!({}))
            .await?;
        out.get("room_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Invalid("join returned no room_id".into()))
    }

    /// `GET /directory/room/{alias}` → room id. Aliases (`#a:server`) are
    /// resolved once at boot and on send; ids (`!id:server`) pass through.
    pub async fn resolve_room(&self, room: &str) -> Result<String, TransportError> {
        if room.starts_with('!') {
            return Ok(room.to_string());
        }
        let out = self
            .get(&format!("/directory/room/{}", enc(room)), &[])
            .await?;
        out.get("room_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Invalid("alias resolved to no room_id".into()))
    }

    /// `GET /joined_rooms` → room id list (doctor's membership check).
    pub async fn joined_rooms(&self) -> Result<Vec<String>, TransportError> {
        let out = self.get("/joined_rooms", &[]).await?;
        Ok(out
            .get("joined_rooms")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// `GET /account/whoami` — verifies the token and returns the mxid.
    pub async fn whoami(&self) -> Result<String, TransportError> {
        let out = self.get("/account/whoami", &[]).await?;
        out.get("user_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Invalid("whoami returned no user_id".into()))
    }
}

/// `POST /login` — password auth. Unauthenticated by definition, so it uses
/// a bare client; on success the caller stores the returned token. The
/// wizard writes `access_token`/`user_id`/`device_id` to the 0600 config.
pub async fn login(
    homeserver: &str,
    user: &str,
    password: &str,
) -> Result<(String, String), TransportError> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|_| TransportError::Net("client build failed".into()))?;
    let body = json!({
        "type": "m.login.password",
        "identifier": {"type": "m.id.user", "user": user},
        "password": password,
    });
    let resp = client
        .post(format!(
            "{}/_matrix/client/v3/login",
            homeserver.trim_end_matches('/')
        ))
        .json(&body)
        .send()
        .await
        .map_err(|_| TransportError::Net("login request failed".into()))?;
    let code = resp.status().as_u16();
    let parsed = resp.json::<Value>().await.unwrap_or(Value::Null);
    if !(200..300).contains(&code) {
        return Err(match code {
            401 | 403 => TransportError::Forbidden(error_detail(&parsed)),
            429 => TransportError::RateLimited(retry_after(&parsed)),
            _ => TransportError::Http(code.max(400), error_detail(&parsed)),
        });
    }
    let token = parsed
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| TransportError::Invalid("login returned no access_token".into()))?;
    let user_id = parsed
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or(user)
        .to_string();
    Ok((token.to_string(), user_id))
}

/// Read a config string.
pub fn cfg_str<'a>(cfg: &'a Value, key: &str) -> &'a str {
    cfg.get(key).and_then(Value::as_str).unwrap_or("")
}

/// One-shot delivery used by `matrix_send` and lifecycle notices: resolve
/// the room, render markdown to `org.matrix.custom.html`, send. Works with
/// or without the long-running daemon.
pub async fn standalone_send(cfg: &Value, room: &str, text: &str) -> Result<String, String> {
    let Some(rest) = Rest::from_config(cfg) else {
        return Err("matrix access token is not configured".to_string());
    };
    let room_id = rest
        .resolve_room(room)
        .await
        .map_err(|e| e.to_string())?;
    let chunks = crate::text::split_message(text, MAX_MESSAGE_CHARS);
    if chunks.is_empty() {
        return Err("text is empty".to_string());
    }
    for chunk in &chunks {
        let html = crate::text::md_to_html(chunk);
        rest.send_text(&room_id, chunk, &html)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(format!("Sent to {room}."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enc_escapes_matrix_ids() {
        assert_eq!(enc("!abc:example.org"), "%21abc%3Aexample.org");
        assert_eq!(enc("#room:example.org"), "%23room%3Aexample.org");
        assert_eq!(enc("$ev+id/x"), "%24ev%2Bid%2Fx");
        assert_eq!(enc("plain-1.2_3~4"), "plain-1.2_3~4");
    }
}
