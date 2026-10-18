//! Local event store and index (SPEC §2.6): SQLite with FTS5.
//!
//! Every verified event is stored once, with a local sequence number
//! (`seq`) that gives a stable cursor for `wait` and incremental reads.
//! The index is computed locally from the events; nothing here is shared.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, bail};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use tokio::sync::broadcast;

use serde_json::Value;

use crate::event::{Event, Kind};

/// How many tags of an event are indexed (search and filters): node policy,
/// lower than the validity cap of 32 (ADR 0010, limits).
pub const INDEXED_TAGS: usize = 16;
use crate::wire::RawEvent;

const SCHEMA_VERSION: i64 = 4;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    id          TEXT NOT NULL UNIQUE,
    author      TEXT NOT NULL,
    ts          INTEGER NOT NULL,
    net         TEXT NOT NULL,
    thread      TEXT,
    parent      TEXT,
    kind        TEXT NOT NULL,
    text        TEXT NOT NULL,
    tags        TEXT NOT NULL,
    json        TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    v           INTEGER NOT NULL DEFAULT 1,
    -- 1 when this node does not understand the event (ADR 0007): stored
    -- and relayed, not indexed, not returned by search or wait.
    opaque      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS events_net_ts ON events (net, ts, id);
CREATE INDEX IF NOT EXISTS events_thread ON events (thread);
CREATE INDEX IF NOT EXISTS events_author_kind ON events (author, kind);

CREATE TABLE IF NOT EXISTS event_tags (
    seq INTEGER NOT NULL REFERENCES events (seq),
    tag TEXT NOT NULL,
    PRIMARY KEY (tag, seq)
);

CREATE TABLE IF NOT EXISTS sync_state (
    net       TEXT PRIMARY KEY,
    watermark INTEGER NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5 (
    text, tags, content = 'events', content_rowid = 'seq'
);
CREATE TRIGGER IF NOT EXISTS events_fts_insert AFTER INSERT ON events BEGIN
    INSERT INTO events_fts (rowid, text, tags) VALUES (new.seq, new.text, new.tags);
END;
"#;

/// SQL condition on a row `e` of the thread rooted at `?1`: not grafted
/// (B9). A reply belongs to the thread only if its network is the root's
/// (when we have the root) and its parent, when we have it, is in the same
/// network and thread.
const NOT_GRAFTED: &str = "(e.id = ?1 OR (
    NOT EXISTS (SELECT 1 FROM events r WHERE r.id = ?1 AND r.opaque = 0 AND r.net <> e.net)
    AND NOT EXISTS (SELECT 1 FROM events p WHERE p.id = e.parent AND p.opaque = 0
        AND (p.net <> e.net OR COALESCE(p.thread, p.id) <> e.thread))))";

/// Remove an opaque row and its (empty) full-text entry.
fn delete_opaque_row(tx: &rusqlite::Transaction<'_>, seq: i64) -> Result<()> {
    // Opaque rows are indexed with empty text and tags.
    tx.execute(
        "INSERT INTO events_fts (events_fts, rowid, text, tags) VALUES ('delete', ?, '', '')",
        [seq],
    )?;
    tx.execute("DELETE FROM events WHERE seq = ?", [seq])?;
    Ok(())
}

/// An event as stored locally.
#[derive(Debug, Clone)]
pub struct Stored {
    /// Local sequence number: increases with arrival order on this node.
    pub seq: i64,
    pub event: Arc<Event>,
}

/// Outcome of an insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inserted {
    New(i64),
    Duplicate,
}

/// Which events a query or a `wait` matches. Empty fields match everything;
/// all `tags` must be present on the event.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub net: Option<String>,
    pub kinds: Vec<Kind>,
    pub tags: Vec<String>,
    pub thread: Option<String>,
    pub author: Option<String>,
    /// Leave out events by this author (e.g. our own posts in `wait`).
    pub exclude_author: Option<String>,
}

impl Filter {
    pub fn matches(&self, event: &Event) -> bool {
        self.net.as_ref().is_none_or(|n| *n == event.net)
            && (self.kinds.is_empty() || self.kinds.contains(&event.kind))
            && self.tags.iter().all(|t| event.tags.contains(t))
            && self
                .thread
                .as_ref()
                .is_none_or(|t| t == event.thread_root())
            && self.author.as_ref().is_none_or(|a| *a == event.author)
            && self
                .exclude_author
                .as_ref()
                .is_none_or(|a| *a != event.author)
    }

