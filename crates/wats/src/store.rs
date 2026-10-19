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

const SCHEMA_VERSION: i64 = 6;

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
-- Rotations and retractions are looked up by kind (ADR 0020 §5.1).
CREATE INDEX IF NOT EXISTS events_kind ON events (kind);
-- Replies of a post, for its signals (ADR 0019 §1).
CREATE INDEX IF NOT EXISTS events_parent ON events (parent);

-- Asks a human posted from an MCP host the node can notify (ADR 0018 §7).
CREATE TABLE IF NOT EXISTS ask_notify (
    ask    TEXT PRIMARY KEY,
    client TEXT NOT NULL,
    at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS event_tags (
    seq INTEGER NOT NULL REFERENCES events (seq),
    tag TEXT NOT NULL,
    PRIMARY KEY (tag, seq)
);

-- The posts each understood event cites in `refs` (ADR 0019 §1, schema
-- 6): its citations, found without reading every event.
CREATE TABLE IF NOT EXISTS event_refs (
    ref TEXT NOT NULL,
    seq INTEGER NOT NULL REFERENCES events (seq),
    PRIMARY KEY (ref, seq)
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

-- Local drop records (ADR 0020, schema 5): bodies this node dropped, or
-- will refuse when they arrive. Never shared.
CREATE TABLE IF NOT EXISTS dropped (
    id      TEXT PRIMARY KEY,
    reason  TEXT NOT NULL,
    at      INTEGER NOT NULL,
    -- The id of the retract event or label that caused the drop, if any.
    trigger TEXT,
    -- The human's own words (`wats drop --reason`), if any.
    detail    TEXT
);
"#;

/// SQL condition on a row `e` of the thread rooted at `?1`: not grafted
/// (B9). A reply belongs to the thread only if its network is the root's
/// (when we have the root) and its parent, when we have it, is in the same
/// network and thread.
const NOT_GRAFTED: &str = "(e.id = ?1 OR (
    NOT EXISTS (SELECT 1 FROM events r WHERE r.id = ?1 AND r.opaque = 0 AND r.net <> e.net)
    AND NOT EXISTS (SELECT 1 FROM events p WHERE p.id = e.parent AND p.opaque = 0
        AND (p.net <> e.net OR COALESCE(p.thread, p.id) <> e.thread))))";

/// Insert an opaque row (an event this node does not understand, or a
/// tombstone) unless one with its id exists. Returns the rows changed.
fn insert_opaque_row(
    conn: &Connection,
    raw: &RawEvent,
    json: &str,
    received_at: u64,
) -> Result<usize> {
    Ok(conn.execute(
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
    )?)
}

/// Whether a kept full event lists blob `hash`.
fn blob_referenced(conn: &Connection, hash: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM events e WHERE e.opaque = 0 AND instr(e.json, ?1) > 0
             AND EXISTS (SELECT 1 FROM json_each(e.json, '$.blobs') b
                         WHERE json_extract(b.value, '$.hash') = ?1))",
        [hash],
        |r| r.get(0),
    )?)
}

/// Most keys walked back along a rotation chain (ADR 0010 A5): enough
/// for any honest history, a bound for a forged web of rotations.
const MAX_ROTATION_KEYS: usize = 256;

/// Whether `author` is `successor`, or a key that rotated into it,
/// directly or through a chain of `rotate` events this node holds in full
/// (both keys signed each link).
fn rotated_into(conn: &Connection, successor: &str, author: &str) -> Result<bool> {
    if successor == author {
        return Ok(true);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT author FROM events WHERE kind = 'rotate' AND opaque = 0 AND v = 1
             AND json_extract(json, '$.data.new_key') = ?",
    )?;
    let mut seen = std::collections::HashSet::from([successor.to_string()]);
    let mut frontier = vec![successor.to_string()];
    while let Some(key) = frontier.pop() {
        let rows = stmt.query_map([&key], |r| r.get::<_, String>(0))?;
        for predecessor in rows {
            let predecessor = predecessor?;
            if predecessor == author {
                return Ok(true);
            }
            if seen.len() < MAX_ROTATION_KEYS && seen.insert(predecessor.clone()) {
                frontier.push(predecessor);
            }
        }
    }
    Ok(false)
}

/// The id of a retraction held here (full or tombstone) that cites post
/// `target` and whose author is `author` or a key `author` rotated into:
/// the retraction that applies to that post, if any.
fn retraction_citing(conn: &Connection, target: &str, author: &str) -> Result<Option<String>> {
    let citing: Vec<(String, String)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT e.id, e.author FROM events e WHERE e.kind = ?1 AND e.v = 1
                 AND e.thread IS NULL AND e.parent IS NULL
                 AND EXISTS (SELECT 1 FROM json_each(e.json, '$.refs') r WHERE r.value = ?2)
             ORDER BY e.seq",
        )?;
        let rows = stmt.query_map(params![crate::retract::KIND, target], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (id, retractor) in citing {
        if rotated_into(conn, &retractor, author)? {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Remove an opaque row and its (empty) full-text entry.
fn delete_opaque_row(tx: &rusqlite::Transaction<'_>, seq: i64) -> Result<()> {
    // Opaque rows are indexed with empty text and tags.
    tx.execute(
        "INSERT INTO events_fts (events_fts, rowid, text, tags) VALUES ('delete', ?, '', '')",
        [seq],
    )?;
    tx.execute("DELETE FROM event_refs WHERE seq = ?", [seq])?;
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

/// An event that replies to a post or cites it, as [`Store::signal_rows`]
/// returns it: what the post's signals are computed from.
#[derive(Debug, Clone)]
pub struct SignalRow {
    /// The post it points to.
    pub target: String,
    /// It replies to the target (`parent`), rather than citing it (`refs`).
    pub reply: bool,
    pub id: String,
    pub author: String,
    pub ts: u64,
    pub net: String,
    /// Its kind, as stored.
    pub kind: String,
    pub data: serde_json::Map<String, Value>,
    /// Its title, or the start of its text.
    pub title: String,
}

/// A `react` reply carrying `data.accept`, as [`Store::accept_reacts`]
/// returns it.
#[derive(Debug, Clone)]
pub struct AcceptRow {
    pub thread: String,
    pub id: String,
    pub author: String,
    pub ts: u64,
    /// The post accepted (or no longer).
    pub parent: String,
    pub accept: bool,
}

/// A tag and what carries it (`browse`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagCount {
    pub tag: String,
    /// Threads with at least one post carrying the tag.
    pub threads: u64,
    pub posts: u64,
    pub last_ts: u64,
}

/// A thread root with its activity (`browse`).
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub root: Stored,
    pub replies: u64,
    /// Answers by others than the root's author.
    pub answers: u64,
    /// The latest `ts` in the thread.
    pub last_ts: u64,
}

/// Outcome of an insert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inserted {
    New(i64),
    Duplicate,
    /// A full event whose body this node dropped (ADR 0020): only its
    /// tombstone is kept, the body is refused. Its blobs must not be
    /// fetched.
    Dropped,
}

/// Why this node dropped a body (ADR 0020 §2). A local record, never
/// shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The human running the node (`wats drop`).
    Own,
    /// The post's author retracted it.
    Retraction,
    /// An operator's erasure label, from a subscribed labeler.
    Erasure,
    /// A subscribed labeler's moderation label.
    Label,
    /// Storage pressure. The only reason whose body is accepted again;
    /// its record expires after [`EVICTED_RECORD_SECS`].
    Evicted,
}

