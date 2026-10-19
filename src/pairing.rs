//! Matrix-side pairing. A user who DMs a bot that does not admit them is
//! told their own mxid and a one-shot code; the owner approves the code with
//! `gray matrix pairing approve matrix <code>` and that mxid joins the
//! allowlist. No terminal wizard, no 5-minute timer, nothing to hunt — the
//! discovery happens where the user already is.

use serde_json::Value;
use std::path::Path;

/// A fresh code: 8 uppercase alphanumerics (short enough
/// to read off a phone, 36^8 space, single-use).
pub fn gen_code() -> String {
    const ALPHABET: &[u8; 36] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut bytes = [0u8; 8];
    crate::store::random_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

/// The reply an un-admitted DMer sees. Matrix user ids are cryptographic —
/// whoever controls the account controls the identity.
pub fn unconfigured_reply(mxid: &str, code: &str) -> String {
    format!(
        "access not configured.\n\
         Your Matrix ID: {mxid}\n\
         Pairing code: {code}\n\
         Ask the bot owner to approve with:\n\
         gray matrix pairing approve matrix {code}"
    )
}

/// What the gateway does when a message is not admitted and the sender is a
/// human DMing the bot: mint (or reuse) a code and reply. Room messages
/// and admitted senders never see this — `None` for them.
pub struct PairingReply {
    pub text: String,
    #[allow(dead_code)]
    pub code: String,
}

pub fn reply_for(store: &crate::store::Store, mxid: &str, dm: bool) -> Option<PairingReply> {
    if !dm || mxid.is_empty() {
        return None;
    }
    let code = store
        .pairing_code_for(mxid)
        .ok()
        .flatten()
        .unwrap_or_else(gen_code);
    store.pairing_insert(&code, mxid).ok()?;
    Some(PairingReply {
        text: unconfigured_reply(mxid, &code),
        code,
    })
}

/// Consume a code and admit its mxid into `allowed_users`. Config is
/// rewritten privately (0600) exactly as the wizard would.
pub fn approve(config_path: &Path, platform: &str, code: &str) -> Result<String, String> {
    if platform != "matrix" {
        return Err(format!(
            "unknown platform '{platform}'; the only one is matrix"
        ));
    }
    let store = crate::gateway::open_store(config_path)?;
    let mxid = store
        .pairing_take(code.trim())?
        .ok_or_else(|| "unknown or already-used pairing code".to_string())?;
    if !crate::config::user_id_valid(&Value::String(mxid.clone())) {
        return Err("pairing code did not resolve to a Matrix user ID".to_string());
    }
    let mut config = crate::config::load_config(config_path)?;
    let mut allowed: Vec<String> = config
        .get("allowed_users")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if allowed.iter().any(|u| u.eq_ignore_ascii_case(&mxid)) {
        return Ok(format!("{mxid} was already allowed"));
    }
    allowed.push(mxid.clone());
    config["allowed_users"] = Value::Array(allowed.into_iter().map(Value::String).collect());
    crate::config::save_config(config_path, &config)?;
    Ok(format!(
        "{mxid} can now trigger the bot; restart the gateway to apply (it re-reads on start)"
    ))
}