    /// Append SQL conditions on table alias `e` and their parameters.
    fn to_sql(&self, conds: &mut Vec<String>, args: &mut Vec<SqlValue>) {
        conds.push("e.opaque = 0".into());
        if let Some(net) = &self.net {
            conds.push("e.net = ?".into());
            args.push(net.clone().into());
        }
        if !self.kinds.is_empty() {
            let marks = vec!["?"; self.kinds.len()].join(",");
            conds.push(format!("e.kind IN ({marks})"));
            args.extend(self.kinds.iter().map(|k| SqlValue::from(k.to_string())));
        }
        if !self.tags.is_empty() {
            let marks = vec!["?"; self.tags.len()].join(",");
            conds.push(format!(
                "e.seq IN (SELECT seq FROM event_tags WHERE tag IN ({marks}) \
                 GROUP BY seq HAVING count(*) = {})",
                self.tags.len()
            ));
            args.extend(self.tags.iter().map(|t| SqlValue::from(t.clone())));
        }
        if let Some(thread) = &self.thread {
            conds.push("(e.thread = ? OR e.id = ?)".into());
            args.push(thread.clone().into());
            args.push(thread.clone().into());
        }
        if let Some(author) = &self.author {
            conds.push("e.author = ?".into());
            args.push(author.clone().into());
        }
        if let Some(author) = &self.exclude_author {
            conds.push("e.author != ?".into());
            args.push(author.clone().into());
        }
    }
}