/// How long an `evicted` drop record is kept (ADR 0020 §2): 30 days. The
/// other records are kept forever.
pub const EVICTED_RECORD_SECS: u64 = 30 * 24 * 3600;

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Own => "own",
            DropReason::Retraction => "retraction",
            DropReason::Erasure => "erasure",
            DropReason::Label => "label",
            DropReason::Evicted => "evicted",
        }
    }
}

impl std::fmt::Display for DropReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DropReason {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "own" => DropReason::Own,
            "retraction" => DropReason::Retraction,
            "erasure" => DropReason::Erasure,
            "label" => DropReason::Label,
            "evicted" => DropReason::Evicted,
            other => bail!("unknown drop reason {other:?}"),
        })
    }
}

/// A local drop record (ADR 0020 §0): why and when this node dropped a
/// body, or decided to refuse it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropRecord {
    pub reason: DropReason,
    /// Unix seconds.
    pub at: u64,
    /// The id of the retract event or label that caused the drop.
    pub trigger: Option<String>,
    /// The human's own words (`wats drop --reason`).
    pub detail: Option<String>,
}

/// What [`Store::drop_body`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DropOutcome {
    /// The event was stored here (full or not). When it was not, the record
    /// is kept for later: the body is refused when it arrives.
    pub held: bool,
    /// A full body was replaced by its tombstone.
    pub body_dropped: bool,
    /// Blob hashes of the dropped body that no kept event references any
    /// more: the node deletes them.
    pub freed_blobs: Vec<String>,
}

