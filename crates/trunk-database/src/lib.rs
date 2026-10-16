use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use trunk_oxide_proto::types::{CallRowId, Freq, SignalDb, Talkgroup};

const SCHEMA_SQL: &str = include_str!("../sql/schema.sql");

fn schema_hash() -> String {
    let hash = Sha256::digest(SCHEMA_SQL.as_bytes());
    format!("{hash:x}")[..8].to_string()
}

/// Returns the database file path for a given hour.
fn db_path(base_path: &Path, hour: &str) -> PathBuf {
    let hash = schema_hash();
    base_path.join(format!("trunk-discord-{hour}-{hash}.db"))
}

pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open or create the DB file for the given hour. Creates tables if new.
    ///
    /// `hour` should be formatted as `"YYYY-MM-DDTHH"` (e.g. `"2026-02-26T14"`).
    pub fn open_hourly(base_path: &Path, hour: &str) -> Result<Self> {
        std::fs::create_dir_all(base_path)
            .with_context(|| format!("creating db directory: {}", base_path.display()))?;

        let path = db_path(base_path, hour);
        let conn = Connection::open(&path)
            .with_context(|| format!("opening database: {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .context("setting pragmas")?;

        conn.execute_batch(SCHEMA_SQL).context("creating schema")?;

        tracing::debug!(path = %path.display(), "opened hourly database");

        Ok(Self { conn })
    }

    // -- Calls --

    pub fn insert_call(&self, call: &CallRecord) -> Result<CallRowId> {
        self.conn.execute(
            "INSERT INTO calls (talkgroup, freq, encrypted, emergency, start_time, tag, short_name, thread_id, embed_message_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                call.talkgroup.0,
                call.freq.0,
                call.encrypted,
                call.emergency,
                call.start_time,
                call.tag,
                call.short_name,
                call.thread_id,
                call.embed_message_id,
            ],
        ).context("inserting call")?;

        Ok(CallRowId(self.conn.last_insert_rowid()))
    }

    pub fn update_call(&self, id: CallRowId, update: &CallUpdate) -> Result<()> {
        let transcription = update.transcription.as_deref();
        let tones = update
            .tones
            .as_ref()
            .map(|t| serde_json::to_string(t).unwrap_or_default());
        let events = update
            .events
            .as_ref()
            .map(|e| serde_json::to_string(e).unwrap_or_default());
        let duration = update.duration.map(|d| d.as_secs_f64());
        let signal = update.signal.map(|s| s.0);

        self.conn.execute(
            "UPDATE calls SET duration = ?1, signal = ?2, transcription = ?3, tones = ?4, events = ?5
             WHERE id = ?6",
            rusqlite::params![duration, signal, transcription, tones, events, id.0],
        ).context("updating call")?;

        Ok(())
    }

    // -- Thread cache --

    pub fn get_thread(
        &self,
        short_name: &str,
        talkgroup: Talkgroup,
        date: &str,
    ) -> Result<Option<u64>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT channel_id FROM thread_cache WHERE short_name = ?1 AND talkgroup = ?2 AND date = ?3",
        )?;

        let result = stmt
            .query_row(rusqlite::params![short_name, talkgroup.0, date], |row| {
                row.get(0)
            })
            .optional()
            .context("querying thread cache")?;

        Ok(result)
    }

    pub fn set_thread(
        &self,
        short_name: &str,
        talkgroup: Talkgroup,
        date: &str,
        channel_id: u64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO thread_cache (short_name, talkgroup, date, channel_id)
             VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![short_name, talkgroup.0, date, channel_id],
            )
            .context("setting thread cache")?;

        Ok(())
    }

    pub fn copy_threads_from(&self, other: &Self, date: &str) -> Result<usize> {
        let mut read_stmt = other.conn.prepare(
            "SELECT short_name, talkgroup, date, channel_id FROM thread_cache WHERE date = ?1",
        )?;

        let rows: Vec<(String, i64, String, u64)> = read_stmt
            .query_map([date], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let count = rows.len();
        for (short_name, talkgroup, d, channel_id) in rows {
            self.conn.execute(
                "INSERT OR IGNORE INTO thread_cache (short_name, talkgroup, date, channel_id)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![short_name, talkgroup, d, channel_id],
            )?;
        }

        Ok(count)
    }

    // -- Activity --

    pub fn insert_activity(&self, snapshot: &ActivitySnapshot) -> Result<()> {
        let signal_counts = serde_json::to_string(&snapshot.signal_counts)?;
        let trunk_msg_counts = serde_json::to_string(&snapshot.trunk_msg_counts)?;
        let unit_counts = serde_json::to_string(&snapshot.unit_counts)?;

        self.conn.execute(
            "INSERT INTO activity (timestamp, signal_counts, trunk_msg_counts, unit_counts, active_calls)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                snapshot.timestamp,
                signal_counts,
                trunk_msg_counts,
                unit_counts,
                snapshot.active_calls,
            ],
        ).context("inserting activity")?;

        Ok(())
    }
}

