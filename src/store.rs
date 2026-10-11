//! Durable SQLite inbox: inbound Matrix events survive a daemon restart.
//! `queued` -> `running` -> `done`/`failed`; `recover` re-queues `running`.
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS inbox (
    id TEXT PRIMARY KEY, target TEXT NOT NULL, sender TEXT NOT NULL,
    conversation TEXT NOT NULL,
    text TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'queued',
    created REAL NOT NULL, reply TEXT, error TEXT);
CREATE TABLE IF NOT EXISTS pairing (
    code TEXT PRIMARY KEY, mxid TEXT NOT NULL, created REAL NOT NULL);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
";

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    /// The room the reply goes to.
    pub target: String,
    pub sender: String,
    pub conversation: String,
    pub text: String,
    pub state: String,
}

pub struct Store {
    conn: Mutex<Connection>,
}

pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

pub fn uuid_hex() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut bytes);
    } else {
        // Fallback: time + pid entropy — ids only need uniqueness.
        let nanos = now_secs().to_bits();
        bytes[..8].copy_from_slice(&nanos.to_le_bytes());
        bytes[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn map_err(e: rusqlite::Error) -> String {
    format!("store error: {e}")
}

fn row_to_item(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: r.get(0)?,
        target: r.get(1)?,
        sender: r.get(2)?,
        conversation: r.get(3)?,
        text: r.get(4)?,
        state: r.get(5)?,
    })
}

const COLUMNS: &str = "id,target,sender,conversation,text,state";