/// What [`Store::apply_retraction`] did with one referenced post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Held and by the retraction's author (or a key it rotated from): its
    /// body was dropped, or, held as a tombstone, recorded as retracted.
    Dropped(DropOutcome),
    /// Not held yet: recorded, so only its tombstone is stored when it
    /// arrives, once its author is checked then.
    Remembered,
    /// Held, by another author as far as this node knows (someone else's
    /// post, or a rotation link not held yet): ignored, body kept.
    NotTheAuthor,
    /// A key succession or a halt event: never retracted.
    Unretractable,
    /// Already dropped here, by this or another reason: left as it is.
    AlreadyDropped,
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
                "DELETE FROM event_tags; DELETE FROM event_refs; DELETE FROM events;
                 DELETE FROM sync_state; INSERT INTO events_fts (events_fts) VALUES ('rebuild');",
            )?;
        }
        // v4 -> v5: the drop records table (ADR 0020), created by SCHEMA
        // above. Events are kept.
        if (4..6).contains(&version) {
            // v5 -> v6: the citations table (ADR 0019), filled from the
            // events kept.
            conn.execute_batch(
                "INSERT OR IGNORE INTO event_refs (ref, seq)
                 SELECT r.value, e.seq FROM events e, json_each(e.json, '$.refs') r
                 WHERE e.opaque = 0",
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
            // A body this node dropped is refused (ADR 0020 §2): only its
            // tombstone is kept. An evicted one is accepted again.
            let dropped: Option<String> = tx
                .query_row(
                    "SELECT reason FROM dropped WHERE id = ?",
                    [&event.id],
                    |r| r.get(0),
                )
                .optional()?;
            let refuse = match dropped.as_deref() {
                None => false,
                Some(reason) if reason == DropReason::Evicted.as_str() => {
                    tx.execute("DELETE FROM dropped WHERE id = ?", [&event.id])?;
                    false
                }
                // Recorded before the post arrived (ADR 0020 §5.1): only now
                // can its author be checked, against every retraction held
                // that cites it. None matches (someone else's post, or a
                // rotation link still missing): the record goes, the body is
                // stored; a later `rotate` applies the retraction again.
                Some(reason) if reason == DropReason::Retraction.as_str() => {
                    let trigger = if crate::retract::unretractable(event.kind.as_str()) {
                        None
                    } else {
                        retraction_citing(&tx, &event.id, &event.author)?
                    };
                    match trigger {
                        Some(trigger) => {
                            tx.execute(
                                "UPDATE dropped SET trigger = ?1 WHERE id = ?2",
                                params![trigger, event.id],
                            )?;
                            true
                        }
                        None => {
                            tx.execute("DELETE FROM dropped WHERE id = ?", [&event.id])?;
                            false
                        }
                    }
                }
                Some(_) => true,
            };
            if refuse {
                let mut added = 0;
                if existing.is_none() {
                    let tombstone = crate::wire::tombstone(&serde_json::to_value(event)?)?;
                    let json = serde_json::to_string(&tombstone.wire)?;
                    insert_opaque_row(&tx, &tombstone, &json, received_at)?;
                    added = json.len() as u64;
                }
                tx.commit()?;
                self.opaque_bytes.fetch_add(added, Ordering::Relaxed);
                return Ok(Inserted::Dropped);
            }
            let mut freed = 0;
            match existing {
                Some((_, false, _)) => {
                    // Commits the removal of an evicted record, if any.
                    tx.commit()?;
                    return Ok(Inserted::Duplicate);
                }
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
            for cited in &event.refs {
                tx.execute(
                    "INSERT OR IGNORE INTO event_refs (ref, seq) VALUES (?, ?)",
                    params![cited, seq],
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
        let changed = insert_opaque_row(&conn, raw, &json, received_at)?;
        Ok(if changed == 0 {
            Inserted::Duplicate
        } else {
            self.opaque_bytes.fetch_add(len, Ordering::Relaxed);
            Inserted::New(conn.last_insert_rowid())
        })
    }

    /// Drop the body of event `id` in place (ADR 0020 §2), in one
    /// transaction. A full event becomes its tombstone: same row and `seq`,
    /// header columns kept (author, ts, net, thread, parent, kind), no
    /// full-text entry, no tags, no `ask_notify` row. Whatever the row (full, already opaque,
    /// or absent: remembered for later), the drop record is written or
    /// updated, so that the body is refused when it arrives.
    pub fn drop_body(
        &self,
        id: &str,
        reason: DropReason,
        trigger: Option<&str>,
        detail: Option<&str>,
        now: u64,
    ) -> Result<DropOutcome> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let row: Option<(i64, bool, String, String, String)> = tx
            .query_row(
                "SELECT seq, opaque, text, tags, json FROM events WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let mut outcome = DropOutcome {
            held: row.is_some(),
            ..DropOutcome::default()
        };
        let mut added = 0;
        if let Some((seq, false, text, tags, json)) = row {
            let full: Value = serde_json::from_str(&json).context("corrupt stored event")?;
            let tombstone = crate::wire::tombstone(&full)?;
            let tombstone = serde_json::to_string(&tombstone.wire)?;
            // FTS5 removes an entry given the exact values it indexed; an
            // opaque row keeps an empty entry, as `insert_raw` gives it.
            tx.execute(
                "INSERT INTO events_fts (events_fts, rowid, text, tags) VALUES ('delete', ?, ?, ?)",
                params![seq, text, tags],
            )?;
            tx.execute(
                "INSERT INTO events_fts (rowid, text, tags) VALUES (?, '', '')",
                [seq],
            )?;
            tx.execute("DELETE FROM event_tags WHERE seq = ?", [seq])?;
            tx.execute("DELETE FROM event_refs WHERE seq = ?", [seq])?;
            // Which of the human's clients posted it goes with the body.
            tx.execute("DELETE FROM ask_notify WHERE ask = ?", [id])?;
            tx.execute(
                "UPDATE events SET json = ?, opaque = 1, text = '', tags = '' WHERE seq = ?",
                params![tombstone, seq],
            )?;
            added = tombstone.len() as u64;
            outcome.body_dropped = true;
            let hashes = full
                .get("blobs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|b| b.get("hash").and_then(Value::as_str));
            for hash in hashes {
                if !outcome.freed_blobs.iter().any(|h| h == hash) && !blob_referenced(&tx, hash)? {
                    outcome.freed_blobs.push(hash.to_string());
                }
            }
        }
        tx.execute(
            "INSERT INTO dropped (id, reason, at, trigger, detail) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (id) DO UPDATE SET reason = excluded.reason, at = excluded.at,
                 trigger = excluded.trigger, detail = excluded.detail",
            params![
                id,
                reason.as_str(),
                now.min(i64::MAX as u64) as i64,
                trigger,
                detail
            ],
        )?;
        tx.commit()?;
        self.opaque_bytes.fetch_add(added, Ordering::Relaxed);
        Ok(outcome)
    }

    /// The drop record of event `id`, if this node dropped its body.
    pub fn drop_record(&self, id: &str) -> Result<Option<DropRecord>> {
        let row: Option<(String, i64, Option<String>, Option<String>)> = self
            .conn()
            .query_row(
                "SELECT reason, at, trigger, detail FROM dropped WHERE id = ?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        row.map(|(reason, at, trigger, detail)| {
            Ok(DropRecord {
                reason: reason.parse()?,
                at: at.max(0) as u64,
                trigger,
                detail,
            })
        })
        .transpose()
    }

    /// Delete the `evicted` drop records older than `before` (unix
    /// seconds): after 30 days an evicted body is no longer remembered
    /// (ADR 0020 §2). Returns how many were deleted.
    pub fn expire_evicted_records(&self, before: u64) -> Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM dropped WHERE reason = ?1 AND at < ?2",
            params![
                DropReason::Evicted.as_str(),
                before.min(i64::MAX as u64) as i64
            ],
        )?)
    }

    /// Whether `author` is `successor` or a key that rotated into it,
    /// through the `rotate` events this node holds (ADR 0010 A5).
    pub fn rotated_into(&self, successor: &str, author: &str) -> Result<bool> {
        rotated_into(&self.conn(), successor, author)
    }

    /// `key` and every key it rotated to, directly or through a chain of
    /// `rotate` events this node holds in full.
    pub fn rotation_successors(&self, key: &str) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT json_extract(json, '$.data.new_key') FROM events
                 WHERE kind = 'rotate' AND opaque = 0 AND v = 1 AND author = ?",
        )?;
        let mut keys = vec![key.to_string()];
        let mut next = 0;
        while let Some(current) = keys.get(next).cloned() {
            next += 1;
            let rows = stmt.query_map([&current], |r| r.get::<_, Option<String>>(0))?;
            for successor in rows {
                if let Some(successor) = successor?
                    && keys.len() < MAX_ROTATION_KEYS
                    && !keys.contains(&successor)
                {
                    keys.push(successor);
                }
            }
        }
        Ok(keys)
    }

    /// The retractions held here (full or tombstone) by these authors,
    /// oldest first.
    pub fn retractions_by(&self, authors: &[String]) -> Result<Vec<crate::retract::Retraction>> {
        if authors.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; authors.len()].join(",");
        let sql = format!(
            "SELECT seq, json FROM events WHERE kind = ? AND v = 1 AND author IN ({marks})
             ORDER BY seq"
        );
        let mut args = vec![SqlValue::from(crate::retract::KIND.to_string())];
        args.extend(authors.iter().map(|a| SqlValue::from(a.clone())));
        Ok(self
            .query_raw(&sql, args)?
            .into_iter()
            .filter_map(|wire| match crate::wire::AnyEvent::from_value(wire) {
                Ok(event) => crate::retract::Retraction::from_any(&event),
                Err(_) => None,
            })
            .collect())
    }

    /// Apply a retraction this node holds (ADR 0020 §5.1), post by post:
    /// see [`Applied`]. Posts already dropped are left as they are, so it
    /// can run again (when a `rotate` arrives) at no cost.
    pub fn apply_retraction(
        &self,
        retraction: &crate::retract::Retraction,
        now: u64,
    ) -> Result<Vec<(String, Applied)>> {
        let mut out = Vec::new();
        for id in &retraction.refs {
            if *id == retraction.id {
                continue;
            }
            let (row, record) = {
                let conn = self.conn();
                let row: Option<(String, String, bool)> = conn
                    .query_row(
                        "SELECT author, kind, opaque FROM events WHERE id = ?",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                let record: Option<String> = conn
                    .query_row("SELECT reason FROM dropped WHERE id = ?", [id], |r| {
                        r.get(0)
                    })
                    .optional()?;
                (row, record)
            };
            // A drop for another reason (the human's, a label) stays as it
            // is: a retraction never turns it into one that a later
            // author check could undo.
            let other = record.as_deref().is_some_and(|r| {
                r != DropReason::Retraction.as_str() && r != DropReason::Evicted.as_str()
            });
            let retracted = record.as_deref() == Some(DropReason::Retraction.as_str());
            let applied = match row {
                Some((_, kind, _)) if crate::retract::unretractable(&kind) => {
                    Applied::Unretractable
                }
                Some((author, _, _)) if !self.rotated_into(&retraction.author, &author)? => {
                    Applied::NotTheAuthor
                }
                Some((_, _, opaque)) if other || (opaque && retracted) => Applied::AlreadyDropped,
                Some(_) => Applied::Dropped(self.drop_body(
                    id,
                    DropReason::Retraction,
                    Some(&retraction.id),
                    None,
                    now,
                )?),
                None if other || retracted => Applied::AlreadyDropped,
                None => {
                    self.drop_body(id, DropReason::Retraction, Some(&retraction.id), None, now)?;
                    Applied::Remembered
                }
            };
            out.push((id.clone(), applied));
        }
        Ok(out)
    }

    /// Every blob hash listed by a kept full event: the blobs the node
    /// keeps from garbage collection.
    pub fn referenced_blobs(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT json_extract(b.value, '$.hash') FROM events e,
                 json_each(e.json, '$.blobs') b WHERE e.opaque = 0",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.extend(row?);
        }
        Ok(out)
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

    /// The stored v2 `submit` events addressed to `inbox` (`ext.to`), as
    /// wire JSON, oldest first. Tombstones are left out.
    pub fn submits_to(&self, inbox: &str) -> Result<Vec<Value>> {
        let rows = self.query_raw(
            "SELECT seq, json FROM events
             WHERE opaque = 1 AND v = 2 AND kind = 'submit'
               AND json_extract(json, '$.ext.to') = ?
             ORDER BY seq",
            vec![inbox.to_string().into()],
        )?;
        Ok(rows
            .into_iter()
            .filter(|e| {
                e.get("ext")
                    .and_then(|x| x.get("to"))
                    .and_then(Value::as_str)
                    == Some(inbox)
                    && e.get("ct").is_some()
            })
            .collect())
    }

    /// Open the store of a data directory read-only, next to a running
    /// node (WAL), e.g. for `wats inbox read`.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        Self::open_read_only_with(path, std::time::Duration::from_secs(5))
    }

    /// [`Store::open_read_only`], waiting at most `busy` for the writer
    /// (the session hook must not hold up a prompt).
    pub fn open_read_only_with(path: &Path, busy: std::time::Duration) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(busy)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            bail!(
                "store schema version {version}, this wats reads {SCHEMA_VERSION}: start the node once"
            );
        }
        let (new_events, _) = broadcast::channel(1);
        Ok(Self {
            conn: Mutex::new(conn),
            new_events,
            opaque_bytes: AtomicU64::new(0),
        })
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

    /// The tags of the understood events, busiest first (`browse`): how
    /// many threads and posts carry each, and their latest `ts`. `prefix`
    /// keeps the tags that start with it (e.g. `cap:`).
    pub fn tag_counts(
        &self,
        net: Option<&str>,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TagCount>> {
        let mut conds = vec!["e.opaque = 0".to_string()];
        let mut args: Vec<SqlValue> = Vec::new();
        if let Some(net) = net {
            conds.push("e.net = ?".into());
            args.push(net.to_string().into());
        }
        if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
            conds.push("substr(t.tag, 1, ?) = ?".into());
            args.push((prefix.chars().count() as i64).into());
            args.push(prefix.to_string().into());
        }
        args.push((limit as i64).into());
        let sql = format!(
            "SELECT t.tag, count(DISTINCT coalesce(e.thread, e.id)), count(*), max(e.ts)
             FROM event_tags t JOIN events e ON e.seq = t.seq
             WHERE {} GROUP BY t.tag ORDER BY 2 DESC, 4 DESC, t.tag LIMIT ?",
            conds.join(" AND ")
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok(TagCount {
                tag: r.get(0)?,
                threads: r.get::<_, i64>(1)? as u64,
                posts: r.get::<_, i64>(2)? as u64,
                last_ts: r.get::<_, i64>(3)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Thread roots by latest activity (`browse`), optionally of one network,
    /// one kind, and carrying one tag, with their reply and answer counts.
    /// Replies from another network than the root's are not counted (B9).
    pub fn thread_summaries(
        &self,
        net: Option<&str>,
        tag: Option<&str>,
        kind: Option<&Kind>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ThreadSummary>> {
        let mut conds = vec!["e.thread IS NULL".to_string(), "e.opaque = 0".to_string()];
        let mut args: Vec<SqlValue> = Vec::new();
        if let Some(net) = net {
            conds.push("e.net = ?".into());
            args.push(net.to_string().into());
        }
        if let Some(kind) = kind {
            conds.push("e.kind = ?".into());
            args.push(kind.as_str().to_string().into());
        }
        if let Some(tag) = tag {
            conds.push(
                "EXISTS (SELECT 1 FROM event_tags t WHERE t.seq = e.seq AND t.tag = ?)".into(),
            );
            args.push(tag.to_string().into());
        }
        args.push((limit as i64).into());
        args.push((offset as i64).into());
        let reply = "r.thread = e.id AND r.opaque = 0 AND r.net = e.net";
        let sql = format!(
            "SELECT e.seq, e.json,
                (SELECT count(*) FROM events r WHERE {reply}),
                (SELECT count(*) FROM events r WHERE {reply} AND r.kind = 'answer' AND r.author <> e.author),
                max(e.ts, coalesce((SELECT max(r.ts) FROM events r WHERE {reply}), 0)) AS last
             FROM events e WHERE {} ORDER BY last DESC, e.seq DESC LIMIT ? OFFSET ?",
            conds.join(" AND ")
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, json, replies, answers, last) = row?;
            let event: Event = serde_json::from_str(&json).context("corrupt stored event")?;
            out.push(ThreadSummary {
                root: Stored {
                    seq,
                    event: Arc::new(event),
                },
                replies: replies as u64,
                answers: answers as u64,
                last_ts: last as u64,
            });
        }
        Ok(out)
    }

    /// Remember that replies to `ask` go to the human's MCP host `client`.
    pub fn set_ask_notify(&self, ask: &str, client: &str, at: u64) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO ask_notify (ask, client, at) VALUES (?, ?, ?)",
            params![ask, client, at as i64],
        )?;
        Ok(())
    }

    /// The MCP host to notify of replies to `ask`, if any.
    pub fn ask_notify(&self, ask: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT client FROM ask_notify WHERE ask = ?", [ask], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Full-text search (BM25), newest first among equal ranks. Without
    /// text, the newest matching events.
    pub fn search(&self, text: Option<&str>, filter: &Filter, limit: usize) -> Result<Vec<Stored>> {
        Ok(self
            .search_scored(text, filter, limit)?
            .into_iter()
            .map(|(stored, _)| stored)
            .collect())
    }

    /// [`Store::search`], with each event's BM25 score: negative, lower is
    /// more relevant; `None` without text.
    pub fn search_scored(
        &self,
        text: Option<&str>,
        filter: &Filter,
        limit: usize,
    ) -> Result<Vec<(Stored, Option<f64>)>> {
        let mut conds = Vec::new();
        let mut args = Vec::new();
        let fts = text.and_then(fts_query);
        let (score, from, order) = if let Some(q) = fts {
            conds.push("events_fts MATCH ?".to_string());
            args.push(q.into());
            (
                "bm25(events_fts)",
                "events_fts JOIN events e ON e.seq = events_fts.rowid",
                "bm25(events_fts), e.ts DESC",
            )
        } else {
            ("NULL", "events e", "e.ts DESC, e.seq DESC")
        };
        filter.to_sql(&mut conds, &mut args);
        let filter_sql = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };
        args.push((limit as i64).into());
        let sql = format!(
            "SELECT e.seq, e.json, {score} FROM {from} {filter_sql} ORDER BY {order} LIMIT ?"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<f64>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (seq, json, score) = row?;
            let event: Event = serde_json::from_str(&json).context("corrupt stored event")?;
            out.push((
                Stored {
                    seq,
                    event: Arc::new(event),
                },
                score,
            ));
        }
        Ok(out)
    }

    /// The understood events that reply to one of `targets` (`parent`) or
    /// cite it (`refs`), each with the target it points to: what the
    /// signals of those posts are computed from (ADR 0019 §1). At most
    /// `limit` rows.
    pub fn signal_rows(&self, targets: &[String], limit: usize) -> Result<Vec<SignalRow>> {
        if targets.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; targets.len()].join(",");
        let columns = "e.id, e.author, e.ts, e.net, e.kind, json_extract(e.json, '$.data'),
             coalesce(json_extract(e.json, '$.title'), substr(e.text, 1, 200))";
        let sql = format!(
            "SELECT e.parent, 1, {columns} FROM events e
                 WHERE e.parent IN ({marks}) AND e.opaque = 0
             UNION ALL
             SELECT r.ref, 0, {columns} FROM event_refs r JOIN events e ON e.seq = r.seq
                 WHERE r.ref IN ({marks}) AND e.opaque = 0
             LIMIT ?"
        );
        let mut args: Vec<SqlValue> = Vec::new();
        for _ in 0..2 {
            args.extend(targets.iter().map(|t| SqlValue::from(t.clone())));
        }
        args.push((limit as i64).into());
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok(SignalRow {
                target: r.get(0)?,
                reply: r.get(1)?,
                id: r.get(2)?,
                author: r.get(3)?,
                ts: r.get::<_, i64>(4)?.max(0) as u64,
                net: r.get(5)?,
                kind: r.get(6)?,
                data: r
                    .get::<_, Option<String>>(7)?
                    .and_then(|d| serde_json::from_str(&d).ok())
                    .unwrap_or_default(),
                title: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// `(id, author, kind, net)` of each of these events held here and
    /// understood: thread roots, for their asker.
    pub fn heads(&self, ids: &[String]) -> Result<Vec<(String, String, String, String)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT id, author, kind, net FROM events WHERE id IN ({}) AND opaque = 0",
            vec!["?"; ids.len()].join(",")
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(ids.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The `react` replies of these threads that carry a boolean
    /// `data.accept`.
    pub fn accept_reacts(&self, threads: &[String]) -> Result<Vec<AcceptRow>> {
        if threads.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT thread, id, author, ts, parent, json_extract(json, '$.data.accept')
             FROM events WHERE thread IN ({}) AND kind = 'react' AND opaque = 0
                 AND parent IS NOT NULL
                 AND json_type(json, '$.data.accept') IN ('true', 'false')
             LIMIT 10000",
            vec!["?"; threads.len()].join(",")
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(threads.iter()), |r| {
            Ok(AcceptRow {
                thread: r.get(0)?,
                id: r.get(1)?,
                author: r.get(2)?,
                ts: r.get::<_, i64>(3)?.max(0) as u64,
                parent: r.get(4)?,
                accept: r.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Whether any key succession (`rotate`) is held: without one, no two
    /// keys are the same author's.
    pub fn has_rotations(&self) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM events WHERE kind = 'rotate' AND opaque = 0)",
            [],
            |r| r.get(0),
        )?)
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

    /// Halt-kind events of a network (ADR 0016) by these authors, as
    /// understood events (tombstones, being opaque, are left out), oldest
    /// first.
    pub fn halt_events(&self, net: &str, authors: &[String]) -> Result<Vec<Event>> {
        if authors.is_empty() {
            return Ok(Vec::new());
        }
        let kinds = crate::halt::KINDS;
        let mut args: Vec<SqlValue> = vec![net.to_string().into()];
        args.extend(kinds.iter().map(|k| SqlValue::from(k.to_string())));
        args.extend(authors.iter().map(|a| SqlValue::from(a.clone())));
        let marks = |n: usize| vec!["?"; n].join(", ");
        let sql = format!(
            "SELECT seq, json FROM events WHERE opaque = 0 AND net = ? AND kind IN ({})
             AND author IN ({}) ORDER BY ts, id LIMIT 10000",
            marks(kinds.len()),
            marks(authors.len())
        );
        Ok(self
            .query(&sql, args)?
            .into_iter()
            .map(|s| (*s.event).clone())
            .collect())
    }

    /// Like [`Store::page_after`], halt-kind events only: what a node
    /// serving a halt (`halt.serve`) answers to `sync`.
    pub fn halt_page_after(
        &self,
        net: &str,
        after_ts: u64,
        after_id: &str,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let [k1, k2, k3] = crate::halt::KINDS;
        self.query_raw(
            "SELECT seq, json FROM events WHERE net = ?1 AND (ts > ?2 OR (ts = ?2 AND id > ?3))
             AND kind IN (?5, ?6, ?7) AND opaque = 0 ORDER BY ts, id LIMIT ?4",
            vec![
                net.to_string().into(),
                (after_ts as i64).into(),
                after_id.to_string().into(),
                (limit as i64).into(),
                k1.to_string().into(),
                k2.to_string().into(),
                k3.to_string().into(),
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
        let raw = crate::wire::tombstone(&serde_json::to_value(event).unwrap()).unwrap();
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

    fn with_blobs(key: u8, ts: u64, text: &str, hashes: &[&str]) -> Event {
        Draft {
            net: "public".into(),
            kind: Some(Kind::Offer),
            title: Some(text.into()),
            text: text.into(),
            blobs: hashes
                .iter()
                .map(|h| crate::event::BlobRef {
                    hash: h.to_string(),
                    name: "file.txt".into(),
                    size: 3,
                    url: None,
                })
                .collect(),
            ..Draft::default()
        }
        .sign(&SigningKey::from_bytes(&[key; 32]), ts, 0)
        .unwrap()
    }

    fn by_tag(store: &Store, tag: &str) -> usize {
        store
            .search(
                None,
                &Filter {
                    tags: vec![tag.into()],
                    ..Filter::default()
                },
                10,
            )
            .unwrap()
            .len()
    }

    fn found(store: &Store, text: &str) -> usize {
        store
            .search(Some(text), &Filter::default(), 10)
            .unwrap()
            .len()
    }

    #[test]
    fn drop_body_keeps_the_header_and_forgets_the_body() {
        let store = Store::open_in_memory().unwrap();
        let root = post(1, 10, Kind::Ask, &["cap:pdf"], "a leaked secret phrase");
        let answer = reply(&root, 2, 11, "an answer stays");
        store.insert(&root, 10).unwrap();
        store.insert(&answer, 11).unwrap();
        let seq = store.get(&root.id).unwrap().unwrap().seq;
        store.set_ask_notify(&root.id, "claude", 10).unwrap();
        assert_eq!(found(&store, "secret"), 1);
        assert_eq!(store.tag_counts(None, None, 10).unwrap().len(), 1);
        assert_eq!(
            store
                .thread_summaries(None, None, None, 0, 10)
                .unwrap()
                .len(),
            1
        );

        let outcome = store
            .drop_body(&root.id, DropReason::Own, None, Some("personal data"), 100)
            .unwrap();
        assert!(outcome.held && outcome.body_dropped);
        assert!(outcome.freed_blobs.is_empty());
        // Not readable, not searchable, not tagged, not listed by browse,
        // and which client posted it is forgotten.
        assert!(store.get(&root.id).unwrap().is_none());
        assert_eq!(store.ask_notify(&root.id).unwrap(), None);
        assert_eq!(found(&store, "secret"), 0);
        assert_eq!(found(&store, "pdf"), 0);
        assert_eq!(by_tag(&store, "cap:pdf"), 0);
        assert!(store.tag_counts(None, None, 10).unwrap().is_empty());
        assert!(
            store
                .thread_summaries(None, None, None, 0, 10)
                .unwrap()
                .is_empty()
        );
        let tag_rows: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM event_tags WHERE seq = ?",
                [seq],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tag_rows, 0);
        // The row is the tombstone, with its seq and header columns.
        let (v, opaque, raw) = store.get_raw(&root.id).unwrap().unwrap();
        assert_eq!((v, opaque), (1, true));
        assert_eq!(raw, tombstone_of(&root).to_value());
        let (kept_seq, author, ts, kind): (i64, String, i64, String) = store
            .conn()
            .query_row(
                "SELECT seq, author, ts, kind FROM events WHERE id = ?",
                [&root.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (kept_seq, author, ts, kind.as_str()),
            (seq, root.author.clone(), 10, "ask")
        );
        // Sync pages serve the tombstone.
        let page = store.page_after("public", 0, "", 10).unwrap();
        let served = page.iter().find(|e| e["id"] == root.id).unwrap();
        assert!(served.get("text").is_none() && served.get("body_hash").is_some());
        // The thread survives: the reply still resolves.
        assert_eq!(store.thread_opaque(&root.id, 10).unwrap().len(), 1);
        assert_eq!(store.thread(&root.id, 10).unwrap().len(), 1);
        assert_eq!(
            store.drop_record(&root.id).unwrap().unwrap(),
            DropRecord {
                reason: DropReason::Own,
                at: 100,
                trigger: None,
                detail: Some("personal data".into()),
            }
        );
        // Dropping again updates the record; the index stays sound.
        let again = store
            .drop_body(
                &root.id,
                DropReason::Retraction,
                Some(&answer.id),
                None,
                200,
            )
            .unwrap();
        assert!(again.held && !again.body_dropped);
        let record = store.drop_record(&root.id).unwrap().unwrap();
        assert_eq!(record.reason, DropReason::Retraction);
        assert_eq!(record.trigger.as_deref(), Some(answer.id.as_str()));
        store
            .conn()
            .execute(
                "INSERT INTO events_fts (events_fts) VALUES ('integrity-check')",
                [],
            )
            .unwrap();
        assert!(store.drop_record(&answer.id).unwrap().is_none());
    }

    #[test]
    fn a_dropped_body_is_refused_afterwards() {
        let store = Store::open_in_memory().unwrap();
        let event = post(1, 10, Kind::Note, &["cap:pdf"], "drop me");
        store.insert(&event, 10).unwrap();
        store
            .drop_body(&event.id, DropReason::Own, None, None, 20)
            .unwrap();
        let mut rx = store.subscribe();
        assert_eq!(store.insert(&event, 30).unwrap(), Inserted::Dropped);
        assert!(rx.try_recv().is_err(), "nothing new to announce");
        assert!(store.get(&event.id).unwrap().is_none());
        let (_, opaque, raw) = store.get_raw(&event.id).unwrap().unwrap();
        assert!(opaque && raw.get("text").is_none());
        assert_eq!(found(&store, "drop"), 0);
        // A tombstone arriving is a duplicate.
        assert_eq!(
            store.insert_raw(&tombstone_of(&event), 31).unwrap(),
            Inserted::Duplicate
        );
    }

    #[test]
    fn a_drop_record_for_an_absent_event_keeps_only_its_tombstone() {
        let store = Store::open_in_memory().unwrap();
        let event = post(1, 10, Kind::Note, &["cap:pdf"], "not here yet");
        // A retraction record is checked against its author on arrival
        // (see the retraction tests); a human's record is not.
        let outcome = store
            .drop_body(&event.id, DropReason::Own, None, None, 5)
            .unwrap();
        assert_eq!(outcome, DropOutcome::default());
        assert!(store.get_raw(&event.id).unwrap().is_none());
        let before = store.opaque_bytes();
        assert_eq!(store.insert(&event, 10).unwrap(), Inserted::Dropped);
        let (_, opaque, raw) = store.get_raw(&event.id).unwrap().unwrap();
        assert!(opaque);
        assert_eq!(raw, tombstone_of(&event).to_value());
        assert!(store.opaque_bytes() > before);
        assert_eq!(store.insert(&event, 11).unwrap(), Inserted::Dropped);
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(found(&store, "here"), 0);
    }

    #[test]
    fn an_evicted_body_is_accepted_again_and_its_record_expires() {
        let store = Store::open_in_memory().unwrap();
        let event = post(1, 10, Kind::Note, &["cap:pdf"], "evict me");
        store.insert(&event, 10).unwrap();
        store
            .drop_body(&event.id, DropReason::Evicted, None, None, 100)
            .unwrap();
        assert!(store.get(&event.id).unwrap().is_none());
        assert!(matches!(
            store.insert(&event, 120).unwrap(),
            Inserted::New(_)
        ));
        assert_eq!(
            store.get(&event.id).unwrap().unwrap().event.text,
            "evict me"
        );
        assert!(
            store.drop_record(&event.id).unwrap().is_none(),
            "record removed"
        );
        assert_eq!(found(&store, "evict"), 1);
        assert_eq!(by_tag(&store, "cap:pdf"), 1);

        // Only evicted records expire, once older than the cutoff.
        let other = post(2, 11, Kind::Note, &[], "own drop");
        let fresh = "ef".repeat(32);
        let cutoff = 100 + EVICTED_RECORD_SECS;
        store
            .drop_body(&event.id, DropReason::Evicted, None, None, 100)
            .unwrap();
        store
            .drop_body(&other.id, DropReason::Own, None, None, 100)
            .unwrap();
        store
            .drop_body(&fresh, DropReason::Evicted, None, None, cutoff)
            .unwrap();
        assert_eq!(store.expire_evicted_records(cutoff).unwrap(), 1);
        assert!(store.drop_record(&event.id).unwrap().is_none());
        assert!(store.drop_record(&other.id).unwrap().is_some());
        assert!(store.drop_record(&fresh).unwrap().is_some());
    }

    #[test]
    fn drop_body_frees_the_blobs_no_kept_event_lists() {
        let store = Store::open_in_memory().unwrap();
        let (shared, alone) = ("aa".repeat(32), "bb".repeat(32));
        let dropped = with_blobs(1, 10, "two files", &[&shared, &alone]);
        let kept = with_blobs(2, 11, "one of them", &[&shared]);
        store.insert(&dropped, 10).unwrap();
        store.insert(&kept, 11).unwrap();
        let mut referenced = store.referenced_blobs().unwrap();
        referenced.sort();
        assert_eq!(referenced, vec![shared.clone(), alone.clone()]);
        let outcome = store
            .drop_body(&dropped.id, DropReason::Own, None, None, 20)
            .unwrap();
        assert_eq!(outcome.freed_blobs, vec![alone]);
        assert_eq!(store.referenced_blobs().unwrap(), vec![shared]);
    }

    #[test]
    fn migrates_schema_v4_keeping_events() {
        let store = Store::open_in_memory().unwrap();
        let event = post(1, 10, Kind::Note, &["cap:pdf"], "kept across the migration");
        store.insert(&event, 10).unwrap();
        let conn = store.conn.into_inner().unwrap();
        conn.execute_batch("DROP TABLE dropped; PRAGMA user_version = 4;")
            .unwrap();
        let store = Store::init(conn).unwrap();
        assert_eq!(store.count().unwrap(), 1, "no resync");
        assert_eq!(found(&store, "migration"), 1);
        store
            .drop_body(&event.id, DropReason::Own, None, None, 20)
            .unwrap();
        assert!(store.drop_record(&event.id).unwrap().is_some());
        let conn = store.conn.into_inner().unwrap();
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn migrates_schema_v5_indexing_the_citations_kept() {
        let store = Store::open_in_memory().unwrap();
        let cited = post(1, 10, Kind::Note, &[], "cited");
        let citing = Draft {
            net: "public".into(),
            kind: Some(Kind::Note),
            title: Some("citing".into()),
            text: "builds on it".into(),
            refs: vec![cited.id.clone()],
            ..Draft::default()
        }
        .sign(&SigningKey::from_bytes(&[2; 32]), 11, 0)
        .unwrap();
        store.insert(&cited, 10).unwrap();
        store.insert(&citing, 11).unwrap();
        let conn = store.conn.into_inner().unwrap();
        conn.execute_batch("DROP TABLE event_refs; PRAGMA user_version = 5;")
            .unwrap();
        let store = Store::init(conn).unwrap();
        let rows = store
            .signal_rows(std::slice::from_ref(&cited.id), 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].id.as_str(), rows[0].reply),
            (citing.id.as_str(), false)
        );
    }

    #[test]
    fn read_only_opens_the_current_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let store = Store::open(&path).unwrap();
        let event = post(1, 10, Kind::Note, &[], "read me");
        store.insert(&event, 10).unwrap();
        store
            .drop_body(&event.id, DropReason::Own, None, None, 20)
            .unwrap();
        let ro = Store::open_read_only(&path).unwrap();
        assert_eq!(
            ro.drop_record(&event.id).unwrap().unwrap().reason,
            DropReason::Own
        );
    }

    #[test]
    fn drop_reasons_round_trip() {
        for reason in [
            DropReason::Own,
            DropReason::Retraction,
            DropReason::Erasure,
            DropReason::Label,
            DropReason::Evicted,
        ] {
            assert_eq!(reason.as_str().parse::<DropReason>().unwrap(), reason);
        }
        assert!("whim".parse::<DropReason>().is_err());
    }

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// A retraction by `seed` of `refs`, as stored and applied.
    fn retraction(seed: u8, ts: u64, refs: &[&str]) -> (Event, crate::retract::Retraction) {
        let draft =
            crate::retract::draft("public", refs.iter().map(|r| r.to_string()).collect(), None)
                .unwrap();
        let event = draft.sign(&key(seed), ts, 0).unwrap();
        let applied = crate::retract::Retraction::from_event(&event).unwrap();
        (event, applied)
    }

    /// `old` hands over to `new` (both sign).
    fn rotate(old: u8, new: u8, ts: u64) -> Event {
        let next = key(new).verifying_key();
        Draft {
            net: "public".into(),
            kind: Some(Kind::Rotate),
            title: Some("rotate".into()),
            text: "handing over".into(),
            data: serde_json::json!({
                "new_key": hex::encode(next.as_bytes()),
                "new_key_sig": crate::event::rotate_signature(&key(new), key(old).verifying_key().as_bytes()),
            })
            .as_object()
            .unwrap()
            .clone(),
            ..Draft::default()
        }
        .sign(&key(old), ts, 0)
        .unwrap()
    }

    #[test]
    fn a_retraction_drops_the_authors_post_and_refuses_it_afterwards() {
        let store = Store::open_in_memory().unwrap();
        let mine = post(1, 10, Kind::Note, &["cap:x"], "my leaked key");
        let theirs = post(2, 11, Kind::Note, &[], "someone else's post");
        store.insert(&mine, 10).unwrap();
        store.insert(&theirs, 11).unwrap();
        let (event, r) = retraction(1, 20, &[&mine.id, &theirs.id]);
        store.insert(&event, 20).unwrap();
        let applied = store.apply_retraction(&r, 30).unwrap();
        let outcome = |id: &str| applied.iter().find(|(i, _)| i == id).unwrap().1.clone();
        assert!(matches!(outcome(&mine.id), Applied::Dropped(o) if o.body_dropped));
        // Someone else's post: ignored, body kept.
        assert_eq!(outcome(&theirs.id), Applied::NotTheAuthor);
        assert!(store.get(&theirs.id).unwrap().is_some());
        assert!(store.drop_record(&theirs.id).unwrap().is_none());
        assert!(store.get(&mine.id).unwrap().is_none());
        let record = store.drop_record(&mine.id).unwrap().unwrap();
        assert_eq!(record.reason, DropReason::Retraction);
        assert_eq!(record.trigger.as_deref(), Some(event.id.as_str()));
        // Refused afterwards; applying again changes nothing.
        assert_eq!(store.insert(&mine, 40).unwrap(), Inserted::Dropped);
        assert!(store.get(&mine.id).unwrap().is_none());
        let again = store.apply_retraction(&r, 50).unwrap();
        assert_eq!(again[0].1, Applied::AlreadyDropped);
        assert_eq!(store.drop_record(&mine.id).unwrap().unwrap().at, 30);
    }

    #[test]
    fn a_retraction_before_its_target_keeps_only_the_tombstone() {
        let store = Store::open_in_memory().unwrap();
        let mine = post(1, 10, Kind::Note, &[], "not here yet");
        let (event, r) = retraction(1, 20, &[&mine.id]);
        store.insert(&event, 20).unwrap();
        assert_eq!(
            store.apply_retraction(&r, 21).unwrap()[0].1,
            Applied::Remembered
        );
        assert_eq!(store.insert(&mine, 30).unwrap(), Inserted::Dropped);
        assert!(store.get(&mine.id).unwrap().is_none());
        let (_, opaque, raw) = store.get_raw(&mine.id).unwrap().unwrap();
        assert!(opaque && raw.get("body_hash").is_some() && raw.get("text").is_none());
        // A forged retraction of the same post by someone else, stored
        // later, does not change who the record trusts.
        let (forged_mine, fm) = retraction(3, 23, &[&mine.id]);
        store.insert(&forged_mine, 23).unwrap();
        assert_eq!(
            store.apply_retraction(&fm, 24).unwrap()[0].1,
            Applied::NotTheAuthor
        );
        assert_eq!(
            store
                .drop_record(&mine.id)
                .unwrap()
                .unwrap()
                .trigger
                .as_deref(),
            Some(event.id.as_str())
        );
    }

    #[test]
    fn a_retraction_of_an_absent_post_by_someone_else_is_undone_on_arrival() {
        let store = Store::open_in_memory().unwrap();
        let theirs = post(2, 10, Kind::Note, &[], "someone else's post");
        // Key 3 claims it before it arrives: recorded, unchecked.
        let (event, r) = retraction(3, 20, &[&theirs.id]);
        store.insert(&event, 20).unwrap();
        assert_eq!(
            store.apply_retraction(&r, 21).unwrap()[0].1,
            Applied::Remembered
        );
        // On arrival the author does not match: the record goes, the body
        // is stored.
        assert!(matches!(
            store.insert(&theirs, 30).unwrap(),
            Inserted::New(_)
        ));
        assert!(store.get(&theirs.id).unwrap().is_some());
        assert!(store.drop_record(&theirs.id).unwrap().is_none());
    }

    #[test]
    fn a_retraction_by_a_successor_key_follows_the_rotation_chain() {
        let store = Store::open_in_memory().unwrap();
        let old = post(4, 10, Kind::Note, &[], "by the old key");
        store.insert(&old, 10).unwrap();
        // Key 6 retracts a post by key 4: 4 -> 5 -> 6, links not held yet.
        let (event, r) = retraction(6, 20, &[&old.id]);
        store.insert(&event, 20).unwrap();
        assert_eq!(
            store.apply_retraction(&r, 21).unwrap()[0].1,
            Applied::NotTheAuthor
        );
        assert!(store.get(&old.id).unwrap().is_some(), "pending: body kept");
        let first = rotate(4, 5, 15);
        store.insert(&first, 22).unwrap();
        assert!(!store.rotated_into(&hex_key(6), &hex_key(4)).unwrap());
        store.insert(&rotate(5, 6, 16), 23).unwrap();
        assert!(store.rotated_into(&hex_key(6), &hex_key(4)).unwrap());
        assert_eq!(
            store.rotation_successors(&hex_key(5)).unwrap(),
            [hex_key(5), hex_key(6)]
        );
        assert_eq!(
            store.retractions_by(&[hex_key(6)]).unwrap(),
            std::slice::from_ref(&r)
        );
        assert!(matches!(
            store.apply_retraction(&r, 24).unwrap()[0].1,
            Applied::Dropped(_)
        ));
        assert!(store.get(&old.id).unwrap().is_none());
        // The chain also lets an absent post by the old key be refused on
        // arrival.
        let later = post(4, 11, Kind::Note, &[], "another one by the old key");
        let (event, r) = retraction(6, 25, &[&later.id]);
        store.insert(&event, 25).unwrap();
        store.apply_retraction(&r, 26).unwrap();
        assert_eq!(store.insert(&later, 27).unwrap(), Inserted::Dropped);
        // Key successions are never retracted.
        let (event, r) = retraction(4, 28, &[&first.id]);
        store.insert(&event, 28).unwrap();
        assert_eq!(
            store.apply_retraction(&r, 29).unwrap()[0].1,
            Applied::Unretractable
        );
        assert!(store.get(&first.id).unwrap().is_some());
    }

    #[test]
    fn a_retraction_keeps_a_drop_for_another_reason() {
        let store = Store::open_in_memory().unwrap();
        let mine = post(1, 10, Kind::Note, &[], "dropped by the human");
        store.insert(&mine, 10).unwrap();
        store
            .drop_body(&mine.id, DropReason::Own, None, Some("mine"), 11)
            .unwrap();
        let (event, r) = retraction(1, 20, &[&mine.id]);
        store.insert(&event, 20).unwrap();
        assert_eq!(
            store.apply_retraction(&r, 21).unwrap()[0].1,
            Applied::AlreadyDropped
        );
        assert_eq!(
            store.drop_record(&mine.id).unwrap().unwrap().reason,
            DropReason::Own
        );
    }

    fn hex_key(seed: u8) -> String {
        hex::encode(key(seed).verifying_key().as_bytes())
    }
}