pub struct CallRecord {
    pub talkgroup: Talkgroup,
    pub freq: Freq,
    pub encrypted: bool,
    pub emergency: bool,
    pub start_time: i64,
    pub tag: String,
    pub short_name: String,
    pub thread_id: Option<u64>,
    pub embed_message_id: Option<u64>,
}

pub struct CallUpdate {
    pub duration: Option<Duration>,
    pub signal: Option<SignalDb>,
    pub transcription: Option<String>,
    pub tones: Option<Vec<String>>,
    pub events: Option<Vec<String>>,
}

pub struct ActivitySnapshot {
    pub timestamp: i64,
    pub signal_counts: HashMap<String, u64>,
    pub trunk_msg_counts: HashMap<String, (u64, u64)>,
    pub unit_counts: HashMap<String, u64>,
    pub active_calls: Option<usize>,
}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn test_db() -> Database {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        Database { conn }
    }

    #[test]
    fn schema_hash_is_stable() {
        let h1 = schema_hash();
        let h2 = schema_hash();
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 8);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn insert_and_update_call() {
        let db = test_db();

        let record = CallRecord {
            talkgroup: Talkgroup(1001),
            freq: Freq(772_406_250.0),
            encrypted: false,
            emergency: false,
            start_time: 1_700_000_000,
            tag: "FD Dispatch".to_string(),
            short_name: "smco".to_string(),
            thread_id: Some(123_456),
            embed_message_id: None,
        };

        let row_id = db.insert_call(&record).unwrap();
        assert_eq!(row_id.0, 1);

        let update = CallUpdate {
            duration: Some(Duration::from_secs_f64(12.5)),
            signal: Some(SignalDb(-3.2)),
            transcription: Some("Engine 5 responding".to_string()),
            tones: Some(vec!["1050 Hz".to_string(), "1100 Hz".to_string()]),
            events: Some(vec!["Signal: unit 123".to_string()]),
        };

        db.update_call(row_id, &update).unwrap();

        // Verify
        let (dur, sig, text): (f64, f64, String) = db
            .conn
            .query_row(
                "SELECT duration, signal, transcription FROM calls WHERE id = ?1",
                [row_id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert!((dur - 12.5).abs() < 0.01);
        assert!((sig - (-3.2)).abs() < 0.01);
        assert_eq!(text, "Engine 5 responding");
    }

    #[test]
    fn thread_cache_round_trip() {
        let db = test_db();

        let sys = "smco";
        let tg = Talkgroup(1001);

        assert!(db.get_thread(sys, tg, "2026-02-26").unwrap().is_none());

        db.set_thread(sys, tg, "2026-02-26", 999_888).unwrap();
        assert_eq!(db.get_thread(sys, tg, "2026-02-26").unwrap(), Some(999_888));

        // Different date returns None
        assert!(db.get_thread(sys, tg, "2026-02-27").unwrap().is_none());

        // The same talkgroup on another system is a separate thread
        assert!(db.get_thread("svrcs", tg, "2026-02-26").unwrap().is_none());
    }

    #[test]
    fn copy_threads() {
        let old = test_db();
        let new = test_db();

        old.set_thread("smco", Talkgroup(100), "2026-02-26", 111)
            .unwrap();
        old.set_thread("smco", Talkgroup(200), "2026-02-26", 222)
            .unwrap();
        old.set_thread("smco", Talkgroup(300), "2026-02-25", 333)
            .unwrap();

        let copied = new.copy_threads_from(&old, "2026-02-26").unwrap();
        assert_eq!(copied, 2);

        assert_eq!(
            new.get_thread("smco", Talkgroup(100), "2026-02-26")
                .unwrap(),
            Some(111)
        );
        assert_eq!(
            new.get_thread("smco", Talkgroup(200), "2026-02-26")
                .unwrap(),
            Some(222)
        );
        // Old date not copied
        assert!(
            new.get_thread("smco", Talkgroup(300), "2026-02-25")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn insert_activity_snapshot() {
        let db = test_db();

        let snapshot = ActivitySnapshot {
            timestamp: 1_700_000_060,
            signal_counts: [("NORMAL".to_string(), 12)].into_iter().collect(),
            trunk_msg_counts: [("smco".to_string(), (1364, 1280))].into_iter().collect(),
            unit_counts: HashMap::new(),
            active_calls: Some(3),
        };

        db.insert_activity(&snapshot).unwrap();

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM activity", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