impl Store {
    pub fn new(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "cannot create state dir".to_string())?;
        }
        let conn = Connection::open(path).map_err(map_err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(map_err)?;
        conn.execute_batch(SCHEMA).map_err(map_err)?;
        let _ = conn.execute(
            "ALTER TABLE inbox ADD COLUMN cancel INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE inbox ADD COLUMN retry_at REAL NOT NULL DEFAULT 0",
            [],
        );
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Default location beside the config file.
    pub fn default_path(config_path: &Path) -> PathBuf {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("matrix.sqlite")
    }

    /// Accept one inbound event under a caller-chosen id (`mx:<event_id>`).
    /// `Err` when the queue is at capacity; `Ok(false)` when the id was
    /// already stored — a replayed sync batch dedupes that way.
    pub fn enqueue(
        &self,
        id: &str,
        target: &str,
        sender: &str,
        conversation: &str,
        text: &str,
        capacity: u64,
    ) -> Result<bool, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let pending: i64 = conn
            .query_row("SELECT COUNT(*) FROM inbox WHERE state='queued'", [], |r| {
                r.get(0)
            })
            .map_err(map_err)?;
        if pending >= capacity.max(1) as i64 {
            return Err("queue is full".to_string());
        }
        let n = conn
            .execute(
                "INSERT OR IGNORE INTO inbox (id,target,sender,conversation,text,state,created) \
                 VALUES (?1,?2,?3,?4,?5,'queued',?6)",
                rusqlite::params![id, target, sender, conversation, text, now_secs()],
            )
            .map_err(map_err)?;
        Ok(n > 0)
    }

    pub fn pending_count(&self) -> Result<i64, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .query_row(
                "SELECT COUNT(*) FROM inbox WHERE state='queued'",
                [],
                |r| r.get(0),
            )
            .map_err(map_err)
    }

    /// Oldest queued item whose conversation has nothing running or older
    /// still queued, atomically marked `running`.
    pub fn claim(&self) -> Result<Option<Item>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let row = conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM inbox q WHERE state='queued' AND retry_at<=?1 \
                     AND NOT EXISTS (SELECT 1 FROM inbox r WHERE r.conversation=q.conversation \
                     AND (r.state='running' OR (r.state='queued' AND r.created<q.created))) \
                     ORDER BY created LIMIT 1"
                ),
                [now_secs()],
                row_to_item,
            )
            .ok();
        match row {
            Some(item) => {
                conn.execute(
                    "UPDATE inbox SET state='running', cancel=0 WHERE id=?1",
                    rusqlite::params![item.id],
                )
                .map_err(map_err)?;
                Ok(Some(Item {
                    state: "running".to_string(),
                    ..item
                }))
            }
            None => Ok(None),
        }
    }

    pub fn requeue_busy(&self, id: &str, after_secs: f64) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='queued', retry_at=?2 WHERE id=?1 AND state='running'",
                rusqlite::params![id, now_secs() + after_secs],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn ahead(&self, conversation: &str, id: &str) -> Result<i64, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .query_row(
                "SELECT COUNT(*) FROM inbox WHERE conversation=?1 AND id<>?2 \
                 AND state IN ('queued','running')",
                rusqlite::params![conversation, id],
                |r| r.get(0),
            )
            .map_err(map_err)
    }

    /// Put a claimed item back (a Busy conversation holds its lock, or the
    /// connection dropped before delivery).
    pub fn requeue(&self, id: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='queued' WHERE id=?1 AND state='running'",
                rusqlite::params![id],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn complete(&self, id: &str, reply: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='done', reply=?2 WHERE id=?1",
                rusqlite::params![id, reply],
            )
            .map_err(map_err)?;
        Ok(())
    }

    pub fn fail(&self, id: &str, error: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='failed', error=?2 WHERE id=?1",
                rusqlite::params![id, error],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// List queued/running items for `queue list`.
    pub fn items(&self) -> Result<Vec<Item>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM inbox \
                 WHERE state IN ('queued','running') ORDER BY created"
            ))
            .map_err(map_err)?;
        let rows = stmt.query_map([], row_to_item).map_err(map_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_err)
    }

    /// Items with a turn in flight, for shutdown notices.
    pub fn running_targets(&self) -> Result<Vec<Item>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let mut stmt = conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM inbox WHERE state='running' ORDER BY created"
            ))
            .map_err(map_err)?;
        let rows = stmt.query_map([], row_to_item).map_err(map_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_err)
    }

    /// Cancel a still-queued item.
    pub fn cancel(&self, id: &str) -> Result<bool, String> {
        let n = self
            .conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='cancelled' WHERE id=?1 AND state='queued'",
                rusqlite::params![id],
            )
            .map_err(map_err)?;
        Ok(n > 0)
    }

    pub fn cancel_conversation(&self, conversation: &str) -> Result<bool, String> {
        let n = self
            .conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET cancel=1 WHERE conversation=?1 AND state='running'",
                rusqlite::params![conversation],
            )
            .map_err(map_err)?;
        Ok(n > 0)
    }

    pub fn cancel_requested(&self, id: &str) -> Result<bool, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .query_row("SELECT cancel FROM inbox WHERE id=?1", [id], |r| {
                r.get::<_, i64>(0)
            })
            .map(|c| c != 0)
            .map_err(map_err)
    }

    pub fn mark_cancelled(&self, id: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state='cancelled', cancel=0 WHERE id=?1",
                rusqlite::params![id],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// Restart recovery: `running` rows were interrupted — re-queue them.
    /// Returns how many turns were cut short.
    pub fn recover(&self) -> Result<usize, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "UPDATE inbox SET state=CASE WHEN cancel=1 THEN 'cancelled' ELSE 'queued' END \
                 WHERE state='running'",
                [],
            )
            .map_err(map_err)
    }

    // ----- pairing codes -----

    /// The latest unused code minted for this user.
    pub fn pairing_code_for(&self, mxid: &str) -> Result<Option<String>, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .query_row(
                "SELECT code FROM pairing WHERE mxid=?1 ORDER BY created DESC LIMIT 1",
                [mxid],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(map_err(other)),
            })
    }

    pub fn pairing_insert(&self, code: &str, mxid: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "INSERT OR REPLACE INTO pairing (code,mxid,created) VALUES (?1,?2,?3)",
                rusqlite::params![code, mxid, now_secs()],
            )
            .map_err(map_err)?;
        Ok(())
    }

    /// Consume a code; `None` when unknown or already used.
    pub fn pairing_take(&self, code: &str) -> Result<Option<String>, String> {
        let conn = self.conn.lock().map_err(|_| "store locked".to_string())?;
        let mxid: Option<String> = conn
            .query_row("SELECT mxid FROM pairing WHERE code=?1", [code], |r| {
                r.get(0)
            })
            .ok();
        if mxid.is_some() {
            conn.execute("DELETE FROM pairing WHERE code=?1", [code])
                .map_err(map_err)?;
        }
        Ok(mxid)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>, String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(map_err(other)),
            })
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<(), String> {
        self.conn
            .lock()
            .map_err(|_| "store locked".to_string())?
            .execute(
                "INSERT OR REPLACE INTO meta (key,value) VALUES (?1,?2)",
                rusqlite::params![key, value],
            )
            .map_err(map_err)?;
        Ok(())
    }
}