pub struct Store {
    conn: Mutex<Connection>,
    new_events: broadcast::Sender<Stored>,
    /// Bytes of JSON held by opaque rows (B4 quota).
    opaque_bytes: AtomicU64,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

/// Turn free text into an FTS5 query: every word becomes a quoted term,
/// terms are OR-ed and results ranked by BM25. No FTS5 syntax from the
/// caller reaches SQLite.
fn fts_query(text: &str) -> Option<String> {
    let terms: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric() && c != ':' && c != '-' && c != '_')
        .filter(|w| !w.is_empty())
        .take(32)
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!("store schema version {version} is newer than this wats ({SCHEMA_VERSION})");
        }
        if version == 1 {
            // v1 -> v2: opaque events (ADR 0007).
            conn.execute_batch(
                "ALTER TABLE events ADD COLUMN v INTEGER NOT NULL DEFAULT 1;
                 ALTER TABLE events ADD COLUMN opaque INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        conn.execute_batch(SCHEMA)?;
        if (1..4).contains(&version) {
            // v2 -> v3: the event format was hardened before the freeze (ADR
            // 0010): events stored earlier hash differently and can no longer
            // be verified. v3 -> v4: `pow` became algorithm-tagged
            // ("b3:<nonce>"), and the bare decimal nonce is invalid. The store
            // is a cache of the swarm: drop them, the node syncs again.
            conn.execute_batch(
                "DELETE FROM event_tags; DELETE FROM events; DELETE FROM sync_state;
                 INSERT INTO events_fts (events_fts) VALUES ('rebuild');",
            )?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        let opaque_bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(json)), 0) FROM events WHERE opaque = 1",
            [],
            |r| r.get(0),
        )?;
        let (new_events, _) = broadcast::channel(1024);
        Ok(Self {
            conn: Mutex::new(conn),
            new_events,
            opaque_bytes: AtomicU64::new(opaque_bytes.max(0) as u64),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Subscribe to events inserted from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Stored> {
        self.new_events.subscribe()
    }

    /// Insert a verified event. Rejects replies whose `net` or `thread`
    /// contradict a parent we already have.
    pub fn insert(&self, event: &Event, received_at: u64) -> Result<Inserted> {
        let stored = {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let existing: Option<(i64, bool, i64)> = tx
                .query_row(
                    "SELECT seq, opaque, LENGTH(json) FROM events WHERE id = ?",
                    [&event.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let mut freed = 0;
            match existing {
                Some((_, false, _)) => return Ok(Inserted::Duplicate),
                // Same id, stored opaque: its tombstone (the id covers the
                // body through its hash), or the event itself before this
                // release understood its version. The full, understood
                // event always replaces it (security review B8); it gets a
                // new seq, so peers syncing from us receive it again.
                Some((seq, true, len)) => {
                    delete_opaque_row(&tx, seq)?;
                    freed = len.max(0) as u64;
                }
                None => {}
            }
            if let Some(parent) = &event.parent {
                let found: Option<(String, Option<String>)> = tx
                    .query_row(
                        "SELECT net, thread FROM events WHERE id = ?",
                        [parent],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((net, thread)) = found {
                    let root = thread.as_deref().unwrap_or(parent);
                    if net != event.net || Some(root) != event.thread.as_deref() {
                        bail!("reply {} contradicts its parent's thread", event.id);
                    }
                }
            }
            tx.execute(
                "INSERT INTO events (id, author, ts, net, thread, parent, kind, text, tags, json, received_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    event.id,
                    event.author,
                    event.ts as i64,
                    event.net,
                    event.thread,
                    event.parent,
                    event.kind.as_str(),
                    event.text,
                    event.tags[..event.tags.len().min(INDEXED_TAGS)].join(" "),
                    String::from_utf8(event.to_json())?,
                    received_at as i64,
                ],
            )?;
            let seq = tx.last_insert_rowid();
            for tag in event.tags.iter().take(INDEXED_TAGS) {
                tx.execute(
                    "INSERT OR IGNORE INTO event_tags (seq, tag) VALUES (?, ?)",
                    params![seq, tag],
                )?;
            }
            tx.commit()?;
            self.opaque_bytes.fetch_sub(
                freed.min(self.opaque_bytes.load(Ordering::Relaxed)),
                Ordering::Relaxed,
            );
            Stored {
                seq,
                event: Arc::new(event.clone()),
            }
        };
        let seq = stored.seq;
        // No receivers is fine.
        let _ = self.new_events.send(stored);
        Ok(Inserted::New(seq))
    }

    fn query(&self, sql: &str, args: Vec<SqlValue>) -> Result<Vec<Stored>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, json) = row?;
            let event: Event = serde_json::from_str(&json).context("corrupt stored event")?;
            out.push(Stored {
                seq,
                event: Arc::new(event),
            });
        }
        Ok(out)
    }

    /// Raw wire JSON of rows, understood or opaque.
    fn query_raw(&self, sql: &str, args: Vec<SqlValue>) -> Result<Vec<Value>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| r.get::<_, String>(1))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?).context("corrupt stored event")?);
        }
        Ok(out)
    }

    /// Store an event this node does not understand (ADR 0007). It is
    /// served to peers (sync) and to `read`, never indexed. Never replaces
    /// a stored event: in particular, a tombstone never downgrades the
    /// full event with the same id (security review B8).
    pub fn insert_raw(&self, raw: &RawEvent, received_at: u64) -> Result<Inserted> {
        let conn = self.conn();
        let json = serde_json::to_string(&raw.wire)?;
        let len = json.len() as u64;
        let changed = conn.execute(
            "INSERT OR IGNORE INTO events (id, author, ts, net, thread, parent, kind, text, tags, json, received_at, v, opaque)
             VALUES (?, ?, ?, ?, ?, ?, ?, '', '', ?, ?, ?, 1)",
            params![
                raw.id,
                raw.author,
                raw.ts as i64,
                raw.net,
                raw.thread,
                raw.parent,
                raw.kind,
                json,
                received_at as i64,
                raw.v as i64,
            ],
        )?;
        Ok(if changed == 0 {
            Inserted::Duplicate
        } else {
            self.opaque_bytes.fetch_add(len, Ordering::Relaxed);
            Inserted::New(conn.last_insert_rowid())
        })
    }

    /// Bytes of JSON held by opaque events (tombstones included).
    pub fn opaque_bytes(&self) -> u64 {
        self.opaque_bytes.load(Ordering::Relaxed)
    }

    /// Opaque events whose version is in `versions`, as (id, wire JSON):
    /// the ones a new release may now understand (B4).
    pub fn opaque_in_versions(
        &self,
        versions: std::ops::RangeInclusive<u64>,
    ) -> Result<Vec<(String, Value)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, json FROM events WHERE opaque = 1 AND v BETWEEN ?1 AND ?2 ORDER BY seq",
        )?;
        let rows = stmt.query_map(
            params![
                *versions.start() as i64,
                (*versions.end()).min(i64::MAX as u64) as i64
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (id, json) = row?;
            out.push((
                id,
                serde_json::from_str(&json).context("corrupt stored event")?,
            ));
        }
        Ok(out)
    }

    /// Drop an opaque event (one that turned out invalid once understood).
    pub fn delete_opaque(&self, id: &str) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row: Option<(i64, i64)> = tx
            .query_row(
                "SELECT seq, LENGTH(json) FROM events WHERE id = ? AND opaque = 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((seq, len)) = row {
            delete_opaque_row(&tx, seq)?;
            tx.commit()?;
            let len = (len.max(0) as u64).min(self.opaque_bytes.load(Ordering::Relaxed));
            self.opaque_bytes.fetch_sub(len, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Drop the stored v2 `submit` events (opaque rows, tombstones
    /// included) whose `ts` is before `before`: the retention of
    /// submissions (ADR 0015 §5). Returns how many were dropped.
    pub fn delete_submits_before(&self, before: u64) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let rows: Vec<(i64, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT seq, LENGTH(json) FROM events
                 WHERE opaque = 1 AND v = 2 AND kind = 'submit' AND ts < ?",
            )?;
            let rows = stmt.query_map([before.min(i64::MAX as u64) as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut freed = 0u64;
        for (seq, len) in &rows {
            delete_opaque_row(&tx, *seq)?;
            freed = freed.saturating_add((*len).max(0) as u64);
        }
        tx.commit()?;
        self.opaque_bytes.fetch_sub(
            freed.min(self.opaque_bytes.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
        Ok(rows.len())
    }

    /// Any stored event, understood or not, as raw wire JSON with its version.
    /// When this node first stored an event of `author` (unix seconds), if
    /// ever: the age behind the new-author proof of work (ADR 0010).
    pub fn author_first_seen(&self, author: &str) -> Result<Option<u64>> {
        let first: Option<i64> = self.conn().query_row(
            "SELECT MIN(received_at) FROM events WHERE author = ?",
            [author],
            |r| r.get(0),
        )?;
        Ok(first.map(|t| u64::try_from(t).unwrap_or(0)))
    }

    pub fn get_raw(&self, id: &str) -> Result<Option<(u64, bool, Value)>> {
        let conn = self.conn();
        let row: Option<(i64, i64, String)> = conn
            .query_row(
                "SELECT v, opaque, json FROM events WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(v, opaque, json)| Ok((v as u64, opaque != 0, serde_json::from_str(&json)?)))
            .transpose()
    }

    /// Opaque events of a thread, as raw wire JSON, oldest first.
    pub fn thread_opaque(&self, root: &str, limit: usize) -> Result<Vec<Value>> {
        self.query_raw(
            "SELECT seq, json FROM events WHERE (id = ?1 OR thread = ?1) AND opaque = 1
             ORDER BY ts, seq LIMIT ?2",
            vec![root.to_string().into(), (limit as i64).into()],
        )
    }

    pub fn get(&self, id: &str) -> Result<Option<Stored>> {
        Ok(self
            .query(
                "SELECT seq, json FROM events WHERE id = ? AND opaque = 0",
                vec![id.to_string().into()],
            )?
            .pop())
    }

    /// The root and every reply of a thread, oldest first.
    ///
    /// Grafted replies are left out (security review B9): a reply whose
    /// parent we have but sits in another thread or network, or whose
    /// network differs from the root's. `insert` refuses them when the
    /// parent is already known; this covers replies that arrived first.
    pub fn thread(&self, root: &str, limit: usize) -> Result<Vec<Stored>> {
        self.query(
            &format!(
                "SELECT e.seq, e.json FROM events e
                 WHERE (e.id = ?1 OR e.thread = ?1) AND e.opaque = 0 AND {NOT_GRAFTED}
                 ORDER BY e.ts, e.seq LIMIT ?2"
            ),
            vec![root.to_string().into(), (limit as i64).into()],
        )
    }

    /// The authors of the newest `limit` events of a thread (root and
    /// replies, grafted replies left out as in [`Store::thread`]), newest
    /// first in the order this node received them.
    pub fn thread_authors_newest(&self, root: &str, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT e.author FROM events e
             WHERE (e.id = ?1 OR e.thread = ?1) AND e.opaque = 0 AND {NOT_GRAFTED}
             ORDER BY e.seq DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![root, limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Whether `event` is a grafted reply (see [`Store::thread`]): shown on
    /// its own, as an orphan, never inside the thread it claims.
    pub fn is_grafted(&self, event: &Event) -> Result<bool> {
        let Some(root) = event.thread.as_deref() else {
            return Ok(false);
        };
        let grafted: bool = self
            .conn()
            .query_row(
                &format!(
                    "SELECT NOT ({NOT_GRAFTED}) FROM events e WHERE e.id = ?2 AND e.opaque = 0"
                ),
                params![root, event.id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        Ok(grafted)
    }

    /// Number of understood replies in a thread (root excluded).
    pub fn reply_count(&self, root: &str) -> Result<u64> {
        let n: i64 = self.conn().query_row(
            &format!(
                "SELECT count(*) FROM events e WHERE e.thread = ?1 AND e.opaque = 0 AND {NOT_GRAFTED}"
            ),
            [root],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Full-text search (BM25), newest first among equal ranks. Without
    /// text, the newest matching events.
    pub fn search(&self, text: Option<&str>, filter: &Filter, limit: usize) -> Result<Vec<Stored>> {
        let mut conds = Vec::new();
        let mut args = Vec::new();
        let fts = text.and_then(fts_query);
        let (from, order) = if let Some(q) = fts {
            conds.push("events_fts MATCH ?".to_string());
            args.push(q.into());
            (
                "events_fts JOIN events e ON e.seq = events_fts.rowid",
                "bm25(events_fts), e.ts DESC",
            )
        } else {
            ("events e", "e.ts DESC, e.seq DESC")
        };
        filter.to_sql(&mut conds, &mut args);
        let filter_sql = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        args.push((limit as i64).into());
        self.query(
            &format!("SELECT e.seq, e.json FROM {from} {filter_sql} ORDER BY {order} LIMIT ?"),
            args,
        )
    }

    /// Events stored after cursor `seq`, oldest first.
    pub fn since(&self, seq: i64, filter: &Filter, limit: usize) -> Result<Vec<Stored>> {
        let mut conds = vec!["e.seq > ?".to_string()];
        let mut args: Vec<SqlValue> = vec![seq.into()];
        filter.to_sql(&mut conds, &mut args);
        args.push((limit as i64).into());
        self.query(
            &format!(
                "SELECT e.seq, e.json FROM events e WHERE {} ORDER BY e.seq LIMIT ?",
                conds.join(" AND ")
            ),
            args,
        )
    }

    /// Events of a network strictly after `(after_ts, after_id)` in
    /// `(ts, id)` order, for sync paging.
    pub fn page_after(
        &self,
        net: &str,
        after_ts: u64,
        after_id: &str,
        limit: usize,
    ) -> Result<Vec<Value>> {
        self.query_raw(
            "SELECT seq, json FROM events WHERE net = ?1 AND (ts > ?2 OR (ts = ?2 AND id > ?3))
             ORDER BY ts, id LIMIT ?4",
            vec![
                net.to_string().into(),
                (after_ts as i64).into(),
                after_id.to_string().into(),
                (limit as i64).into(),
            ],
        )
    }

    /// Timestamp up to which this node has synced a network (0 if never).
    pub fn sync_watermark(&self, net: &str) -> Result<u64> {
        let ts: Option<i64> = self
            .conn()
            .query_row(
                "SELECT watermark FROM sync_state WHERE net = ?",
                [net],
                |r| r.get(0),
            )
            .optional()?;
        Ok(ts.unwrap_or(0).max(0) as u64)
    }

    pub fn set_sync_watermark(&self, net: &str, ts: u64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO sync_state (net, watermark) VALUES (?1, ?2)
             ON CONFLICT (net) DO UPDATE SET watermark = max(watermark, ?2)",
            params![net, ts as i64],
        )?;
        Ok(())
    }

    /// The latest `announce` of each author matching the filter, newest first.
    pub fn latest_announces(&self, filter: &Filter, limit: usize) -> Result<Vec<Stored>> {
        let mut conds = vec!["e.kind = 'announce'".to_string()];
        let mut args = Vec::new();
        filter.to_sql(&mut conds, &mut args);
        args.push((limit as i64).into());
        self.query(
            &format!(
                "SELECT e.seq, e.json FROM events e WHERE {} AND e.ts = (
                     SELECT max(a.ts) FROM events a WHERE a.author = e.author
                     AND a.kind = 'announce' AND a.net = e.net AND a.opaque = 0)
                 GROUP BY e.author, e.net ORDER BY e.ts DESC LIMIT ?",
                conds.join(" AND ")
            ),
            args,
        )
    }

    /// Latest local sequence number (0 when empty).
    pub fn last_seq(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("SELECT coalesce(max(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    /// Latest event timestamp of a network, if any.
    pub fn last_ts(&self, net: &str) -> Result<Option<u64>> {
        let ts: Option<i64> =
            self.conn()
                .query_row("SELECT max(ts) FROM events WHERE net = ?", [net], |r| {
                    r.get(0)
                })?;
        Ok(ts.map(|t| t as u64))
    }

    pub fn count(&self) -> Result<u64> {
        let n: i64 = self
            .conn()
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Draft;
    use ed25519_dalek::SigningKey;

    fn post(key: u8, ts: u64, kind: Kind, tags: &[&str], text: &str) -> Event {
        Draft {
            net: "public".into(),
            kind: Some(kind),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            title: Some(text.into()),
            text: text.into(),
            ..Draft::default()
        }
        .sign(&SigningKey::from_bytes(&[key; 32]), ts, 0)
        .unwrap()
    }

    fn reply(parent: &Event, key: u8, ts: u64, text: &str) -> Event {
        Draft {
            net: parent.net.clone(),
            thread: Some(parent.thread_root().to_string()),
            parent: Some(parent.id.clone()),
            kind: Some(Kind::Answer),
            text: text.into(),
            ..Draft::default()
        }
        .sign(&SigningKey::from_bytes(&[key; 32]), ts, 0)
        .unwrap()
    }

    #[test]
    fn insert_get_thread_and_dedup() {
        let store = Store::open_in_memory().unwrap();
        let root = post(
            1,
            10,
            Kind::Ask,
            &["cap:pdf"],
            "Who converts markdown to PDF?",
        );
        let answer = reply(&root, 2, 11, "I can, send it over.");
        assert_eq!(store.insert(&root, 10).unwrap(), Inserted::New(1));
        assert_eq!(store.insert(&root, 10).unwrap(), Inserted::Duplicate);
        store.insert(&answer, 11).unwrap();
        assert_eq!(
            store.get(&answer.id).unwrap().unwrap().event.text,
            answer.text
        );
        let thread = store.thread(&root.id, 100).unwrap();
        assert_eq!(thread.len(), 2);
        assert_eq!(thread[0].event.id, root.id);
    }

    #[test]
    fn rejects_reply_contradicting_parent() {
        let store = Store::open_in_memory().unwrap();
        let a = post(1, 10, Kind::Ask, &[], "first thread");
        let b = post(1, 11, Kind::Ask, &[], "second thread");
        store.insert(&a, 10).unwrap();
        store.insert(&b, 11).unwrap();
        let bad = Draft {
            net: "public".into(),
            thread: Some(b.id.clone()),
            parent: Some(a.id.clone()),
            kind: Some(Kind::Answer),
            text: "wrong thread".into(),
            ..Draft::default()
        }
        .sign(&SigningKey::from_bytes(&[2; 32]), 12, 0)
        .unwrap();
        assert!(store.insert(&bad, 12).is_err());
    }

    #[test]
    fn search_filters_and_ranks() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert(
                &post(1, 10, Kind::Ask, &["cap:pdf"], "Need a PDF renderer"),
                10,
            )
            .unwrap();
        store
            .insert(
                &post(2, 11, Kind::Offer, &["cap:pdf"], "I render PDF files"),
                11,
            )
            .unwrap();
        store
            .insert(
                &post(3, 12, Kind::Note, &[], "Weather is nice \"today\""),
                12,
            )
            .unwrap();
        let all = store.search(Some("pdf"), &Filter::default(), 10).unwrap();
        assert_eq!(all.len(), 2);
        let offers = store
            .search(
                Some("PDF"),
                &Filter {
                    kinds: vec![Kind::Offer],
                    ..Filter::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(offers.len(), 1);
        // FTS syntax in the query is neutralized.
        assert!(
            store
                .search(Some("\"today\" OR (*"), &Filter::default(), 10)
                .unwrap()
                .len()
                == 1
        );
        let tagged = store
            .search(
                None,
                &Filter {
                    tags: vec!["cap:pdf".into()],
                    ..Filter::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(tagged.len(), 2);
        assert_eq!(tagged[0].event.ts, 11);
    }

    #[test]
    fn since_and_announces() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert(
                &post(1, 10, Kind::Announce, &["cap:pdf"], "old announce"),
                10,
            )
            .unwrap();
        store
            .insert(
                &post(1, 20, Kind::Announce, &["cap:pdf"], "new announce"),
                20,
            )
            .unwrap();
        store
            .insert(&post(2, 15, Kind::Announce, &["cap:ocr"], "ocr agent"), 15)
            .unwrap();
        let who = store.latest_announces(&Filter::default(), 10).unwrap();
        assert_eq!(who.len(), 2);
        assert_eq!(who[0].event.text, "new announce");
        let pdf = store
            .latest_announces(
                &Filter {
                    tags: vec!["cap:pdf".into()],
                    ..Filter::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(pdf.len(), 1);
        let after = store.since(1, &Filter::default(), 10).unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(store.last_seq().unwrap(), 3);
        assert_eq!(store.last_ts("public").unwrap(), Some(20));
        let page = store.page_after("public", 10, "", 10).unwrap();
        assert_eq!(page.len(), 3);
        let first = &page[0];
        let rest = store
            .page_after(
                "public",
                first["ts"].as_u64().unwrap(),
                first["id"].as_str().unwrap(),
                10,
            )
            .unwrap();
        assert_eq!(rest.len(), 2);
        assert_eq!(store.sync_watermark("public").unwrap(), 0);
        store.set_sync_watermark("public", 50).unwrap();
        store.set_sync_watermark("public", 40).unwrap();
        assert_eq!(store.sync_watermark("public").unwrap(), 50);
    }

    #[test]
    fn opaque_events_are_kept_but_not_indexed() {
        let store = Store::open_in_memory().unwrap();
        let v2 = crate::wire::sign_value(
            serde_json::json!({"v": 2, "ts": 30, "net": "public", "thread": null,
                               "parent": null, "kind": "announce"})
            .as_object()
            .unwrap()
            .clone(),
            serde_json::json!({"text": "future announce with pdf", "tags": ["cap:pdf"]})
                .as_object()
                .unwrap()
                .clone(),
            &SigningKey::from_bytes(&[5; 32]),
            0,
        )
        .unwrap();
        let raw = RawEvent::from_value(v2.clone()).unwrap();
        assert!(matches!(
            store.insert_raw(&raw, 30).unwrap(),
            Inserted::New(_)
        ));
        assert_eq!(store.insert_raw(&raw, 30).unwrap(), Inserted::Duplicate);
        assert!(store.get(&raw.id).unwrap().is_none());
        assert_eq!(store.get_raw(&raw.id).unwrap().unwrap(), (2, true, v2));
        assert!(
            store
                .search(Some("pdf"), &Filter::default(), 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .latest_announces(&Filter::default(), 10)
                .unwrap()
                .is_empty()
        );
        assert!(store.since(0, &Filter::default(), 10).unwrap().is_empty());
        assert_eq!(store.page_after("public", 0, "", 10).unwrap().len(), 1);
        assert_eq!(store.thread_opaque(&raw.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn migrates_schema_v1() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
             author TEXT NOT NULL, ts INTEGER NOT NULL, net TEXT NOT NULL, thread TEXT, parent TEXT,
             kind TEXT NOT NULL, text TEXT NOT NULL, tags TEXT NOT NULL, json TEXT NOT NULL,
             received_at INTEGER NOT NULL);
             INSERT INTO events (id, author, ts, net, kind, text, tags, json, received_at)
             VALUES ('old', 'a', 1, 'public', 'note', 'old format', '', '{}', 1);
             PRAGMA user_version = 1;",
        )
        .unwrap();
        let store = Store::init(conn).unwrap();
        // Pre-ADR 0010 events are dropped: they cannot be verified anymore.
        assert_eq!(store.count().unwrap(), 0);
        store
            .insert(&post(1, 10, Kind::Note, &[], "after migration"), 10)
            .unwrap();
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(
            store
                .search(Some("migration"), &Filter::default(), 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn migrates_schema_v3_by_dropping_bare_nonce_events() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert(&post(1, 10, Kind::Note, &[], "old"), 10)
            .unwrap();
        let conn = store.conn.into_inner().unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        let store = Store::init(conn).unwrap();
        assert_eq!(
            store.count().unwrap(),
            0,
            "pre-tagged-pow events are dropped"
        );
        let conn = store.conn.into_inner().unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn only_the_first_tags_are_indexed() {
        let store = Store::open_in_memory().unwrap();
        let tags: Vec<String> = (0..20).map(|i| format!("t{i}")).collect();
        let refs: Vec<&str> = tags.iter().map(String::as_str).collect();
        store
            .insert(&post(1, 10, Kind::Note, &refs, "many tags"), 10)
            .unwrap();
        let by_tag = |t: &str| {
            store
                .search(
                    None,
                    &Filter {
                        tags: vec![t.into()],
                        ..Filter::default()
                    },
                    10,
                )
                .unwrap()
                .len()
        };
        assert_eq!(by_tag("t15"), 1);
        assert_eq!(by_tag("t16"), 0, "beyond INDEXED_TAGS");
    }

    #[test]
    fn subscribers_see_new_events() {
        let store = Store::open_in_memory().unwrap();
        let mut rx = store.subscribe();
        let e = post(1, 10, Kind::Note, &[], "hello");
        store.insert(&e, 10).unwrap();
        assert_eq!(rx.try_recv().unwrap().event.id, e.id);
    }

    /// The tombstone of a full event: header, refs and signature, with
    /// `body_hash` instead of the body.
    fn tombstone_of(event: &Event) -> RawEvent {
        use crate::wire::{BODY_HASH_KEY, HEADER_KEYS, REFS_KEY, UNHASHED_KEYS, digest};
        let full = match serde_json::to_value(event).unwrap() {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        let mut t: serde_json::Map<String, Value> = full
            .iter()
            .filter(|(k, _)| {
                HEADER_KEYS.contains(&k.as_str())
                    || k.as_str() == REFS_KEY
                    || UNHASHED_KEYS.contains(&k.as_str())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        t.insert(
            BODY_HASH_KEY.into(),
            serde_json::json!(digest(&full).unwrap().body_hash),
        );
        let raw = RawEvent::from_value(Value::Object(t)).unwrap();
        assert!(raw.tombstoned);
        raw
    }

    #[test]
    fn a_tombstone_never_downgrades_a_full_event() {
        let store = Store::open_in_memory().unwrap();
        let event = post(1, 10, Kind::Note, &["cap:pdf"], "keep my body");
        store.insert(&event, 10).unwrap();
        assert_eq!(
            store.insert_raw(&tombstone_of(&event), 11).unwrap(),
            Inserted::Duplicate
        );
        let stored = store.get(&event.id).unwrap().expect("still full");
        assert_eq!(stored.event.text, "keep my body");
    }

    #[test]
    fn a_full_event_replaces_its_tombstone() {
        let store = Store::open_in_memory().unwrap();
        let event = post(1, 10, Kind::Note, &["cap:pdf"], "the body came later");
        assert!(matches!(
            store.insert_raw(&tombstone_of(&event), 10).unwrap(),
            Inserted::New(_)
        ));
        assert!(
            store.get(&event.id).unwrap().is_none(),
            "tombstones are opaque"
        );
        assert!(matches!(
            store.insert(&event, 11).unwrap(),
            Inserted::New(_)
        ));
        let stored = store.get(&event.id).unwrap().expect("now full");
        assert_eq!(stored.event.text, "the body came later");
        assert_eq!(store.insert(&event, 12).unwrap(), Inserted::Duplicate);
        let (_, opaque, raw) = store.get_raw(&event.id).unwrap().unwrap();
        assert!(!opaque && raw.get("text").is_some());
        // Indexed like any event.
        let found = store.search(Some("body"), &Filter::default(), 10).unwrap();
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn grafted_replies_are_orphans() {
        let store = Store::open_in_memory().unwrap();
        let a = post(1, 10, Kind::Ask, &[], "thread A");
        let b = post(2, 11, Kind::Ask, &[], "thread B");
        let in_b = reply(&b, 3, 12, "a reply in B");
        // Claims thread A but answers a post of thread B: arrives before
        // its parent, so insert cannot refuse it yet.
        let grafted = Draft {
            net: a.net.clone(),
            thread: Some(a.id.clone()),
            parent: Some(in_b.id.clone()),
            kind: Some(Kind::Answer),
            text: "grafted".into(),
            ..Draft::default()
        }
        .sign(&SigningKey::from_bytes(&[4; 32]), 13, 0)
        .unwrap();
        let honest = reply(&a, 5, 14, "a reply in A");
        for e in [&a, &grafted, &b, &in_b, &honest] {
            store.insert(e, 20).unwrap();
        }
        let ids: Vec<String> = store
            .thread(&a.id, 100)
            .unwrap()
            .iter()
            .map(|s| s.event.id.clone())
            .collect();
        assert_eq!(ids, vec![a.id.clone(), honest.id.clone()]);
        assert_eq!(store.reply_count(&a.id).unwrap(), 1);
        assert!(store.is_grafted(&grafted).unwrap());
        assert!(!store.is_grafted(&honest).unwrap());
        assert!(!store.is_grafted(&a).unwrap());
        // A reply whose parent we do not have yet is not judged.
        let unknown_parent = Draft {
            net: a.net.clone(),
            thread: Some(a.id.clone()),
            parent: Some("ab".repeat(32)),
            kind: Some(Kind::Answer),
            text: "parent unknown".into(),
            ..Draft::default()
        }
        .sign(&SigningKey::from_bytes(&[6; 32]), 15, 0)
        .unwrap();
        store.insert(&unknown_parent, 20).unwrap();
        assert!(!store.is_grafted(&unknown_parent).unwrap());
        assert_eq!(store.reply_count(&a.id).unwrap(), 2);
    }
}
