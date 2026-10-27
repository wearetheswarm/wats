//! Local event store and index (SPEC §2.6): SQLite with FTS5.
//!
//! Every verified event is stored once, with a local sequence number
//! (`seq`) that gives a stable cursor for `wait` and incremental reads.
//! The index is computed locally from the events; nothing here is shared.

use std::collections::{BTreeMap, HashMap, HashSet};
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

const SCHEMA_VERSION: i64 = 7;

/// The triggers that drop a post's vectors with its body (ADR 0053 §5),
/// as in [`SCHEMA`]: created again after the embeddings table's move to a
/// `(seq, model)` key (0.5.2).
const EMBEDDINGS_TRIGGERS: &str = r#"
CREATE TRIGGER IF NOT EXISTS embeddings_event_delete AFTER DELETE ON events BEGIN
    DELETE FROM embeddings WHERE seq = old.seq;
END;
CREATE TRIGGER IF NOT EXISTS embeddings_body_change
    AFTER UPDATE OF json, text, title, opaque ON events BEGIN
    DELETE FROM embeddings WHERE seq = old.seq;
END;
"#;

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
    opaque      INTEGER NOT NULL DEFAULT 0,
    -- The title, for full-text search (schema 7).
    title       TEXT NOT NULL DEFAULT ''
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
    title, text, tags, content = 'events', content_rowid = 'seq'
);
CREATE TRIGGER IF NOT EXISTS events_fts_insert AFTER INSERT ON events BEGIN
    INSERT INTO events_fts (rowid, title, text, tags)
        VALUES (new.seq, new.title, new.text, new.tags);
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

-- History ranges (ADR 0025 §5): what this node still has to
-- fetch newest first (pending), or chose not to hold (cut, skipped).
CREATE TABLE IF NOT EXISTS sync_ranges (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    net        TEXT NOT NULL,
    lo_ts      INTEGER NOT NULL,
    hi_ts      INTEGER NOT NULL,
    -- The last event fetched, walking down: held above, a hole below.
    cursor_ts  INTEGER NOT NULL,
    cursor_id  TEXT NOT NULL,
    -- pending | cut | skipped | done | unconfirmed
    state      TEXT NOT NULL,
    -- The initial fill's phase (keep, own, knowledge, window); '' for a
    -- catch-up range.
    phase      TEXT NOT NULL DEFAULT '',
    -- When it was opened: the watermark once it and every older pending
    -- range are complete.
    opened     INTEGER NOT NULL,
    -- Store bytes when it was opened: its budget counts from there.
    base_bytes INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS sync_ranges_net ON sync_ranges (net, state);

-- When peers say they first stored an event of a key (`seen` in sync pages,
-- ADR 0025): each peer's earliest report, for at most 8 peers per key. The
-- age floor of reputation counts from their median (ADR 0019 §2.3).
CREATE TABLE IF NOT EXISTS key_seen (
    author TEXT NOT NULL,
    peer   TEXT NOT NULL,
    seen   INTEGER NOT NULL,
    PRIMARY KEY (author, peer)
) WITHOUT ROWID;

-- The peers of `key_seen`: whether each advertised `history_full` in its
-- hello, as of its last reports (0.4.2).
CREATE TABLE IF NOT EXISTS seen_peers (
    peer         TEXT PRIMARY KEY,
    history_full INTEGER NOT NULL
) WITHOUT ROWID;

-- Rotation at the cap (ADR 0025 §7, 0.4.3). Created on open like the
-- tables above: no schema bump.
--
-- The keys of this node's profiles, as they are used: their posts and the
-- threads they posted in are never rotated out, across restarts.
CREATE TABLE IF NOT EXISTS local_keys (
    author  TEXT PRIMARY KEY,
    profile TEXT NOT NULL,
    since   INTEGER NOT NULL
) WITHOUT ROWID;

-- What rows rotation deleted said about their key: when this node first
-- stored one (`received_at`) and the earliest `ts`, so a key's age never
-- goes back when its oldest events go.
CREATE TABLE IF NOT EXISTS key_first (
    author     TEXT NOT NULL,
    net        TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    first_ts   INTEGER NOT NULL,
    PRIMARY KEY (author, net)
) WITHOUT ROWID;

-- Blobs that no kept post lists any more since rotation dropped or deleted
-- the posts that did, written in the rotation's transaction: untagged
-- after it, also after a crash in between.
CREATE TABLE IF NOT EXISTS unpin_pending (
    hash TEXT PRIMARY KEY
) WITHOUT ROWID;

-- Blobs whose posts rotation dropped but that this node keeps, and why
-- (`replica`: it is one of the k peers responsible for it): blob retention
-- follows blob responsibility, not whether the body is held. What a later
-- blob eviction starts from.
CREATE TABLE IF NOT EXISTS blob_retained (
    hash   TEXT PRIMARY KEY,
    reason TEXT NOT NULL,
    at     INTEGER NOT NULL
) WITHOUT ROWID;

-- The blobs each kept full event lists by hash (ADR 0029, 0.4.3): which
-- events list a blob, found without reading every event. One row per
-- (hash, event); a tombstone or an opaque event has none. Filled on
-- insert, emptied on drop and delete, and backfilled at open from
-- `event_blobs_filled` on: no schema bump, so 0.4.2 still opens the store,
-- and what an older release inserted meanwhile is filled at the next open.
CREATE TABLE IF NOT EXISTS event_blobs (
    hash TEXT NOT NULL,
    seq  INTEGER NOT NULL REFERENCES events (seq),
    PRIMARY KEY (hash, seq)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS event_blobs_seq ON event_blobs (seq);
-- The highest `seq` the backfill of `event_blobs` covered (one row).
CREATE TABLE IF NOT EXISTS event_blobs_filled (
    upto INTEGER NOT NULL
);

-- Blobs kept as replicas when the body that listed them was evicted (ADR
-- 0029 §5): a later retraction, erasure, label or `wats drop` of that
-- post releases them, unless a kept event lists them. `k`: the replicas
-- the blob wanted when evicted (5 for knowledge; the post is a tombstone
-- since, so knowledge cannot be computed again). `due`: when repair looks
-- at it next (Unix seconds).
CREATE TABLE IF NOT EXISTS evicted_replicas (
    id   TEXT NOT NULL,
    hash TEXT NOT NULL,
    k    INTEGER NOT NULL DEFAULT 0,
    due  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (id, hash)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS evicted_replicas_due ON evicted_replicas (due);

-- Small values the store keeps about itself (0.4.3): what the opaque
-- quota counted and up to which `seq`, whether rotation ever dropped
-- history.
CREATE TABLE IF NOT EXISTS store_meta (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;

-- A key's rows of a network, oldest first (rotation's per-key share,
-- 0.4.3).
CREATE INDEX IF NOT EXISTS events_net_author_ts ON events (net, author, ts, id);

-- The rotations run, newest last (the last 100 are kept).
CREATE TABLE IF NOT EXISTS rotation_log (
    at      INTEGER NOT NULL,
    bodies  INTEGER NOT NULL,
    trimmed INTEGER NOT NULL,
    rows    INTEGER NOT NULL,
    before  INTEGER NOT NULL,
    after   INTEGER NOT NULL
);

-- Semantic search (ADR 0053): each post's embedding, computed here and
-- never shared, with the model it comes from (its file's SHA-256). No
-- schema bump: an older wats ignores the table, and the triggers, stored
-- in the database, still drop a vector with its body whichever wats drops
-- it (rotation, drop, retraction, eviction).
-- One vector per post and model (0.5.2: while a new model builds its
-- index, the previous one's still serves).
CREATE TABLE IF NOT EXISTS embeddings (
    seq   INTEGER NOT NULL,
    model TEXT NOT NULL,
    vec   BLOB NOT NULL,
    PRIMARY KEY (seq, model)
) WITHOUT ROWID;
CREATE TRIGGER IF NOT EXISTS embeddings_event_delete AFTER DELETE ON events BEGIN
    DELETE FROM embeddings WHERE seq = old.seq;
END;
CREATE TRIGGER IF NOT EXISTS embeddings_body_change
    AFTER UPDATE OF json, text, title, opaque ON events BEGIN
    DELETE FROM embeddings WHERE seq = old.seq;
END;
-- Where the backfill and the scan of new posts stand.
CREATE TABLE IF NOT EXISTS semantic_state (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;
-- The find-before-ask flow, counted per day (ADR 0053 §8): local, never
-- sent.
CREATE TABLE IF NOT EXISTS find_counters (
    day   INTEGER NOT NULL,
    name  TEXT NOT NULL,
    value INTEGER NOT NULL,
    PRIMARY KEY (day, name)
) WITHOUT ROWID;
"#;

/// Events whose blobs one backfill transaction of `event_blobs` indexes.
const EVENT_BLOBS_BATCH: i64 = 5_000;

/// Index the blobs of the full event at `seq` (`json`) in `event_blobs`.
fn index_blobs(tx: &Connection, seq: i64, json: &str) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO event_blobs (hash, seq)
         SELECT json_extract(b.value, '$.hash'), ?1 FROM json_each(?2, '$.blobs') b
         WHERE json_extract(b.value, '$.hash') IS NOT NULL",
        params![seq, json],
    )?;
    Ok(())
}

/// Fill `event_blobs` for the full events above its backfill mark, in
/// transactions of [`EVENT_BLOBS_BATCH`] events.
fn backfill_event_blobs(conn: &Connection) -> Result<()> {
    let mut upto: i64 = conn
        .query_row("SELECT upto FROM event_blobs_filled", [], |r| r.get(0))
        .optional()?
        .unwrap_or(0);
    let last: i64 = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
    if conn
        .query_row("SELECT 1 FROM event_blobs_filled", [], |_| Ok(()))
        .optional()?
        .is_none()
    {
        conn.execute("INSERT INTO event_blobs_filled (upto) VALUES (?)", [upto])?;
    }
    while upto < last {
        let next = upto.saturating_add(EVENT_BLOBS_BATCH).min(last);
        conn.execute_batch("BEGIN")?;
        let done = (|| -> Result<()> {
            conn.execute(
                "INSERT OR IGNORE INTO event_blobs (hash, seq)
                 SELECT json_extract(b.value, '$.hash'), e.seq FROM events e,
                     json_each(e.json, '$.blobs') b
                 WHERE e.seq > ?1 AND e.seq <= ?2 AND e.opaque = 0
                     AND json_extract(b.value, '$.hash') IS NOT NULL",
                params![upto, next],
            )?;
            conn.execute("DELETE FROM event_blobs_filled", [])?;
            conn.execute("INSERT INTO event_blobs_filled (upto) VALUES (?)", [next])?;
            Ok(())
        })();
        match done {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(e) => {
                conn.execute_batch("ROLLBACK").ok();
                return Err(e);
            }
        }
        upto = next;
    }
    Ok(())
}

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
    if changed > 0 {
        // An opaque row lists no blob: the backfill mark moves past it.
        // `last_insert_rowid` stays the insert's.
        advance_filled(conn, conn.last_insert_rowid())?;
    }
    Ok(changed)
}

/// Whether a kept full event lists blob `hash` (`event_blobs`; an entry
/// whose row is gone or no longer full counts for nothing).
fn blob_referenced(conn: &Connection, hash: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM event_blobs b JOIN events e ON e.seq = b.seq
             WHERE b.hash = ?1 AND e.opaque = 0)",
        [hash],
        |r| r.get(0),
    )?)
}

/// Whether a replica kept through the eviction of a post other than
/// `id` holds blob `hash` (ADR 0029 §4).
fn blob_retained_elsewhere(conn: &Connection, hash: &str, id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM evicted_replicas WHERE hash = ?1 AND id <> ?2)",
        params![hash, id],
        |r| r.get(0),
    )?)
}

/// Advance the backfill mark of `event_blobs` past `seq` when it is the
/// next one (N7): a restart then re-scans only what an older release
/// inserted meanwhile.
fn advance_filled(conn: &Connection, seq: i64) -> Result<()> {
    conn.execute(
        "UPDATE event_blobs_filled SET upto = ?1 WHERE upto = ?1 - 1",
        [seq],
    )?;
    Ok(())
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

/// SQL condition: row `alias` is the tombstone of an event of a version
/// this node understands (ADR 0020): its header columns are those of an
/// understood event, its body is gone. Never NULL (`IS`, not `=`), so
/// that `NOT` of it holds for every other row.
fn tombstone_sql(alias: &str) -> String {
    let (lo, hi) = crate::wire::UNDERSTOOD_VERSIONS;
    format!(
        "({alias}.opaque = 1 AND {alias}.v BETWEEN {lo} AND {hi}
          AND json_type({alias}.json, '$.body_hash') IS 'text')"
    )
}

/// SQL condition: row `alias` may give a signal (ADR 0025 PR B): a full
/// understood event, or the tombstone of one that no drop record of a
/// reason other than eviction covers (retracted, erased, labelled or the
/// human's own drop: no signal).
fn source_sql(alias: &str) -> String {
    format!(
        "({alias}.opaque = 0 OR ({} AND NOT EXISTS (SELECT 1 FROM dropped d
             WHERE d.id = {alias}.id AND d.reason <> 'evicted')))",
        tombstone_sql(alias)
    )
}

/// SQL condition: row `alias` counts in the opaque quota (B4): an opaque
/// row that is not a tombstone of an understood version (ADR 0025 §7).
fn quota_sql(alias: &str) -> String {
    format!("({alias}.opaque = 1 AND NOT {})", tombstone_sql(alias))
}

/// Whether an opaque event counts in the opaque quota: not a tombstone of
/// a version this node understands.
pub fn counts_as_opaque(raw: &RawEvent) -> bool {
    let (lo, hi) = crate::wire::UNDERSTOOD_VERSIONS;
    !(raw.tombstoned && (lo..=hi).contains(&raw.v))
}

/// Remove an opaque row and its (empty) full-text entry.
fn delete_opaque_row(tx: &rusqlite::Transaction<'_>, seq: i64) -> Result<()> {
    // Opaque rows are indexed with empty title, text and tags.
    tx.execute(
        "INSERT INTO events_fts (events_fts, rowid, title, text, tags)
         VALUES ('delete', ?, '', '', '')",
        [seq],
    )?;
    tx.execute("DELETE FROM event_refs WHERE seq = ?", [seq])?;
    tx.execute("DELETE FROM event_blobs WHERE seq = ?", [seq])?;
    tx.execute("DELETE FROM events WHERE seq = ?", [seq])?;
    Ok(())
}

/// [`Store::drop_body_keeping`] inside a transaction (ADR 0029 §4, §5):
/// a stronger reason recorded meanwhile wins over an eviction, which then
/// keeps no replica and writes no record; the replicas `keep` are recorded
/// with the post; a drop for any other reason releases those an earlier
/// eviction kept. A tombstone is not in the opaque quota (ADR 0025 §7), so
/// the quota does not change.
fn drop_body_tx(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    reason: DropReason,
    trigger: Option<&str>,
    detail: Option<&str>,
    now: u64,
    keep: &[(String, usize)],
) -> Result<DropOutcome> {
    let recorded: Option<String> = tx
        .query_row("SELECT reason FROM dropped WHERE id = ?", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let outranked = reason == DropReason::Evicted
        && recorded
            .as_deref()
            .is_some_and(|r| r != DropReason::Evicted.as_str());
    let keep: &[(String, usize)] = if outranked { &[] } else { keep };
    let inner = drop_body_inner(tx, id, reason, trigger, detail, now, !outranked)?;
    let listed = referenced_among(tx, &inner.listed)?;
    let mut freed: Vec<String> = Vec::new();
    for hash in inner.listed {
        if !keep.iter().any(|(h, _)| *h == hash)
            && !listed.contains(&hash)
            && !blob_retained_elsewhere(tx, &hash, id)?
            && !freed.contains(&hash)
        {
            freed.push(hash);
        }
    }
    // Replicas kept when this body was evicted (ADR 0029 §5): any other
    // reason releases them, unless a kept event lists them.
    if reason != DropReason::Evicted {
        let kept: Vec<String> = {
            let mut stmt = tx.prepare("SELECT hash FROM evicted_replicas WHERE id = ?")?;
            let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        tx.execute("DELETE FROM evicted_replicas WHERE id = ?", [id])?;
        for hash in kept {
            if !freed.contains(&hash)
                && !blob_referenced(tx, &hash)?
                && !blob_retained_elsewhere(tx, &hash, id)?
            {
                freed.push(hash);
            }
        }
    }
    for (hash, k) in keep {
        tx.execute(
            "INSERT INTO evicted_replicas (id, hash, k, due) VALUES (?, ?, ?, 0)
             ON CONFLICT (id, hash) DO UPDATE SET k = MAX(k, excluded.k)",
            params![id, hash, *k as i64],
        )?;
    }
    // An eviction drops history: this node no longer says it is full
    // (`history_full`, ADR 0025 PR B), whatever path evicted.
    if reason == DropReason::Evicted && inner.body_dropped && !outranked {
        set_meta(tx, "history_lossy", 1)?;
    }
    Ok(DropOutcome {
        held: inner.held,
        body_dropped: inner.body_dropped,
        freed_blobs: freed,
    })
}

/// What [`drop_body_inner`] did: whether the row was held and its body
/// dropped, and every blob the dropped body listed.
struct DropInner {
    held: bool,
    body_dropped: bool,
    listed: Vec<String>,
}

/// Drop a body in place, its tags, full-text entry and blob index with it.
/// For an eviction, its `refs` stay indexed: they are header fields, kept
/// by the tombstone, and the signals it gives keep counting (ADR 0025 §7).
/// For any other reason (a retraction, an erasure, a label, the human's
/// own drop) they go, on the tombstone of an earlier eviction too: what
/// its author withdrew, or an operator erased, gives no signal any more.
/// With `record`, the drop record is written or updated.
fn drop_body_inner(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    reason: DropReason,
    trigger: Option<&str>,
    detail: Option<&str>,
    now: u64,
    record: bool,
) -> Result<DropInner> {
    let row: Option<(i64, bool, String, String, String, String)> = tx
        .query_row(
            "SELECT seq, opaque, title, text, tags, json FROM events WHERE id = ?",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let mut outcome = DropInner {
        held: row.is_some(),
        body_dropped: false,
        listed: Vec::new(),
    };
    if let Some((seq, false, title, text, tags, json)) = row {
        let full: Value = serde_json::from_str(&json).context("corrupt stored event")?;
        let tombstone = crate::wire::tombstone(&full)?;
        let tombstone = serde_json::to_string(&tombstone.wire)?;
        // FTS5 removes an entry given the exact values it indexed; an
        // opaque row keeps an empty entry, as `insert_raw` gives it.
        tx.execute(
            "INSERT INTO events_fts (events_fts, rowid, title, text, tags)
             VALUES ('delete', ?, ?, ?, ?)",
            params![seq, title, text, tags],
        )?;
        tx.execute(
            "INSERT INTO events_fts (rowid, title, text, tags) VALUES (?, '', '', '')",
            [seq],
        )?;
        tx.execute("DELETE FROM event_tags WHERE seq = ?", [seq])?;
        tx.execute("DELETE FROM event_blobs WHERE seq = ?", [seq])?;
        // Which of the human's clients posted it goes with the body.
        tx.execute("DELETE FROM ask_notify WHERE ask = ?", [id])?;
        tx.execute(
            "UPDATE events SET json = ?, opaque = 1, title = '', text = '', tags = '' WHERE seq = ?",
            params![tombstone, seq],
        )?;
        outcome.body_dropped = true;
        outcome.listed = blob_hashes(&full);
    }
    if reason != DropReason::Evicted {
        tx.execute(
            "DELETE FROM event_refs WHERE seq IN (SELECT seq FROM events WHERE id = ?)",
            [id],
        )?;
    }
    if record {
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
    }
    Ok(outcome)
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

/// The post a [`SignalRow`] points to, as [`Store::net_signal_rows`]
/// returns it.
#[derive(Debug, Clone)]
pub struct TargetHead {
    pub author: String,
    pub kind: String,
    pub thread: Option<String>,
}

/// An event as [`Store::author_events`] returns it: who signed it, when,
/// its size, and the proof of work it carries.
#[derive(Debug, Clone)]
pub struct AuthorEvent {
    pub author: String,
    pub ts: u64,
    pub received_at: u64,
    /// Bytes of its stored JSON.
    pub size: usize,
    /// The leading zero bits of its id: its proof of work (`b3`, the
    /// algorithm of every stored event).
    pub pow_bits: u32,
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

/// One child of a level of the tag tree (`browse`, ADR 0024).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagDir {
    /// The full tag, e.g. `lang:rust`.
    pub tag: String,
    /// Its last segment, e.g. `rust`.
    pub head: String,
    /// Threads where a post carries it or a tag below it.
    pub threads: u64,
    /// Posts carrying it or a tag below it.
    pub posts: u64,
    /// Tags directly below it.
    pub children: u64,
    pub last_ts: u64,
}

/// Where a page of [`Store::tag_dirs`] ends: its last child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirKey {
    pub threads: u64,
    pub last: u64,
    pub head: String,
}

/// The tags `tag` covers below itself, as a range: `[tag:, tag;)`, since
/// `;` follows `:` (indexed, unlike a `LIKE`).
fn tag_range(tag: &str) -> (String, String) {
    (format!("{tag}:"), format!("{tag};"))
}

/// SQL condition on a post `tp` of a thread: it is in the network of the
/// thread's root (B9), looked up by the root's id.
const SAME_NET: &str =
    "tp.net = (SELECT r0.net FROM events r0 WHERE r0.id = coalesce(tp.thread, tp.id))";

/// Which threads `browse` maps (ADR 0024): each facet narrows the thread
/// roots, by the root itself (`net`, `kind`) or by its posts (the others).
/// Unset facets select every thread.
#[derive(Debug, Clone, Default)]
pub struct Facets {
    pub net: Option<String>,
    /// The root's kind, as stored (e.g. `ask`).
    pub kind: Option<String>,
    /// A post of the thread carries exactly this tag.
    pub tag: Option<String>,
    /// A post of the thread carries this tag or one below it (`lang`
    /// covers `lang` and `lang:rust`). Empty: any.
    pub dir: Option<String>,
    /// A post of the thread is by this author (hex id)…
    pub author: Option<String>,
    /// … at or after `since`…
    pub since: Option<u64>,
    /// … and before `until` (Unix seconds).
    pub until: Option<u64>,
    /// Only what this node held up to this `seq`: the snapshot a cursor
    /// pages through, so that later arrivals neither repeat nor hide
    /// anything. `None`: everything.
    pub snap: Option<i64>,
}

impl Facets {
    fn snap(&self) -> i64 {
        self.snap.unwrap_or(i64::MAX)
    }

    /// The SQL condition on a row `e` that it is a thread root these
    /// facets select, its parameters appended to `args` in order.
    fn roots_sql(&self, args: &mut Vec<SqlValue>) -> String {
        let snap = self.snap();
        let mut conds = vec![
            "e.thread IS NULL".to_string(),
            "e.opaque = 0".to_string(),
            "e.seq <= ?".to_string(),
        ];
        args.push(snap.into());
        if let Some(net) = &self.net {
            conds.push("e.net = ?".into());
            args.push(net.clone().into());
        }
        if let Some(kind) = &self.kind {
            conds.push("e.kind = ?".into());
            args.push(kind.clone().into());
        }
        // A post of the thread, in the root's network (B9), carries a tag.
        let tagged = |cond: &str| {
            format!(
                "e.id IN (SELECT coalesce(tp.thread, tp.id)
                     FROM event_tags tt JOIN events tp ON tp.seq = tt.seq
                     WHERE {cond} AND tp.opaque = 0 AND tp.seq <= ? AND {SAME_NET})"
            )
        };
        if let Some(tag) = &self.tag {
            conds.push(tagged("tt.tag = ?"));
            args.push(tag.clone().into());
            args.push(snap.into());
        }
        if let Some(dir) = self.dir.as_deref().map(|d| d.trim_end_matches(':'))
            && !dir.is_empty()
        {
            let (lo, hi) = tag_range(dir);
            conds.push(tagged("(tt.tag = ? OR (tt.tag >= ? AND tt.tag < ?))"));
            args.extend([dir.to_string().into(), lo.into(), hi.into(), snap.into()]);
        }
        if self.author.is_some() || self.since.is_some() || self.until.is_some() {
            let mut by = vec!["ap.opaque = 0".to_string(), "ap.seq <= ?".to_string()];
            args.push(snap.into());
            if let Some(author) = &self.author {
                by.push("ap.author = ?".into());
                args.push(author.clone().into());
            }
            if let Some(since) = self.since {
                by.push("ap.ts >= ?".into());
                args.push((since.min(i64::MAX as u64) as i64).into());
            }
            if let Some(until) = self.until {
                by.push("ap.ts < ?".into());
                args.push((until.min(i64::MAX as u64) as i64).into());
            }
            conds.push(format!(
                "e.id IN (SELECT coalesce(ap.thread, ap.id) FROM events ap WHERE {} AND {})",
                by.join(" AND "),
                SAME_NET.replace("tp.", "ap.")
            ));
        }
        conds.join(" AND ")
    }
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
    /// Leave out history: events this node stored more than this many
    /// seconds after their `ts` (`wait`, ADR 0025 §4). Not checked by
    /// [`Filter::matches`], which sees no arrival time.
    pub arrived_within: Option<u64>,
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
        if let Some(within) = self.arrived_within {
            conds.push("e.received_at <= e.ts + ?".into());
            args.push(sql_ts(within).into());
        }
    }
}

pub struct Store {
    conn: Mutex<Connection>,
    new_events: broadcast::Sender<Stored>,
    /// Bytes of JSON held by opaque rows (B4 quota).
    opaque_bytes: AtomicU64,
    /// Whether the store has `key_first` (a read-only store an older
    /// release wrote may not), checked once at open.
    key_first: bool,
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
        if (1..7).contains(&version) {
            // v6 -> v7: titles are searched too (ADR 0019 §5.1). The
            // full-text table gets a column: it is made again by SCHEMA,
            // then filled from the events kept.
            let titled: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info('events') WHERE name = 'title')",
                [],
                |r| r.get(0),
            )?;
            if !titled {
                conn.execute_batch(
                    "ALTER TABLE events ADD COLUMN title TEXT NOT NULL DEFAULT '';",
                )?;
            }
            conn.execute_batch(
                "DROP TRIGGER IF EXISTS events_fts_insert;
                 DROP TABLE IF EXISTS events_fts;",
            )?;
        }
        // `evicted_replicas` from a build that had no `k` or `due` (never
        // released, but cheap to bring along).
        for column in ["k", "due"] {
            let has: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info('evicted_replicas') WHERE name = ?)",
                [column],
                |r| r.get(0),
            )?;
            let table: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'evicted_replicas')",
                [],
                |r| r.get(0),
            )?;
            if table && !has {
                conn.execute_batch(&format!(
                    "ALTER TABLE evicted_replicas ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0;"
                ))?;
            }
        }
        conn.execute_batch(SCHEMA)?;
        // 0.5.0 and 0.5.1 keyed vectors by `seq` alone: one per post. 0.5.2
        // keeps one per post and model, so a previous model's index serves
        // while a new one builds (ADR 0053 §6). Same table name, so an
        // older wats still reads it after a rollback.
        let keyed_by_model: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM pragma_table_info('embeddings')
                            WHERE name = 'model' AND pk > 0)",
            [],
            |r| r.get(0),
        )?;
        if !keyed_by_model {
            conn.execute_batch(&format!(
                "BEGIN;
                 DROP TRIGGER IF EXISTS embeddings_event_delete;
                 DROP TRIGGER IF EXISTS embeddings_body_change;
                 ALTER TABLE embeddings RENAME TO embeddings_by_seq;
                 CREATE TABLE embeddings (
                     seq   INTEGER NOT NULL,
                     model TEXT NOT NULL,
                     vec   BLOB NOT NULL,
                     PRIMARY KEY (seq, model)
                 ) WITHOUT ROWID;
                 INSERT INTO embeddings (seq, model, vec)
                     SELECT seq, model, vec FROM embeddings_by_seq;
                 DROP TABLE embeddings_by_seq;
                 {EMBEDDINGS_TRIGGERS}
                 COMMIT;"
            ))?;
        }
        if (1..4).contains(&version) {
            // v2 -> v3: the event format was hardened before the freeze (ADR
            // 0010): events stored earlier hash differently and can no longer
            // be verified. v3 -> v4: `pow` became algorithm-tagged
            // ("b3:<nonce>"), and the bare decimal nonce is invalid. The store
            // is a cache of the swarm: drop them, the node syncs again.
            conn.execute_batch(
                "DELETE FROM event_tags; DELETE FROM event_refs; DELETE FROM event_blobs;
                 DELETE FROM events;
                 DELETE FROM sync_state; INSERT INTO events_fts (events_fts) VALUES ('rebuild');",
            )?;
        }
        // v4 -> v5: the drop records table (ADR 0020), created by SCHEMA
        // above. Events are kept. The history ranges and the peers' first
        // sightings (ADR 0025) are created by SCHEMA too, on any version:
        // no schema bump, so 0.4.0 still opens the store, and the
        // watermark keeps its meaning, so nothing is synced again.
        if (4..6).contains(&version) {
            // v5 -> v6: the citations table (ADR 0019), filled from the
            // events kept.
            conn.execute_batch(
                "INSERT OR IGNORE INTO event_refs (ref, seq)
                 SELECT r.value, e.seq FROM events e, json_each(e.json, '$.refs') r
                 WHERE e.opaque = 0",
            )?;
        }
        if (1..7).contains(&version) {
            conn.execute_batch(
                "UPDATE events SET title = COALESCE(json_extract(json, '$.title'), '')
                     WHERE opaque = 0;
                 INSERT INTO events_fts (events_fts) VALUES ('rebuild');",
            )?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        backfill_event_blobs(&conn)?;
        // Tombstones of understood versions are left out (ADR 0025 §7);
        // the count is cached, so an open reads only the rows added since.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let opaque_bytes = opaque_bytes_at_open(&conn, now)?;
        let (new_events, _) = broadcast::channel(1024);
        Ok(Self {
            conn: Mutex::new(conn),
            new_events,
            opaque_bytes: AtomicU64::new(opaque_bytes),
            key_first: true,
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
    /// contradict a parent we already have. A body evicted here is refused
    /// (its tombstone kept), as long as its record lasts: it comes back
    /// only through [`Store::insert_with`] on a `read`.
    pub fn insert(&self, event: &Event, received_at: u64) -> Result<Inserted> {
        self.insert_with(event, received_at, false)
    }

    /// [`Store::insert`]; `restore`: a body evicted here is taken back
    /// (ADR 0020 decision 4: fetched for a `read`, on demand). Otherwise it
    /// stays out, so that neither gossip, nor a sync page, nor the fill
    /// brings back what rotation evicted, to be evicted again (ADR 0025
    /// PR B review). The record expires after 30 days.
    pub fn insert_with(&self, event: &Event, received_at: u64, restore: bool) -> Result<Inserted> {
        let stored = {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            let existing: Option<(i64, bool, i64)> = tx
                .query_row(
                    &format!(
                        "SELECT e.seq, e.opaque,
                             CASE WHEN {} THEN LENGTH(CAST(e.json AS BLOB)) ELSE 0 END
                         FROM events e WHERE e.id = ?",
                        quota_sql("e")
                    ),
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
                Some(reason) if reason == DropReason::Evicted.as_str() && restore => {
                    tx.execute("DELETE FROM dropped WHERE id = ?", [&event.id])?;
                    false
                }
                Some(reason) if reason == DropReason::Evicted.as_str() => true,
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
                // A tombstone: not in the opaque quota (ADR 0025 §7).
                if existing.is_none() {
                    let tombstone = crate::wire::tombstone(&serde_json::to_value(event)?)?;
                    let json = serde_json::to_string(&tombstone.wire)?;
                    insert_opaque_row(&tx, &tombstone, &json, received_at)?;
                }
                tx.commit()?;
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
                // new seq, so peers syncing from us receive it again. `len`
                // is what it counted in the opaque quota.
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
                "INSERT INTO events (id, author, ts, net, thread, parent, kind, title, text, tags, json, received_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    event.id,
                    event.author,
                    event.ts as i64,
                    event.net,
                    event.thread,
                    event.parent,
                    event.kind.as_str(),
                    event.title.as_deref().unwrap_or_default(),
                    event.text,
                    event.tags[..event.tags.len().min(INDEXED_TAGS)].join(" "),
                    String::from_utf8(event.to_json())?,
                    received_at as i64,
                ],
            )?;
            let seq = tx.last_insert_rowid();
            index_blobs(&tx, seq, &String::from_utf8(event.to_json())?)?;
            advance_filled(&tx, seq)?;
            // A body taken back (an evicted one) lists its blobs again:
            // the eviction's record of kept replicas is moot.
            tx.execute("DELETE FROM evicted_replicas WHERE id = ?", [&event.id])?;
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
            let quota = self
                .opaque_bytes
                .load(Ordering::Relaxed)
                .saturating_sub(freed);
            if freed > 0 {
                persist_quota(&tx, quota)?;
            }
            tx.commit()?;
            self.opaque_bytes.store(quota, Ordering::Relaxed);
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let json = serde_json::to_string(&raw.wire)?;
        let len = json.len() as u64;
        let changed = insert_opaque_row(&tx, raw, &json, received_at)?;
        if changed == 0 {
            return Ok(Inserted::Duplicate);
        }
        let seq = tx.last_insert_rowid();
        let counted = counts_as_opaque(raw);
        let quota = self
            .opaque_bytes
            .load(Ordering::Relaxed)
            .saturating_add(len);
        if counted {
            persist_quota(&tx, quota)?;
        } else {
            // A tombstone of an understood event: its `refs` are header
            // fields, and its signals still count (ADR 0025 §7).
            for cited in &raw.refs {
                tx.execute(
                    "INSERT OR IGNORE INTO event_refs (ref, seq) VALUES (?, ?)",
                    params![cited, seq],
                )?;
            }
        }
        tx.commit()?;
        if counted {
            self.opaque_bytes.store(quota, Ordering::Relaxed);
        }
        Ok(Inserted::New(seq))
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
        self.drop_body_keeping(id, reason, trigger, detail, now, &[])
    }

    /// [`Store::drop_body`], recording in the same transaction that the
    /// replicas `keep` (hashes) stay with the post (ADR 0029 §4, N8): they
    /// are not in `freed_blobs`, and a later drop of the post for another
    /// reason releases them.
    pub fn drop_body_keeping(
        &self,
        id: &str,
        reason: DropReason,
        trigger: Option<&str>,
        detail: Option<&str>,
        now: u64,
        keep: &[(String, usize)],
    ) -> Result<DropOutcome> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let outcome = drop_body_tx(&tx, id, reason, trigger, detail, now, keep)?;
        tx.commit()?;
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

    /// Every blob a kept full event lists, with the ids of at most
    /// `per_blob` such events, oldest first: what replica repair walks (ADR
    /// 0029), from `event_blobs`.
    pub fn blob_listings(&self, per_blob: usize) -> Result<Vec<(String, Vec<String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT b.hash, e.id FROM event_blobs b JOIN events e ON e.seq = b.seq
                 WHERE e.opaque = 0 ORDER BY b.hash, b.seq",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for row in rows {
            let (hash, id) = row?;
            match out.last_mut() {
                Some((last, ids)) if *last == hash => {
                    if ids.len() < per_blob {
                        ids.push(id);
                    }
                }
                _ => out.push((hash, vec![id])),
            }
        }
        Ok(out)
    }

    /// The kept full events that list blob `hash`, at most `limit`, oldest
    /// first.
    pub fn events_listing_blob(&self, hash: &str, limit: usize) -> Result<Vec<Stored>> {
        self.query(
            "SELECT e.seq, e.json FROM event_blobs b JOIN events e ON e.seq = b.seq
                 WHERE b.hash = ?1 AND e.opaque = 0 ORDER BY e.seq LIMIT ?2",
            vec![hash.to_string().into(), (limit as i64).into()],
        )
    }

    /// The blobs a drop of event `id`'s body would free: those it lists
    /// that no other kept full event lists ([`Store::drop_body`] frees
    /// the same). Empty when `id` is not held in full.
    pub fn blobs_freed_by(&self, id: &str) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT b.hash FROM events e JOIN event_blobs b ON b.seq = e.seq
                 WHERE e.id = ?1 AND e.opaque = 0
                     AND NOT EXISTS (SELECT 1 FROM event_blobs o JOIN events f ON f.seq = o.seq
                         WHERE o.hash = b.hash AND f.opaque = 0 AND f.id <> ?1)",
        )?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Whether a kept full event lists blob `hash`.
    pub fn blob_listed(&self, hash: &str) -> Result<bool> {
        blob_referenced(&self.conn(), hash)
    }

    /// Whether this node keeps blob `hash` for a post: a kept full event
    /// lists it, or a replica kept through an eviction holds it (N13).
    pub fn blob_kept(&self, hash: &str) -> Result<bool> {
        let conn = self.conn();
        Ok(blob_referenced(&conn, hash)? || blob_retained_elsewhere(&conn, hash, "")?)
    }

    /// Replicas kept through an eviction that are due at `now`, `(id, hash,
    /// net, k)`, the most overdue first, at most `limit` (N4): each review
    /// moves a row's `due` on ([`Store::set_evicted_due`]), so no row pins
    /// the head.
    pub fn evicted_replica_rows(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<(String, String, String, usize)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT r.id, r.hash, e.net, r.k FROM evicted_replicas r JOIN events e ON e.id = r.id
                 WHERE r.due <= ?1 ORDER BY r.due LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![now.min(i64::MAX as u64) as i64, limit as i64],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, i64>(3)?.max(0) as usize,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// When repair looks at the replica `hash` kept for post `id` next.
    pub fn set_evicted_due(&self, id: &str, hash: &str, due: u64) -> Result<()> {
        self.conn().execute(
            "UPDATE evicted_replicas SET due = ? WHERE id = ? AND hash = ?",
            params![due.min(i64::MAX as u64) as i64, id, hash],
        )?;
        Ok(())
    }

    /// The replicas kept for post `id`, with the k each wanted and when it
    /// is due.
    pub fn evicted_replica_details(&self, id: &str) -> Result<Vec<(String, usize, u64)>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare_cached("SELECT hash, k, due FROM evicted_replicas WHERE id = ?")?;
        let rows = stmt.query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?.max(0) as usize,
                r.get::<_, i64>(2)?.max(0) as u64,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Forget the replica `hash` kept for post `id`; true when nothing keeps
    /// the blob any more (no kept event lists it, no other eviction holds
    /// it): the node then untags it.
    pub fn release_evicted_replica(&self, id: &str, hash: &str) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM evicted_replicas WHERE id = ? AND hash = ?",
            params![id, hash],
        )?;
        let free = !blob_referenced(&tx, hash)? && !blob_retained_elsewhere(&tx, hash, id)?;
        tx.commit()?;
        Ok(free)
    }

    /// Record that the replicas `hashes` were kept when the body of `id`
    /// was evicted (ADR 0029 §5).
    pub fn record_evicted_replicas(&self, id: &str, hashes: &[(String, usize)]) -> Result<()> {
        let conn = self.conn();
        for (hash, k) in hashes {
            conn.execute(
                "INSERT INTO evicted_replicas (id, hash, k, due) VALUES (?, ?, ?, 0)
                 ON CONFLICT (id, hash) DO UPDATE SET k = MAX(k, excluded.k)",
                params![id, hash, *k as i64],
            )?;
        }
        Ok(())
    }

    /// The replicas kept when the body of `id` was evicted.
    pub fn evicted_replicas(&self, id: &str) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT hash FROM evicted_replicas WHERE id = ?")?;
        let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Bytes of JSON held by opaque events: those of versions this node
    /// does not understand, and v2 submissions. Tombstones of understood
    /// versions are not counted (ADR 0025 §7): a node that evicted many
    /// bodies still takes events of newer versions.
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
                &format!(
                    "SELECT e.seq, CASE WHEN {} THEN LENGTH(CAST(e.json AS BLOB)) ELSE 0 END
                     FROM events e WHERE e.id = ? AND e.opaque = 1",
                    quota_sql("e")
                ),
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((seq, len)) = row {
            delete_opaque_row(&tx, seq)?;
            let quota = self
                .opaque_bytes
                .load(Ordering::Relaxed)
                .saturating_sub(len.max(0) as u64);
            if len > 0 {
                persist_quota(&tx, quota)?;
            }
            tx.commit()?;
            self.opaque_bytes.store(quota, Ordering::Relaxed);
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
        let key_first = has_table(&conn, "key_first")?;
        Ok(Self {
            conn: Mutex::new(conn),
            new_events,
            opaque_bytes: AtomicU64::new(0),
            key_first,
        })
    }

    /// Drop the stored v2 `submit` events (opaque rows, tombstones
    /// included) whose `ts` is before `before`: the retention of
    /// submissions (ADR 0015 §5). Returns how many were dropped.
    pub fn delete_submits_before(&self, before: u64) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let rows: Vec<(i64, i64)> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT e.seq, CASE WHEN {} THEN LENGTH(CAST(e.json AS BLOB)) ELSE 0 END
                 FROM events e
                 WHERE e.opaque = 1 AND e.v = 2 AND e.kind = 'submit' AND e.ts < ?",
                quota_sql("e")
            ))?;
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
        let quota = self
            .opaque_bytes
            .load(Ordering::Relaxed)
            .saturating_sub(freed);
        if freed > 0 {
            persist_quota(&tx, quota)?;
        }
        tx.commit()?;
        self.opaque_bytes.store(quota, Ordering::Relaxed);
        Ok(rows.len())
    }

    /// Any stored event, understood or not, as raw wire JSON with its version.
    /// When this node first stored an event of `author` (unix seconds), if
    /// ever: the age behind the new-author proof of work (ADR 0010). What
    /// rotation deleted still counts (`key_first`, ADR 0025 §7).
    pub fn author_first_seen(&self, author: &str) -> Result<Option<u64>> {
        let conn = self.conn();
        let held: Option<i64> = conn.query_row(
            "SELECT MIN(received_at) FROM events WHERE author = ?",
            [author],
            |r| r.get(0),
        )?;
        let deleted: Option<i64> = if self.key_first {
            conn.query_row(
                "SELECT MIN(first_seen) FROM key_first WHERE author = ?",
                [author],
                |r| r.get(0),
            )?
        } else {
            None
        };
        let first = match (held, deleted) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
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

    /// The full id of the one post whose id starts with `prefix`: a full id
    /// as is, or at least 6 of its first hex characters (as `browse`
    /// shows them), like git's short hashes.
    pub fn resolve_id(&self, prefix: &str) -> Result<String> {
        let p = prefix.trim().to_ascii_lowercase();
        anyhow::ensure!(
            p.len() >= 6 && p.len() <= 64 && p.chars().all(|c| c.is_ascii_hexdigit()),
            "{prefix:?}: a post id, or at least its first 6 hex characters"
        );
        if p.len() == 64 {
            return Ok(p);
        }
        // Ids are lowercase hex: `g` sorts after every one of them.
        let conn = self.conn();
        let mut stmt =
            conn.prepare_cached("SELECT id FROM events WHERE id >= ?1 AND id < ?2 LIMIT 2")?;
        let found: Vec<String> = stmt
            .query_map([p.clone(), format!("{p}g")], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        match found.as_slice() {
            [] => anyhow::bail!("no post {p} on this node"),
            [id] => Ok(id.clone()),
            _ => anyhow::bail!("{p} starts several post ids: give more of it"),
        }
    }

    /// The understood event stored at `seq`.
    pub fn get_seq(&self, seq: i64) -> Result<Option<Stored>> {
        Ok(self
            .query(
                "SELECT seq, json FROM events WHERE seq = ? AND opaque = 0",
                vec![seq.into()],
            )?
            .pop())
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

    /// The tags of the understood events of the threads `facets` selects,
    /// busiest first (`browse`): how many threads and posts carry each,
    /// and their latest `ts`. `prefix` keeps the tags that start with it
    /// (e.g. `cap:`).
    pub fn tag_counts(
        &self,
        facets: &Facets,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TagCount>> {
        let mut args: Vec<SqlValue> = vec![facets.snap().into()];
        let mut conds = vec!["p.opaque = 0".to_string(), "p.seq <= ?".to_string()];
        if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
            conds.push("substr(t.tag, 1, ?) = ?".into());
            args.push((prefix.chars().count() as i64).into());
            args.push(prefix.to_string().into());
        }
        let roots = facets.roots_sql(&mut args);
        args.push((limit as i64).into());
        let sql = format!(
            "SELECT t.tag, count(DISTINCT coalesce(p.thread, p.id)), count(*), max(p.ts)
             FROM event_tags t JOIN events p ON p.seq = t.seq
                 JOIN events e ON e.id = coalesce(p.thread, p.id) AND e.net = p.net
             WHERE {} AND {roots}
             GROUP BY t.tag ORDER BY 2 DESC, 4 DESC, t.tag LIMIT ?",
            conds.join(" AND ")
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok(TagCount {
                tag: r.get(0)?,
                threads: r.get::<_, i64>(1)? as u64,
                posts: r.get::<_, i64>(2)? as u64,
                last_ts: r.get::<_, i64>(3)?.max(0) as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// One level of the tag tree (`browse`, ADR 0024): the tags directly
    /// under `level` (under `lang`, `lang:rust`, which also stands for
    /// `lang:rust:async`; under the empty level, the roots `lang`, `cap`,
    /// …), each with the threads `facets` selects (its `dir` aside) where
    /// a post carries it or a tag below it, those posts, their latest
    /// `ts`, and how many tags sit directly below it. Busiest first, after
    /// `after`, at most `limit`.
    pub fn tag_dirs(
        &self,
        facets: &Facets,
        level: &str,
        after: Option<&DirKey>,
        limit: usize,
    ) -> Result<Vec<TagDir>> {
        let level = level.trim_end_matches(':');
        let prefix = match level {
            "" => String::new(),
            level => format!("{level}:"),
        };
        let mut args: Vec<SqlValue> =
            vec![(prefix.chars().count() as i64).into(), facets.snap().into()];
        let mut conds = vec!["p.opaque = 0".to_string(), "p.seq <= ?".to_string()];
        if !level.is_empty() {
            let (lo, hi) = tag_range(level);
            conds.push("t.tag >= ? AND t.tag < ?".into());
            args.push(lo.into());
            args.push(hi.into());
        }
        let roots = Facets {
            dir: None,
            ..facets.clone()
        }
        .roots_sql(&mut args);
        let mut page = "";
        if let Some(key) = after {
            page = "WHERE threads < ? OR (threads = ? AND (last < ? OR (last = ? AND head > ?)))";
            let (threads, last) = (key.threads as i64, key.last.min(i64::MAX as u64) as i64);
            args.extend([threads.into(), threads.into(), last.into(), last.into()]);
            args.push(key.head.clone().into());
        }
        args.push((limit as i64).into());
        // `rest`: the tag after the level; `head`, its first segment (the
        // child); `sub`, what follows, whose first segment is a grandchild.
        let first = |s: &str| {
            format!(
                "CASE WHEN instr({s}, ':') > 0 THEN substr({s}, 1, instr({s}, ':') - 1) ELSE {s} END"
            )
        };
        let sql = format!(
            "WITH m AS (
                 SELECT substr(t.tag, ? + 1) AS rest, coalesce(p.thread, p.id) AS root,
                     p.seq AS seq, p.ts AS ts
                 FROM event_tags t JOIN events p ON p.seq = t.seq
                     JOIN events e ON e.id = coalesce(p.thread, p.id) AND e.net = p.net
                 WHERE {} AND {roots}
             ), h AS (
                 SELECT {} AS head,
                     CASE WHEN instr(rest, ':') > 0 THEN substr(rest, instr(rest, ':') + 1) END AS sub,
                     root, seq, ts
                 FROM m
             ), g AS (
                 SELECT head, count(DISTINCT root) AS threads, count(DISTINCT seq) AS posts,
                     max(ts) AS last, count(DISTINCT {}) AS children
                 FROM h WHERE head <> '' GROUP BY head
             )
             SELECT head, threads, posts, last, children FROM g {page}
             ORDER BY threads DESC, last DESC, head LIMIT ?",
            conds.join(" AND "),
            first("rest"),
            first("sub"),
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            let head: String = r.get(0)?;
            Ok(TagDir {
                tag: format!("{prefix}{head}"),
                head,
                threads: r.get::<_, i64>(1)? as u64,
                posts: r.get::<_, i64>(2)? as u64,
                last_ts: r.get::<_, i64>(3)?.max(0) as u64,
                children: r.get::<_, i64>(4)? as u64,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The thread roots `facets` selects, by latest activity (`browse`),
    /// with their reply and answer counts: after `after` (the previous
    /// page's last `(last, seq)`), then skipping `offset`, at most `limit`.
    /// Replies from another network than the root's are not counted (B9).
    pub fn thread_summaries(
        &self,
        facets: &Facets,
        after: Option<(u64, i64)>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ThreadSummary>> {
        let snap = facets.snap();
        // Each root's replies by the thread index: left to itself, the
        // planner may scan every answer (`events_kind`) or the whole
        // network (`events_net_ts`) per root.
        let replies = "events r INDEXED BY events_thread";
        let reply = "r.thread = e.id AND r.opaque = 0 AND r.net = e.net AND r.seq <= ?";
        // One snapshot bound per `reply` below.
        let mut args: Vec<SqlValue> = vec![snap.into(), snap.into(), snap.into()];
        let roots = facets.roots_sql(&mut args);
        let mut page = "";
        if let Some((last, seq)) = after {
            page = "WHERE last < ? OR (last = ? AND seq < ?)";
            let last = last.min(i64::MAX as u64) as i64;
            args.extend([last.into(), last.into(), seq.into()]);
        }
        args.push((limit as i64).into());
        args.push((offset as i64).into());
        let sql = format!(
            "SELECT seq, json, replies, answers, last FROM (
                 SELECT e.seq AS seq, e.json AS json,
                     (SELECT count(*) FROM {replies} WHERE {reply}) AS replies,
                     (SELECT count(*) FROM {replies} WHERE {reply} AND r.kind = 'answer'
                         AND r.author <> e.author) AS answers,
                     max(e.ts, coalesce((SELECT max(r.ts) FROM {replies} WHERE {reply}), 0)) AS last
                 FROM events e WHERE {roots}
             ) {page}
             ORDER BY last DESC, seq DESC LIMIT ? OFFSET ?"
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
                last_ts: last.max(0) as u64,
            });
        }
        Ok(out)
    }

    /// The posts that may be knowledge entries (`browse`, ADR 0024), in the
    /// threads `facets` selects (its `kind` aside): digests and answers
    /// that a post replies to or cites (an entry is confirmed, so it has
    /// one). Newest first, after `after` (the previous page's last
    /// `(ts, seq)`), at most `limit`.
    pub fn knowledge_scan(
        &self,
        facets: &Facets,
        after: Option<(u64, i64)>,
        limit: usize,
    ) -> Result<Vec<Stored>> {
        let mut args: Vec<SqlValue> = vec![
            facets.snap().into(),
            crate::knowledge::DIGEST_KIND.to_string().into(),
        ];
        let roots = Facets {
            kind: None,
            ..facets.clone()
        }
        .roots_sql(&mut args);
        let mut page = "";
        if let Some((ts, seq)) = after {
            page = "AND (c.ts < ? OR (c.ts = ? AND c.seq < ?))";
            let ts = ts.min(i64::MAX as u64) as i64;
            args.extend([ts.into(), ts.into(), seq.into()]);
        }
        args.push((limit as i64).into());
        let sql = format!(
            "SELECT c.seq, c.json FROM events c
                 JOIN events e ON e.id = coalesce(c.thread, c.id) AND e.net = c.net
             WHERE c.opaque = 0 AND c.seq <= ? AND c.kind IN ('answer', ?)
                 AND (EXISTS (SELECT 1 FROM events x WHERE x.parent = c.id AND x.opaque = 0)
                     OR EXISTS (SELECT 1 FROM event_refs x WHERE x.ref = c.id))
                 AND {roots}
                 {page}
             ORDER BY c.ts DESC, c.seq DESC LIMIT ?"
        );
        self.query(&sql, args)
    }

    /// How many understood posts outside thread `root` cite each of `ids`
    /// in their `refs` (`browse`): the ids cited at least once.
    pub fn cited_elsewhere(&self, ids: &[String], root: &str) -> Result<HashMap<String, u64>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = format!(
            "SELECT r.ref, count(*) FROM event_refs r JOIN events e ON e.seq = r.seq
             WHERE r.ref IN ({}) AND e.opaque = 0 AND coalesce(e.thread, e.id) <> ?
             GROUP BY r.ref",
            vec!["?"; ids.len()].join(",")
        );
        let mut args: Vec<SqlValue> = ids.iter().map(|id| id.clone().into()).collect();
        args.push(root.to_string().into());
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>()
            .map_err(Into::into)
    }

    /// The understood posts outside thread `root` that cite `id` in their
    /// `refs`, newest first, at most `limit`.
    pub fn citing(&self, id: &str, root: &str, limit: usize) -> Result<Vec<Stored>> {
        self.query(
            "SELECT e.seq, e.json FROM event_refs r JOIN events e ON e.seq = r.seq
             WHERE r.ref = ? AND e.opaque = 0 AND coalesce(e.thread, e.id) <> ?
             ORDER BY e.ts DESC, e.seq DESC LIMIT ?",
            vec![
                id.to_string().into(),
                root.to_string().into(),
                (limit as i64).into(),
            ],
        )
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

    /// Posts semantic search embeds (ADR 0053 §5) that have no vector of
    /// `model` yet: with `after`, those with a `seq` above it, oldest
    /// first (new posts); with `before`, below it, newest first (the
    /// backfill). Each with its title and body.
    pub fn embedding_todo(
        &self,
        model: &str,
        after: Option<i64>,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<(i64, String, String)>> {
        let (cond, order, bound) = match (after, before) {
            (Some(a), _) => ("e.seq > ?2", "e.seq ASC", a),
            (None, Some(b)) => ("e.seq < ?2", "e.seq DESC", b),
            (None, None) => ("?2 = ?2", "e.seq DESC", 0),
        };
        let sql = format!(
            "SELECT e.seq, e.title, e.text FROM events e
             LEFT JOIN embeddings m ON m.seq = e.seq AND m.model = ?1
             WHERE {cond} AND e.opaque = 0 AND e.kind IN ('ask', 'answer', '{}')
                 AND m.seq IS NULL
             ORDER BY {order} LIMIT ?3",
            crate::knowledge::DIGEST_KIND
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![model, bound, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Stores the vector of post `seq`, unless its body is gone meanwhile.
    pub fn put_embedding(&self, seq: i64, model: &str, vec: &[u8]) -> Result<bool> {
        let n = self.conn().execute(
            "INSERT OR REPLACE INTO embeddings (seq, model, vec)
             SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM events WHERE seq = ?1 AND opaque = 0)",
            params![seq, model, vec],
        )?;
        Ok(n == 1)
    }

    /// The vectors of `model` with a `seq` above `after`, in `seq` order.
    pub fn embeddings_after(&self, model: &str, after: i64) -> Result<Vec<(i64, Vec<u8>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT seq, vec FROM embeddings WHERE model = ?1 AND seq > ?2 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![model, after], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// How many vectors of `model` there are, and the highest `seq` among
    /// them: what tells the index in memory it is stale.
    pub fn embeddings_extent(&self, model: &str) -> Result<(usize, i64)> {
        let (n, max): (i64, Option<i64>) = self.conn().query_row(
            "SELECT count(*), max(seq) FROM embeddings WHERE model = ?",
            [model],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((n.max(0) as usize, max.unwrap_or(0)))
    }

    /// Posts embedded with `model`, and posts to embed in all.
    pub fn embedding_counts(&self, model: &str) -> Result<(u64, u64)> {
        let (done, total): (i64, i64) = self.conn().query_row(
            &format!(
                "SELECT
                     (SELECT count(*) FROM embeddings m JOIN events e ON e.seq = m.seq
                      WHERE m.model = ?1 AND e.opaque = 0),
                     (SELECT count(*) FROM events WHERE opaque = 0
                      AND kind IN ('ask', 'answer', '{}'))",
                crate::knowledge::DIGEST_KIND
            ),
            [model],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((done.max(0) as u64, total.max(0) as u64))
    }

    /// Drops the vectors of other models (a new model re-embeds).
    pub fn drop_other_embeddings(&self, model: &str) -> Result<usize> {
        Ok(self
            .conn()
            .execute("DELETE FROM embeddings WHERE model <> ?", [model])?)
    }

    /// Whether an agent announced all of `tags` on `net` since `since`
    /// (unix seconds), in any announce, not only its latest one: a
    /// re-announce must not make a field look empty for a moment.
    pub fn announced_since(&self, net: &str, tags: &[String], since: u64) -> Result<bool> {
        if tags.is_empty() {
            return Ok(false);
        }
        let marks = vec!["?"; tags.len()].join(", ");
        let mut args: Vec<SqlValue> = vec![net.to_string().into(), (since as i64).into()];
        args.extend(tags.iter().map(|t| SqlValue::from(t.to_ascii_lowercase())));
        args.push((tags.len() as i64).into());
        let sql = format!(
            "SELECT EXISTS (
                 SELECT 1 FROM events e JOIN event_tags t ON t.seq = e.seq
                 WHERE e.kind = 'announce' AND e.net = ?1 AND e.ts >= ?2 AND e.opaque = 0
                     AND lower(t.tag) IN ({marks})
                 GROUP BY e.seq HAVING count(DISTINCT lower(t.tag)) = ?{})",
            tags.len() + 3
        );
        let conn = self.conn();
        Ok(conn.query_row(&sql, params_from_iter(args), |r| r.get(0))?)
    }

    /// What others said of `authors`' posts, received since `since`
    /// (unix seconds, by `received_at`): each reacting key's latest
    /// `react` on each post, sorted as [`Received::add`] says. Reacts by
    /// `authors` themselves do not count.
    pub fn signals_received(&self, authors: &[String], since: u64) -> Result<Received> {
        let mut received = Received::default();
        if authors.is_empty() {
            return Ok(received);
        }
        let marks = vec!["?"; authors.len()].join(", ");
        let sql = format!(
            "SELECT r.author, r.parent, r.ts, r.json FROM events p
                 JOIN events r ON r.parent = p.id
             WHERE p.author IN ({marks}) AND p.kind IN ('ask', 'answer', 'x.wearetheswarm.digest')
                 AND r.kind = 'react' AND r.opaque = 0 AND r.received_at >= ?{}
                 AND r.author NOT IN ({marks})",
            authors.len() + 1
        );
        let mut args: Vec<SqlValue> = authors.iter().map(|a| SqlValue::from(a.clone())).collect();
        args.push((since.min(i64::MAX as u64) as i64).into());
        args.extend(authors.iter().map(|a| SqlValue::from(a.clone())));
        let rows: Vec<(String, String, i64, String)> = {
            let conn = self.conn();
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(params_from_iter(args), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?
        };
        // The latest react of each key on each post.
        let mut latest: HashMap<(String, String), (i64, String)> = HashMap::new();
        for (author, parent, ts, json) in rows {
            let entry = latest
                .entry((author, parent))
                .or_insert((ts, String::new()));
            if ts >= entry.0 {
                *entry = (ts, json);
            }
        }
        for (_, json) in latest.into_values() {
            if let Ok(event) = serde_json::from_str::<serde_json::Value>(&json) {
                received.add(&event);
            }
        }
        Ok(received)
    }

    /// When this node stored the event `id` (unix seconds).
    pub fn received_at(&self, id: &str) -> Result<Option<u64>> {
        let at: Option<i64> = self
            .conn()
            .query_row("SELECT received_at FROM events WHERE id = ?", [id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(at.map(|t| t.max(0) as u64))
    }

    /// Whether `author` cites `target` in the `refs` of a post of theirs.
    pub fn cites(&self, author: &str, target: &str) -> Result<bool> {
        Ok(self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM event_refs r JOIN events e ON e.seq = r.seq
                            WHERE r.ref = ?1 AND e.author = ?2 AND e.opaque = 0)",
            params![target, author],
            |r| r.get(0),
        )?)
    }

    /// Adds 1 to each of these find counters, for the day of `now`.
    pub fn count_find(&self, names: &[&str], now: u64) -> Result<()> {
        if names.is_empty() {
            return Ok(());
        }
        let day = (now / 86_400) as i64;
        let conn = self.conn();
        for name in names {
            conn.execute(
                "INSERT INTO find_counters (day, name, value) VALUES (?1, ?2, 1)
                 ON CONFLICT (day, name) DO UPDATE SET value = value + 1",
                params![day, name],
            )?;
        }
        Ok(())
    }

    /// Each find counter's total, and the first day counted (Unix days).
    pub fn find_counts(&self) -> Result<(BTreeMap<String, u64>, Option<i64>)> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare_cached("SELECT name, sum(value) FROM find_counters GROUP BY name")?;
        let totals = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        let first: Option<i64> =
            conn.query_row("SELECT min(day) FROM find_counters", [], |r| r.get(0))?;
        Ok((totals, first))
    }

    /// A value of semantic search's state (`top`, `below`, `done`).
    pub fn semantic_state(&self, key: &str) -> Result<Option<i64>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT value FROM semantic_state WHERE key = ?",
                [key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn delete_semantic_state(&self, key: &str) -> Result<()> {
        self.conn()
            .execute("DELETE FROM semantic_state WHERE key = ?", [key])?;
        Ok(())
    }

    /// Forgets where semantic search stood (a new model starts over).
    pub fn clear_semantic_state(&self) -> Result<()> {
        self.conn().execute("DELETE FROM semantic_state", [])?;
        Ok(())
    }

    pub fn set_semantic_state(&self, key: &str, value: i64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO semantic_state (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
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
        // A source held as a tombstone still gives its signal: its kind,
        // parent and refs are header fields (ADR 0025 §7). A react's body
        // is never evicted, so what is lost is only a reply's text. Not one
        // dropped for another reason: withdrawn or erased, it gives none.
        let held = source_sql("e");
        let sql = format!(
            "SELECT e.parent, 1, {columns} FROM events e
                 WHERE e.parent IN ({marks}) AND {held}
             UNION ALL
             SELECT r.ref, 0, {columns} FROM event_refs r JOIN events e ON e.seq = r.seq
                 WHERE r.ref IN ({marks}) AND {held}
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
    /// understood, or held as the tombstone of an understood event (its
    /// header is kept, ADR 0025 §7): thread roots, for their asker.
    pub fn heads(&self, ids: &[String]) -> Result<Vec<(String, String, String, String)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT id, author, kind, net FROM events
             WHERE id IN ({}) AND (opaque = 0 OR {})",
            vec!["?"; ids.len()].join(","),
            tombstone_sql("events")
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(ids.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// What the knowledge entries of these threads are chosen from (ADR
    /// 0019 §5.2): their `answer` replies, and the digests that may cover
    /// them (rooted in or replying in one, or citing a post of one in
    /// `refs`). Grafted replies (B9) are left out. Newest first, at most
    /// `limit`.
    pub fn knowledge_candidates(&self, threads: &[String], limit: usize) -> Result<Vec<Stored>> {
        if threads.is_empty() {
            return Ok(Vec::new());
        }
        let n = threads.len();
        let marks = (1..=n)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let (kind, limit_mark) = (n + 1, n + 2);
        // A row `e` sits in the thread it claims (see [`NOT_GRAFTED`]).
        let in_place = "NOT EXISTS (SELECT 1 FROM events r WHERE r.id = e.thread
                 AND r.opaque = 0 AND r.net <> e.net)
             AND NOT EXISTS (SELECT 1 FROM events p WHERE p.id = e.parent AND p.opaque = 0
                 AND (p.net <> e.net OR COALESCE(p.thread, p.id) <> e.thread))";
        let sql = format!(
            "SELECT e.seq, e.json, e.ts FROM events e
                 WHERE e.thread IN ({marks}) AND e.kind IN ('answer', ?{kind})
                     AND e.opaque = 0 AND {in_place}
             UNION
             SELECT e.seq, e.json, e.ts FROM events e
                 WHERE e.id IN ({marks}) AND e.kind = ?{kind} AND e.opaque = 0
             UNION
             SELECT e.seq, e.json, e.ts FROM events t
                 JOIN event_refs r ON r.ref = t.id JOIN events e ON e.seq = r.seq
                 WHERE t.id IN ({marks}) AND t.opaque = 0
                     AND e.kind = ?{kind} AND e.opaque = 0 AND {in_place}
             UNION
             SELECT e.seq, e.json, e.ts FROM events t
                 JOIN event_refs r ON r.ref = t.id JOIN events e ON e.seq = r.seq
                 WHERE t.thread IN ({marks}) AND t.opaque = 0
                     AND e.kind = ?{kind} AND e.opaque = 0 AND {in_place}
             ORDER BY 3 DESC, 1 DESC LIMIT ?{limit_mark}"
        );
        let mut args: Vec<SqlValue> = threads.iter().map(|t| t.clone().into()).collect();
        args.push(crate::knowledge::DIGEST_KIND.to_string().into());
        args.push((limit as i64).into());
        self.query(&sql, args)
    }

    /// The digests whose `data.supersedes` names one of `ids` (ADR 0019
    /// §5.1), at most `limit`.
    pub fn superseding(&self, ids: &[String], limit: usize) -> Result<Vec<Stored>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT seq, json FROM events WHERE kind = ? AND opaque = 0
                 AND json_extract(json, '$.data.supersedes') IN ({}) LIMIT ?",
            vec!["?"; ids.len()].join(",")
        );
        let mut args: Vec<SqlValue> = vec![crate::knowledge::DIGEST_KIND.to_string().into()];
        args.extend(ids.iter().map(|id| SqlValue::from(id.clone())));
        args.push((limit as i64).into());
        self.query(&sql, args)
    }

    /// `(id, thread root, net)` of each of these events held here and
    /// understood.
    pub fn roots(&self, ids: &[String]) -> Result<Vec<(String, String, String)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT id, COALESCE(thread, id), net FROM events WHERE id IN ({}) AND opaque = 0",
            vec!["?"; ids.len()].join(",")
        );
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(ids.iter()), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
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

    /// Every event of network `net` held here (understood or not): what a
    /// key's age and proof of work are read from (ADR 0019 §2.3).
    pub fn author_events(&self, net: &str) -> Result<Vec<AuthorEvent>> {
        let conn = self.conn();
        // The leading zero bits of the id: four per leading '0', then those
        // of the first other hex digit.
        let mut stmt = conn.prepare_cached(
            "SELECT author, ts, received_at, LENGTH(json),
                 (LENGTH(id) - LENGTH(LTRIM(id, '0'))) * 4
                 + CASE SUBSTR(LTRIM(id, '0'), 1, 1)
                     WHEN '1' THEN 3 WHEN '2' THEN 2 WHEN '3' THEN 2
                     WHEN '4' THEN 1 WHEN '5' THEN 1 WHEN '6' THEN 1 WHEN '7' THEN 1
                     ELSE 0 END
             FROM events WHERE net = ?",
        )?;
        let rows = stmt.query_map([net], |r| {
            Ok(AuthorEvent {
                author: r.get(0)?,
                ts: r.get::<_, i64>(1)?.max(0) as u64,
                received_at: r.get::<_, i64>(2)?.max(0) as u64,
                size: r.get::<_, i64>(3)?.max(0) as usize,
                pow_bits: r.get::<_, i64>(4)?.clamp(0, 256) as u32,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// `(voucher, vouched, received_at)` of every `vouch` of network `net`
    /// held here: the vouched key is the author of the post it replies to
    /// (by convention, its `announce`), held in full or as a tombstone.
    pub fn vouches(&self, net: &str) -> Result<Vec<(String, String, u64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT e.author, t.author, e.received_at FROM events e
                 JOIN events t ON t.id = e.parent AND (t.opaque = 0 OR {})
             WHERE e.net = ? AND e.kind = 'vouch' AND e.opaque = 0",
            tombstone_sql("t")
        ))?;
        let rows = stmt.query_map([net], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)?.max(0) as u64))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// `(key, new key)` of every key succession (`rotate`) held here.
    pub fn rotations(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT author, json_extract(json, '$.data.new_key') FROM events
                 WHERE kind = 'rotate' AND opaque = 0 AND v = 1
                 AND json_type(json, '$.data.new_key') = 'text'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// `(author, ts, data.owner)` of every `announce` of network `net`,
    /// the owner when it is a string (ADR 0019 §2.4).
    pub fn announce_owners(&self, net: &str) -> Result<Vec<(String, u64, Option<String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT author, ts, CASE WHEN json_type(json, '$.data.owner') = 'text'
                 THEN json_extract(json, '$.data.owner') END
             FROM events WHERE net = ? AND kind = 'announce' AND opaque = 0",
        )?;
        let rows = stmt.query_map([net], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)?.max(0) as u64, r.get(2)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// `(thread, asker)` of every thread of network `net` whose root is an
    /// `ask`, held in full or as a tombstone.
    pub fn ask_roots(&self, net: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT id, author FROM events
                 WHERE net = ? AND kind = 'ask' AND thread IS NULL
                 AND (opaque = 0 OR {})",
            tombstone_sql("events")
        ))?;
        let rows = stmt.query_map([net], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// [`Store::accept_reacts`] for every thread of network `net`, newest
    /// first, at most `limit`.
    pub fn accept_reacts_in(&self, net: &str, limit: usize) -> Result<Vec<AcceptRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT thread, id, author, ts, parent, json_extract(json, '$.data.accept')
             FROM events WHERE net = ? AND kind = 'react' AND opaque = 0
                 AND parent IS NOT NULL AND thread IS NOT NULL
                 AND json_type(json, '$.data.accept') IN ('true', 'false')
             ORDER BY ts DESC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![net, limit as i64], |r| {
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

    /// [`Store::signal_rows`] for every post of network `net` at once
    /// (without titles), newest first, at most `limit`, each with the post
    /// it points to when held here: the reputation table's input (ADR 0019
    /// §2).
    pub fn net_signal_rows(
        &self,
        net: &str,
        limit: usize,
    ) -> Result<Vec<(SignalRow, Option<TargetHead>)>> {
        let columns = "e.id, e.author, e.ts AS ts, e.net, e.kind,
             CASE WHEN e.kind = 'react' THEN json_extract(e.json, '$.data') END,
             t.author, t.kind, t.thread";
        // A target held as a tombstone keeps its head (ADR 0025 §7): an
        // evicted post keeps its signals and its author's score.
        let held = format!("(t.opaque = 0 OR {})", tombstone_sql("t"));
        // Sources held as tombstones too: their signals keep counting, but
        // for one withdrawn or erased.
        let source = source_sql("e");
        let sql = format!(
            "SELECT e.parent, 1, {columns} FROM events e
                 LEFT JOIN events t ON t.id = e.parent AND {held}
                 WHERE e.net = ?1 AND e.parent IS NOT NULL AND {source}
             UNION ALL
             SELECT r.ref, 0, {columns} FROM event_refs r JOIN events e ON e.seq = r.seq
                 LEFT JOIN events t ON t.id = r.ref AND {held}
                 WHERE e.net = ?1 AND {source}
             ORDER BY ts DESC LIMIT ?2"
        );
        // The JSON of `data` is parsed after the lock is released: inserts
        // wait for the query only.
        let raw: Vec<(SignalRow, Option<String>, Option<TargetHead>)> = {
            let conn = self.conn();
            let mut stmt = conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params![net, limit as i64], |r| {
                let row = SignalRow {
                    target: r.get(0)?,
                    reply: r.get(1)?,
                    id: r.get(2)?,
                    author: r.get(3)?,
                    ts: r.get::<_, i64>(4)?.max(0) as u64,
                    net: r.get(5)?,
                    kind: r.get(6)?,
                    data: Default::default(),
                    title: String::new(),
                };
                let head = match r.get::<_, Option<String>>(8)? {
                    Some(author) => Some(TargetHead {
                        author,
                        kind: r.get(9)?,
                        thread: r.get(10)?,
                    }),
                    None => None,
                };
                Ok((row, r.get::<_, Option<String>>(7)?, head))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(raw
            .into_iter()
            .map(|(mut row, data, head)| {
                row.data = data
                    .and_then(|d| serde_json::from_str(&d).ok())
                    .unwrap_or_default();
                (row, head)
            })
            .collect())
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

/// What others said of a node's posts ([`Store::signals_received`]), for
/// people: each react counted once, as the first that fits of accepted,
/// used, confirmed (+1 with evidence), useful (+1), challenged (−1 with a
/// reason) and −1.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Received {
    pub useful: u64,
    pub used: u64,
    pub accepted: u64,
    pub confirmed: u64,
    pub challenged: u64,
    pub down: u64,
}

impl Received {
    /// Count one `react` (its JSON).
    pub fn add(&mut self, react: &serde_json::Value) {
        let data = &react["data"];
        let value = data["value"].as_i64();
        // As the ranking reads it (ADR 0045 §2): evidence, or a comment
        // of some length.
        let reasoned = data.get("evidence").is_some_and(|e| !e.is_null())
            || react["title"]
                .as_str()
                .is_some_and(|t| t.trim().chars().count() >= crate::signals::REASON_CHARS);
        if data["accept"].as_bool() == Some(true) {
            self.accepted += 1;
        } else if data["used"].as_bool() == Some(true) {
            self.used += 1;
        } else if value == Some(1) && data.get("evidence").is_some_and(|e| !e.is_null()) {
            self.confirmed += 1;
        } else if value == Some(1) {
            self.useful += 1;
        } else if value == Some(-1) && reasoned {
            self.challenged += 1;
        } else if value == Some(-1) {
            self.down += 1;
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Received::default()
    }

    /// "3 useful (+1), 2 used, 1 confirmed, 1 challenged (−1 with a
    /// reason)": the counts that are not 0.
    pub fn phrase(&self) -> String {
        [
            (self.useful, "useful (+1)"),
            (self.used, "used"),
            (self.accepted, "accepted"),
            (self.confirmed, "confirmed"),
            (self.challenged, "challenged (−1 with a reason)"),
            (self.down, "−1"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect::<Vec<_>>()
        .join(", ")
    }
}

/// Where a history range stands (ADR 0025 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeState {
    /// Still to fetch, resumable from its cursor.
    Pending,
    /// Below the initial fill's cut, where the budget stopped it: not
    /// held, on purpose.
    Cut,
    /// The budget ran out inside a catch-up: not held, on purpose.
    Skipped,
    /// Complete, waiting for older pending ranges before the watermark
    /// moves.
    Done,
    /// Walked to its end, but no trusted peer confirmed that nothing is
    /// missing: held and served as held, shown as unconfirmed until a
    /// trusted peer confirms it (ADR 0025 §9).
    Unconfirmed,
}

impl RangeState {
    pub fn as_str(self) -> &'static str {
        match self {
            RangeState::Pending => "pending",
            RangeState::Cut => "cut",
            RangeState::Skipped => "skipped",
            RangeState::Done => "done",
            RangeState::Unconfirmed => "unconfirmed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "cut" => RangeState::Cut,
            "skipped" => RangeState::Skipped,
            "done" => RangeState::Done,
            "unconfirmed" => RangeState::Unconfirmed,
            _ => RangeState::Pending,
        }
    }
}

/// A history range of a network (ADR 0025 §5): what this node still has to
/// fetch newest first, or chose not to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRange {
    pub id: i64,
    pub net: String,
    pub lo_ts: u64,
    pub hi_ts: u64,
    /// The last event fetched walking down: the range is held above it.
    pub cursor_ts: u64,
    pub cursor_id: String,
    pub state: RangeState,
    /// The initial fill's phase; empty for a catch-up range.
    pub phase: String,
    /// When it was opened: the watermark once it is complete.
    pub opened: u64,
    /// Store bytes when it was opened.
    pub base_bytes: u64,
}

/// The initial fill's last phase: everything, newest first, down to the
/// budget (ADR 0025 §4, phase 2).
pub const PHASE_WINDOW: &str = "window";

impl SyncRange {
    /// The highest `ts` of the hole this range leaves, if any: this node
    /// does not hold the complete history of `[hole_lo, hole_top]`. Counted
    /// by whole seconds: events at `hole_top` itself may be missing.
    pub fn hole_top(&self) -> Option<u64> {
        match self.state {
            RangeState::Done | RangeState::Unconfirmed => None,
            // The initial fill before its window: nothing below its top is
            // held yet but the kinds of its first phases.
            RangeState::Pending if !self.phase.is_empty() && self.phase != PHASE_WINDOW => {
                Some(self.hi_ts)
            }
            _ => Some(self.cursor_ts),
        }
    }

    /// The bottom of that hole: 0 for a cut.
    pub fn hole_lo(&self) -> u64 {
        match self.state {
            RangeState::Cut => 0,
            _ => self.lo_ts,
        }
    }
}

/// What a `sync` page asks for, served by [`Store::sync_page`].
#[derive(Debug, Clone, Default)]
pub struct PageQuery {
    pub net: String,
    /// Strictly after this `(ts, id)`.
    pub after: (u64, String),
    /// Strictly before this `(ts, id)`, newest first; `None`: ascending.
    pub before: Option<(u64, String)>,
    /// Only events with `ts` above this (a descending page stops at a hole).
    pub above_ts: Option<u64>,
    /// Only these kinds (empty: all).
    pub kinds: Vec<String>,
    /// Only these authors (empty: all).
    pub authors: Vec<String>,
    pub limit: usize,
    /// Below this `ts` (a rotation cut), a `react` asked for is served only
    /// when it is an erasure label: the only reacts rotation keeps there.
    pub reacts_below: Option<u64>,
}

/// The JSON of a row, with when this node stored it.
pub type SeenRow = (Value, u64);

/// Rows `(seq, json, received_at)` into [`SeenRow`]s.
fn seen_rows(conn: &Connection, sql: &str, args: Vec<SqlValue>) -> Result<Vec<SeenRow>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params_from_iter(args), |r| {
        Ok((r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (json, received_at) = row?;
        out.push((
            serde_json::from_str(&json).context("corrupt stored event")?,
            received_at.max(0) as u64,
        ));
    }
    Ok(out)
}

fn marks(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Peers whose first sightings of a key are kept (ADR 0025 §9).
pub const SEEN_PEERS: i64 = 8;

/// A Unix time as SQLite stores it.
fn sql_ts(ts: u64) -> i64 {
    ts.min(i64::MAX as u64) as i64
}

impl Store {
    /// The store bytes (ADR 0025 §1): the used pages of the database, the
    /// full-text index included. With WAL, `page_count` already counts the
    /// pages committed to the log.
    pub fn store_bytes(&self) -> u64 {
        let conn = self.conn();
        let pragma = |name: &str| -> i64 {
            conn.pragma_query_value(None, name, |r| r.get(0))
                .unwrap_or(0)
        };
        let used = pragma("page_count").saturating_sub(pragma("freelist_count"));
        (used.max(0) as u64).saturating_mul(pragma("page_size").max(0) as u64)
    }

    /// A page of events of a network for `sync`, as wire JSON with when
    /// this node stored each: ascending after `after`, or newest first
    /// between `before` and `after`.
    pub fn sync_page(&self, q: &PageQuery) -> Result<Vec<SeenRow>> {
        let mut conds = vec![
            "net = ?".to_string(),
            "(ts > ? OR (ts = ? AND id > ?))".to_string(),
        ];
        let after_ts = sql_ts(q.after.0);
        let mut args: Vec<SqlValue> = vec![
            q.net.clone().into(),
            after_ts.into(),
            after_ts.into(),
            q.after.1.clone().into(),
        ];
        if let Some((ts, id)) = &q.before {
            conds.push("(ts < ? OR (ts = ? AND id < ?))".into());
            args.extend([sql_ts(*ts).into(), sql_ts(*ts).into(), id.clone().into()]);
        }
        if let Some(above) = q.above_ts {
            conds.push("ts > ?".into());
            args.push(sql_ts(above).into());
        }
        if !q.kinds.is_empty() {
            match q.reacts_below {
                Some(cut) => {
                    conds.push(format!(
                        "(kind IN ({}) AND (kind <> 'react' OR ts > ?
                             OR json_extract(json, '$.data.label') IS 'erasure'))",
                        marks(q.kinds.len())
                    ));
                    args.extend(q.kinds.iter().map(|k| SqlValue::from(k.clone())));
                    args.push(sql_ts(cut).into());
                }
                None => {
                    conds.push(format!("kind IN ({})", marks(q.kinds.len())));
                    args.extend(q.kinds.iter().map(|k| SqlValue::from(k.clone())));
                }
            }
        }
        if !q.authors.is_empty() {
            conds.push(format!("author IN ({})", marks(q.authors.len())));
            args.extend(q.authors.iter().map(|a| SqlValue::from(a.clone())));
        }
        args.push((q.limit as i64).into());
        let order = if q.before.is_some() {
            "ts DESC, id DESC"
        } else {
            "ts, id"
        };
        let sql = format!(
            "SELECT seq, json, received_at FROM events WHERE {} ORDER BY {order} LIMIT ?",
            conds.join(" AND ")
        );
        seen_rows(&self.conn(), &sql, args)
    }

    /// The events of `net` held here among `ids`, full or tombstone.
    pub fn events_by_ids(&self, net: &str, ids: &[String]) -> Result<Vec<SeenRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<SqlValue> = vec![net.to_string().into()];
        args.extend(ids.iter().map(|i| SqlValue::from(i.clone())));
        seen_rows(
            &self.conn(),
            &format!(
                "SELECT seq, json, received_at FROM events WHERE net = ? AND id IN ({})",
                marks(ids.len())
            ),
            args,
        )
    }

    /// The thread rooted at `root` in `net`, understood or not, oldest
    /// first, at most `limit` rows.
    pub fn thread_rows(&self, net: &str, root: &str, limit: usize) -> Result<Vec<SeenRow>> {
        seen_rows(
            &self.conn(),
            "SELECT seq, json, received_at FROM events
             WHERE net = ?1 AND (id = ?2 OR thread = ?2) ORDER BY ts, seq LIMIT ?3",
            vec![
                net.to_string().into(),
                root.to_string().into(),
                (limit as i64).into(),
            ],
        )
    }

    /// The events of `net` that reply to one of `ids` or cite it (the rows
    /// of [`Store::signal_rows`], as wire JSON), at most `limit`.
    pub fn signal_events(&self, net: &str, ids: &[String], limit: usize) -> Result<Vec<SeenRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let m = marks(ids.len());
        let mut args: Vec<SqlValue> = vec![net.to_string().into()];
        for _ in 0..2 {
            args.extend(ids.iter().map(|i| SqlValue::from(i.clone())));
        }
        args.push((limit as i64).into());
        seen_rows(
            &self.conn(),
            &format!(
                "SELECT seq, json, received_at FROM events WHERE net = ? AND (parent IN ({m})
                     OR seq IN (SELECT seq FROM event_refs WHERE ref IN ({m})))
                 ORDER BY ts, seq LIMIT ?"
            ),
            args,
        )
    }

    /// The roots of the threads of `net` that `authors` posted in.
    pub fn threads_of(&self, net: &str, authors: &[String], limit: usize) -> Result<Vec<String>> {
        if authors.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<SqlValue> = vec![net.to_string().into()];
        args.extend(authors.iter().map(|a| SqlValue::from(a.clone())));
        args.push((limit as i64).into());
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT DISTINCT COALESCE(thread, id) FROM events
             WHERE net = ? AND author IN ({}) LIMIT ?",
            marks(authors.len())
        ))?;
        let rows = stmt.query_map(params_from_iter(args), |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The ids of the events of `kind` in `net`, newest first.
    pub fn ids_of_kind(&self, net: &str, kind: &str, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT id FROM events WHERE net = ? AND kind = ? ORDER BY ts DESC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![net, kind, limit as i64], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The oldest event timestamp of a network, if any.
    pub fn oldest_ts(&self, net: &str) -> Result<Option<u64>> {
        let ts: Option<i64> =
            self.conn()
                .query_row("SELECT min(ts) FROM events WHERE net = ?", [net], |r| {
                    r.get(0)
                })?;
        Ok(ts.map(|t| t.max(0) as u64))
    }

    /// The history ranges of a network, newest first.
    pub fn sync_ranges(&self, net: &str) -> Result<Vec<SyncRange>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(
            "SELECT id, net, lo_ts, hi_ts, cursor_ts, cursor_id, state, phase, opened, base_bytes
             FROM sync_ranges WHERE net = ? ORDER BY hi_ts DESC, id DESC",
        )?;
        let rows = stmt.query_map([net], |r| {
            let n = |i: usize| r.get::<_, i64>(i).map(|v| v.max(0) as u64);
            Ok(SyncRange {
                id: r.get(0)?,
                net: r.get(1)?,
                lo_ts: n(2)?,
                hi_ts: n(3)?,
                cursor_ts: n(4)?,
                cursor_id: r.get(5)?,
                state: RangeState::parse(&r.get::<_, String>(6)?),
                phase: r.get(7)?,
                opened: n(8)?,
                base_bytes: n(9)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Record a range (its `id` is ignored); returns its id.
    pub fn add_sync_range(&self, range: &SyncRange) -> Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO sync_ranges
                 (net, lo_ts, hi_ts, cursor_ts, cursor_id, state, phase, opened, base_bytes)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                range.net,
                sql_ts(range.lo_ts),
                sql_ts(range.hi_ts),
                sql_ts(range.cursor_ts),
                range.cursor_id,
                range.state.as_str(),
                range.phase,
                sql_ts(range.opened),
                sql_ts(range.base_bytes),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Store a range's bounds, cursor, phase and state.
    pub fn update_sync_range(&self, range: &SyncRange) -> Result<()> {
        self.conn().execute(
            "UPDATE sync_ranges SET lo_ts = ?, hi_ts = ?, cursor_ts = ?, cursor_id = ?,
                 state = ?, phase = ?, opened = ?, base_bytes = ? WHERE id = ?",
            params![
                sql_ts(range.lo_ts),
                sql_ts(range.hi_ts),
                sql_ts(range.cursor_ts),
                range.cursor_id,
                range.state.as_str(),
                range.phase,
                sql_ts(range.opened),
                sql_ts(range.base_bytes),
                range.id,
            ],
        )?;
        Ok(())
    }

    /// Forget a range.
    pub fn delete_sync_range(&self, id: i64) -> Result<()> {
        self.conn()
            .execute("DELETE FROM sync_ranges WHERE id = ?", [id])?;
        Ok(())
    }

    /// Once no range of `net` is pending: the watermark becomes the opening
    /// time of the newest range completed (or skipped), and the completed
    /// ones are forgotten (ADR 0025 §5). Returns the new watermark, if it moved.
    pub fn settle_sync_ranges(&self, net: &str) -> Result<Option<u64>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let pending: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sync_ranges WHERE net = ? AND state = 'pending')",
            [net],
            |r| r.get(0),
        )?;
        if pending {
            return Ok(None);
        }
        // A range skipped for the budget is over too, on purpose, and an
        // unconfirmed one was walked to its end.
        let opened: Option<i64> = tx.query_row(
            "SELECT max(opened) FROM sync_ranges WHERE net = ?
             AND state IN ('done', 'skipped', 'unconfirmed')",
            [net],
            |r| r.get(0),
        )?;
        let Some(opened) = opened else {
            return Ok(None);
        };
        tx.execute(
            "DELETE FROM sync_ranges WHERE net = ? AND state = 'done'",
            [net],
        )?;
        let current: Option<i64> = tx
            .query_row(
                "SELECT watermark FROM sync_state WHERE net = ?",
                [net],
                |r| r.get(0),
            )
            .optional()?;
        let moved = current.is_none_or(|w| opened > w);
        if moved {
            tx.execute(
                "INSERT INTO sync_state (net, watermark) VALUES (?1, ?2)
                 ON CONFLICT (net) DO UPDATE SET watermark = max(watermark, ?2)",
                params![net, opened],
            )?;
        }
        tx.commit()?;
        Ok(moved.then_some(opened.max(0) as u64))
    }

    /// Record what `peer` says about when it first stored events of these
    /// authors (`seen` in sync pages): its earliest report per author.
    /// `history_full`: whether the peer advertised it in its hello. The
    /// callers pass only events this node holds, by their verified author.
    ///
    /// At most [`SEEN_PEERS`] peers per key, the first ones to report it:
    /// which peers count does not depend on what they report, so peers
    /// reporting early times cannot push the others out (ADR 0025, "Key
    /// age"). One exception, so that a key can meet the full-history rule:
    /// a peer with full history takes the place of the latest report of a
    /// key that has none from such a peer.
    pub fn record_seen(
        &self,
        peer: &str,
        history_full: bool,
        reports: &[(String, u64)],
    ) -> Result<()> {
        if reports.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut kept = false;
        {
            let mut known = tx.prepare_cached(
                "UPDATE key_seen SET seen = min(seen, ?3) WHERE author = ?1 AND peer = ?2",
            )?;
            let mut held = tx.prepare_cached(
                "SELECT count(*), coalesce(sum(p.history_full), 0)
                 FROM key_seen k LEFT JOIN seen_peers p ON p.peer = k.peer
                 WHERE k.author = ?1",
            )?;
            let mut latest = tx.prepare_cached(
                "SELECT k.peer FROM key_seen k LEFT JOIN seen_peers p ON p.peer = k.peer
                 WHERE k.author = ?1 AND coalesce(p.history_full, 0) = 0
                 ORDER BY k.seen DESC, k.peer DESC LIMIT 1",
            )?;
            let mut forget =
                tx.prepare_cached("DELETE FROM key_seen WHERE author = ?1 AND peer = ?2")?;
            let mut insert =
                tx.prepare_cached("INSERT INTO key_seen (author, peer, seen) VALUES (?1, ?2, ?3)")?;
            for (author, seen) in reports {
                if author.len() > 64 {
                    continue;
                }
                let seen = sql_ts(*seen);
                if known.execute(params![author, peer, seen])? > 0 {
                    kept = true;
                    continue;
                }
                let (count, full): (i64, i64) =
                    held.query_row([author], |r| Ok((r.get(0)?, r.get(1)?)))?;
                if count >= SEEN_PEERS {
                    if !history_full || full > 0 {
                        continue;
                    }
                    let Some(other) = latest
                        .query_row([author], |r| r.get::<_, String>(0))
                        .optional()?
                    else {
                        continue;
                    };
                    forget.execute(params![author, other])?;
                }
                insert.execute(params![author, peer, seen])?;
                kept = true;
            }
        }
        if kept {
            tx.execute(
                "INSERT INTO seen_peers (peer, history_full) VALUES (?1, ?2)
                 ON CONFLICT (peer) DO UPDATE SET history_full = ?2",
                params![peer, history_full],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every peer's report of when it first stored an event of each key
    /// (`key_seen`), with whether that peer has full history: author to
    /// `(seen, full)`, one per peer. Empty on a store no 0.4.2 node has
    /// opened yet (read-only).
    pub fn seen_reports(&self) -> Result<HashMap<String, Vec<(u64, bool)>>> {
        let conn = self.conn();
        let tables: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('key_seen', 'seen_peers')",
            [],
            |r| r.get(0),
        )?;
        let mut out: HashMap<String, Vec<(u64, bool)>> = HashMap::new();
        if tables < 2 {
            return Ok(out);
        }
        let mut stmt = conn.prepare_cached(
            "SELECT k.author, k.seen, coalesce(p.history_full, 0)
             FROM key_seen k LEFT JOIN seen_peers p ON p.peer = k.peer",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?.max(0) as u64,
                r.get::<_, i64>(2)? != 0,
            ))
        })?;
        for row in rows {
            let (author, seen, full) = row?;
            out.entry(author).or_default().push((seen, full));
        }
        Ok(out)
    }
}

/// Whether table `name` exists: a store opened read-only may predate it.
fn has_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?)",
        [name],
        |r| r.get(0),
    )?)
}

/// A value of `store_meta`, if set (and the table exists).
fn meta(conn: &Connection, key: &str) -> Result<Option<i64>> {
    if !has_table(conn, "store_meta")? {
        return Ok(None);
    }
    Ok(conn
        .query_row("SELECT value FROM store_meta WHERE key = ?", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

fn set_meta(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO store_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Record, in the transaction that changes it, what opaque rows count in
/// the quota, with the newest `seq` it covers: the next open adds only the
/// rows above it (ADR 0025, PR B notes, "Open cost").
fn persist_quota(conn: &Connection, value: u64) -> Result<()> {
    set_meta(conn, "opaque_bytes", sql_ts(value))?;
    let max: i64 = conn.query_row("SELECT COALESCE(max(seq), 0) FROM events", [], |r| r.get(0))?;
    set_meta(conn, "opaque_seq", max)
}

/// The opaque quota's count at open: the cached value plus the rows added
/// since, or a full count once a day (a downgraded node may have deleted
/// rows without updating it; it then over-counts, the safe way).
fn opaque_bytes_at_open(conn: &Connection, now: u64) -> Result<u64> {
    let quota = quota_sql("e");
    let full_at = meta(conn, "opaque_full_at")?.unwrap_or(0);
    let cached = match (meta(conn, "opaque_bytes")?, meta(conn, "opaque_seq")?) {
        (Some(value), Some(seq)) if sql_ts(now).saturating_sub(full_at) < 24 * 3600 => {
            Some((value, seq))
        }
        _ => None,
    };
    let value: i64 = match cached {
        Some((value, seq)) => {
            let added: i64 = conn.query_row(
                &format!(
                    "SELECT COALESCE(SUM(LENGTH(CAST(e.json AS BLOB))), 0) FROM events e
                     WHERE e.seq > ? AND {quota}"
                ),
                [seq],
                |r| r.get(0),
            )?;
            value.saturating_add(added)
        }
        None => {
            let full: i64 = conn.query_row(
                &format!(
                    "SELECT COALESCE(SUM(LENGTH(CAST(e.json AS BLOB))), 0) FROM events e
                     WHERE {quota}"
                ),
                [],
                |r| r.get(0),
            )?;
            set_meta(conn, "opaque_full_at", sql_ts(now))?;
            full
        }
    };
    let value = value.max(0) as u64;
    persist_quota(conn, value)?;
    Ok(value)
}

/// Of `hashes`, those a kept full event lists (`event_blobs`), in one query
/// per 500.
fn referenced_among(conn: &Connection, hashes: &[String]) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for chunk in hashes.chunks(500) {
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT b.hash FROM event_blobs b JOIN events e ON e.seq = b.seq
             WHERE e.opaque = 0 AND b.hash IN ({})",
            marks(chunk.len())
        ))?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?;
        for row in rows {
            out.insert(row?);
        }
    }
    Ok(out)
}

/// The blob hashes a full event's JSON lists.
fn blob_hashes(full: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for blob in full
        .get("blobs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(hash) = blob.get("hash").and_then(Value::as_str)
            && !out.iter().any(|h| h == hash)
        {
            out.push(hash.to_string());
        }
    }
    out
}

/// Whether `row` lists a blob, or has a drop record: stage 1's batch leaves
/// it to the node's eviction (or to the stronger reason).
fn has_blobs_or_record(tx: &rusqlite::Transaction<'_>, row: &RowHead) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM event_blobs WHERE seq = ?1)
             OR EXISTS (SELECT 1 FROM dropped WHERE id = ?2)",
        params![row.seq, row.id],
        |r| r.get(0),
    )?)
}

/// Whether `row` lists a blob, or replicas were kept when its body was
/// evicted: stages 0 and 2 do not delete it as such (ADR 0029).
fn has_blobs_or_replicas(tx: &rusqlite::Transaction<'_>, row: &RowHead) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM event_blobs WHERE seq = ?1)
             OR EXISTS (SELECT 1 FROM evicted_replicas WHERE id = ?2)",
        params![row.seq, row.id],
        |r| r.get(0),
    )?)
}

/// Rotation never drops a body that saves less than this (ADR 0025 §7):
/// the full event must be at least 1 KiB larger than its tombstone.
pub const EVICT_MIN_SAVING: u64 = 1024;

/// Rotation runs kept in `rotation_log`.
const ROTATION_LOG_KEEP: i64 = 100;

/// A row as rotation sees it (ADR 0025 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowHead {
    pub seq: i64,
    pub id: String,
    pub net: String,
    pub author: String,
    pub kind: String,
    /// The root of its thread: its own id for a root.
    pub root: String,
    pub parent: Option<String>,
    pub ts: u64,
    /// Not a full event this node understands: a tombstone, or an event
    /// of a version it does not understand.
    pub opaque: bool,
    /// An erasure label (`react` with `data.label = "erasure"`): kept
    /// forever, so that it keeps travelling.
    pub erasure: bool,
    /// Bytes of its wire JSON.
    pub len: u64,
}

const ROW_HEAD: &str = "e.seq, e.id, e.net, e.author, e.kind, COALESCE(e.thread, e.id), e.parent,
     e.ts, e.opaque,
     CASE WHEN e.kind = 'react' AND e.opaque = 0
          THEN json_extract(e.json, '$.data.label') IS 'erasure' ELSE 0 END,
     LENGTH(CAST(e.json AS BLOB))";

/// The columns of [`ROW_HEAD`] in a row, from column 0.
fn row_head(r: &rusqlite::Row<'_>) -> rusqlite::Result<RowHead> {
    Ok(RowHead {
        seq: r.get(0)?,
        id: r.get(1)?,
        net: r.get(2)?,
        author: r.get(3)?,
        kind: r.get(4)?,
        root: r.get(5)?,
        parent: r.get(6)?,
        ts: r.get::<_, i64>(7)?.max(0) as u64,
        opaque: r.get(8)?,
        erasure: r.get(9)?,
        len: r.get::<_, i64>(10)?.max(0) as u64,
    })
}

fn row_heads(conn: &Connection, sql: &str, args: Vec<SqlValue>) -> Result<Vec<RowHead>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params_from_iter(args), row_head)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Most parent links walked up from one of this node's posts.
const ANCESTOR_DEPTH: usize = 64;
/// Most replies looked at below a row, inside a transaction, for a post of
/// this node's among them.
const SUBTREE_ROWS: usize = 2_000;

/// What rotation keeps (ADR 0025 §7, as narrowed in the PR B review):
///
/// - **strictly**, in every stage: posts of this node's profiles, the asks
///   a human posted from an MCP host (`ask_notify`), the kept-forever kinds
///   (halts, key successions, retractions, announces, vouches) and erasure
///   labels;
/// - **in stages 1 and 2**, in the threads those posts are in: their
///   ancestors (up to the root) and the direct replies to them. Stage 0
///   may trim these when their author is over its share, so that a key
///   flooding replies into this node's threads cannot pin the store full.
///   The other rows of those threads are anyone's, ranked like any other.
#[derive(Debug, Clone, Default)]
pub struct Keep {
    /// This node's profiles: those it uses now, and those recorded in
    /// `local_keys` since 0.4.3.
    pub authors: HashSet<String>,
    /// Rows kept as such: the ancestors of this node's posts, and the asks
    /// of `ask_notify`.
    pub ids: HashSet<String>,
    /// A row replying to one of these is kept: this node's posts and the
    /// asks of `ask_notify`.
    pub parents: HashSet<String>,
}

impl Keep {
    /// Whether a row of `kind` is kept forever.
    pub fn kind_kept(kind: &str) -> bool {
        crate::node::history::KEEP_KINDS.contains(&kind)
    }

    /// Whether rotation must leave `row` alone in every stage.
    pub fn protects_strictly(&self, row: &RowHead) -> bool {
        Keep::kind_kept(&row.kind) || row.erasure || self.authors.contains(&row.author)
    }

    /// Whether stages 1 and 2 must leave `row` alone: [`Keep::protects_strictly`],
    /// or an ancestor of, or a direct reply to, one of this node's posts or
    /// a human's ask.
    pub fn protects(&self, row: &RowHead) -> bool {
        self.protects_strictly(row)
            || self.ids.contains(&row.id)
            || row
                .parent
                .as_ref()
                .is_some_and(|p| self.parents.contains(p))
    }

    /// The same, checked again inside a rotation's transaction, so that a
    /// profile or a post of this node's since the plan was made counts
    /// too: the row's author or its parent's is one of this node's
    /// (`local_keys` included), it is or replies to a human's ask, or one
    /// of this node's posts sits below it.
    fn protects_now(
        &self,
        tx: &rusqlite::Transaction<'_>,
        row: &RowHead,
        strictly: bool,
    ) -> Result<bool> {
        if (self.protects(row) && !strictly) || self.protects_strictly(row) {
            return Ok(true);
        }
        if strictly {
            // A profile recorded since, or a human's ask.
            let kept: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM local_keys WHERE author = ?1)
                     OR EXISTS (SELECT 1 FROM ask_notify WHERE ask = ?2)",
                params![row.author, row.id],
                |r| r.get(0),
            )?;
            return Ok(kept);
        }
        let authors: Vec<&String> = self.authors.iter().collect();
        // Numbered, so that the list can appear twice.
        let own = if authors.is_empty() {
            "author IN (SELECT author FROM local_keys)".to_string()
        } else {
            let numbered: Vec<String> = (0..authors.len()).map(|i| format!("?{}", i + 4)).collect();
            format!(
                "(author IN ({}) OR author IN (SELECT author FROM local_keys))",
                numbered.join(",")
            )
        };
        let mut args: Vec<SqlValue> = vec![
            row.author.clone().into(),
            row.id.clone().into(),
            row.parent.clone().unwrap_or_default().into(),
        ];
        args.extend(authors.iter().map(|a| SqlValue::from((*a).clone())));
        let kept: bool = tx.query_row(
            &format!(
                "WITH RECURSIVE below(id, depth) AS (
                     SELECT ?2, 0
                     UNION ALL
                     SELECT e.id, b.depth + 1 FROM events e JOIN below b ON e.parent = b.id
                     WHERE b.depth < {ANCESTOR_DEPTH} LIMIT {SUBTREE_ROWS})
                 SELECT EXISTS (SELECT 1 FROM local_keys WHERE author = ?1)
                     OR EXISTS (SELECT 1 FROM ask_notify WHERE ask IN (?2, ?3))
                     OR EXISTS (SELECT 1 FROM events WHERE id = ?3 AND {own})
                     OR EXISTS (SELECT 1 FROM events WHERE thread = ?2 AND {own})
                     OR EXISTS (SELECT 1 FROM events WHERE id IN
                         (SELECT id FROM below WHERE depth > 0) AND {own})"
            ),
            params_from_iter(args),
            |r| r.get(0),
        )?;
        Ok(kept)
    }
}

/// How [`Store::delete_rows`] deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deletion {
    /// Stage 0: rows of a key over its share, strict protection only; the
    /// cut does not move.
    Trim,
    /// Stage 2: the oldest rows; the rotation cut rises.
    Cut,
}

/// What one rotation transaction did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rotated {
    /// Bodies dropped (stage 1) or rows deleted (stages 0 and 2).
    pub rows: usize,
    /// Blobs no kept post lists any more: to untag unless this node keeps
    /// them (also recorded in `unpin_pending`, in the same transaction).
    pub freed_blobs: Vec<String>,
    /// The cuts raised, per network (stage 2).
    pub cuts: Vec<(String, u64)>,
    /// Rows skipped because they could not be read or tombstoned (logged).
    pub skipped: usize,
}

/// A rotation run, as logged (ADR 0025 §7): what it actually dropped and
/// deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RotationLog {
    /// Unix seconds, when it ended.
    pub at: u64,
    /// Bodies dropped (stage 1).
    pub bodies: u64,
    /// Rows of keys over their share deleted (stage 0).
    pub trimmed: u64,
    /// Rows deleted below the new cut (stage 2).
    pub rows: u64,
    /// Store bytes before and after.
    pub before: u64,
    pub after: u64,
}

#[cfg(test)]
thread_local! {
    /// Test only: a rotation transaction fails after this many rows, before
    /// it commits (a crash halfway through).
    pub static FAIL_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn fail_point(done: usize) -> Result<()> {
    if FAIL_AFTER.with(|f| f.get()).is_some_and(|n| done >= n) {
        bail!("test: rotation interrupted");
    }
    Ok(())
}

#[cfg(not(test))]
fn fail_point(_done: usize) -> Result<()> {
    Ok(())
}

/// Record `hashes`, freed by a rotation, in `unpin_pending`.
fn pend_unpin(tx: &rusqlite::Transaction<'_>, hashes: &[String]) -> Result<()> {
    for hash in hashes {
        tx.execute(
            "INSERT OR IGNORE INTO unpin_pending (hash) VALUES (?)",
            [hash],
        )?;
    }
    Ok(())
}

/// Raise the rotation cut of `net` to `ts` (ADR 0025 §7, stage 2), unless
/// it is there already: one `cut` range of phase `rotation`, from 0 to
/// `ts`, apart from the fill's cut. The other ranges stay: kept-forever
/// kinds are served below the rotation cut, but not below the fill's cut
/// or a skipped range.
fn raise_cut_tx(tx: &rusqlite::Transaction<'_>, net: &str, ts: u64, now: u64) -> Result<bool> {
    let current: Option<(i64, i64)> = tx
        .query_row(
            "SELECT id, cursor_ts FROM sync_ranges WHERE net = ? AND state = 'cut' AND phase = ?
             ORDER BY cursor_ts DESC LIMIT 1",
            params![net, PHASE_ROTATION],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match current {
        Some((_, at)) if at >= sql_ts(ts) => Ok(false),
        Some((id, _)) => {
            tx.execute(
                "UPDATE sync_ranges SET hi_ts = ?1, cursor_ts = ?1, opened = ?2 WHERE id = ?3",
                params![sql_ts(ts), sql_ts(now), id],
            )?;
            Ok(true)
        }
        None => {
            tx.execute(
                "INSERT INTO sync_ranges
                     (net, lo_ts, hi_ts, cursor_ts, cursor_id, state, phase, opened, base_bytes)
                 VALUES (?1, 0, ?2, ?2, '', 'cut', ?3, ?4, 0)",
                params![net, sql_ts(ts), PHASE_ROTATION, sql_ts(now)],
            )?;
            Ok(true)
        }
    }
}

/// The phase of a cut that rotation raised (its `opened`: when).
pub const PHASE_ROTATION: &str = "rotation";

/// Why rotation kept a blob whose posts it rotated out (`blob_retained`, a
/// log, ADR 0025 PR B notes, "Blobs"): it cannot tell yet a replica from a
/// blob it only cached, so it keeps it until ADR 0029's replica record
/// decides.
pub const RETAIN_UNDECIDED: &str = "undecided";

impl Store {
    /// Bytes of the free pages of the database: freed by deletions, reused
    /// by later inserts. The file does not shrink (no `VACUUM`), so ADR
    /// 0012's measure leaves them out (ADR 0025 §7).
    pub fn free_bytes(&self) -> u64 {
        let conn = self.conn();
        let pragma = |name: &str| -> i64 {
            conn.pragma_query_value(None, name, |r| r.get(0))
                .unwrap_or(0)
        };
        (pragma("freelist_count").max(0) as u64).saturating_mul(pragma("page_size").max(0) as u64)
    }

    /// Record that `author` is the key of this node's profile `profile`:
    /// rotation keeps its posts, across restarts.
    pub fn note_local_key(&self, author: &str, profile: &str, now: u64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO local_keys (author, profile, since) VALUES (?1, ?2, ?3)
             ON CONFLICT (author) DO NOTHING",
            params![author, profile, sql_ts(now)],
        )?;
        Ok(())
    }

    /// The keys recorded by [`Store::note_local_key`].
    pub fn local_keys(&self) -> Result<HashSet<String>> {
        let conn = self.conn();
        if !has_table(&conn, "local_keys")? {
            return Ok(HashSet::new());
        }
        let mut stmt = conn.prepare_cached("SELECT author FROM local_keys")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<HashSet<_>>>()
            .map_err(Into::into)
    }

    /// What rotation keeps, for these profiles of this node (and those of
    /// `local_keys`): see [`Keep`].
    pub fn rotation_keep(&self, local: &HashSet<String>) -> Result<Keep> {
        let mut authors = self.local_keys()?;
        authors.extend(local.iter().cloned());
        let conn = self.conn();
        let mut parents: HashSet<String> = HashSet::new();
        // Walking up starts from the parents of this node's posts.
        let mut up: HashSet<String> = HashSet::new();
        let list: Vec<&String> = authors.iter().collect();
        // Thread roots kept even when a link up to them is missing.
        let mut roots: Vec<String> = Vec::new();
        for chunk in list.chunks(500) {
            let mut stmt = conn.prepare(&format!(
                "SELECT id, parent, thread FROM events WHERE author IN ({})",
                marks(chunk.len())
            ))?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })?;
            for row in rows {
                let (id, parent, thread) = row?;
                parents.insert(id);
                up.extend(parent);
                roots.extend(thread);
            }
        }
        let mut ids: HashSet<String> = HashSet::new();
        {
            let mut stmt = conn.prepare_cached("SELECT ask FROM ask_notify")?;
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                let ask = row?;
                ids.insert(ask.clone());
                parents.insert(ask);
            }
        }
        ids.extend(roots);
        // The ancestors, a level at a time.
        for _ in 0..ANCESTOR_DEPTH {
            let next: Vec<String> = up.drain().filter(|id| ids.insert(id.clone())).collect();
            if next.is_empty() {
                break;
            }
            for chunk in next.chunks(500) {
                let mut stmt = conn.prepare(&format!(
                    "SELECT parent FROM events WHERE parent IS NOT NULL AND id IN ({})",
                    marks(chunk.len())
                ))?;
                let rows =
                    stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?;
                for row in rows {
                    up.insert(row?);
                }
            }
        }
        drop(conn);
        Ok(Keep {
            authors,
            ids,
            parents,
        })
    }

    /// Bytes of the JSON of every row, summed in steps that each hold the
    /// lock briefly.
    pub fn json_bytes(&self) -> Result<u64> {
        let max = self.max_seq()?;
        let mut total = 0u64;
        let mut from = 0i64;
        while from < max {
            let to = from.saturating_add(20_000);
            let part: i64 = self.conn().query_row(
                "SELECT COALESCE(SUM(LENGTH(CAST(json AS BLOB))), 0) FROM events
                 WHERE seq > ? AND seq <= ?",
                params![from, to],
                |r| r.get(0),
            )?;
            total = total.saturating_add(part.max(0) as u64);
            from = to;
        }
        Ok(total)
    }

    /// The highest `seq` held, 0 for none.
    pub fn max_seq(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("SELECT COALESCE(max(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    /// Rows with `seq` in `(from, to]`, in `seq` order: a scan of the whole
    /// store in steps that each hold the lock briefly.
    pub fn rows_in_seq(&self, from: i64, to: i64) -> Result<Vec<RowHead>> {
        row_heads(
            &self.conn(),
            &format!(
                "SELECT {ROW_HEAD} FROM events e WHERE e.seq > ? AND e.seq <= ? ORDER BY e.seq"
            ),
            vec![from.into(), to.into()],
        )
    }

    /// The bodies stage 1 may drop among the rows with `seq` in `(from,
    /// to]` (ADR 0025 §7): full events written before `before_ts`, not
    /// `react`s, whose title, text and tags hold at least `min_body`
    /// bytes. The exact saving is checked later, against the tombstone.
    pub fn body_candidates(
        &self,
        from: i64,
        to: i64,
        before_ts: u64,
        min_body: u64,
    ) -> Result<Vec<RowHead>> {
        row_heads(
            &self.conn(),
            &format!(
                "SELECT {ROW_HEAD} FROM events e WHERE e.seq > ? AND e.seq <= ?
                     AND e.opaque = 0 AND e.ts < ? AND e.kind <> 'react'
                     AND LENGTH(CAST(e.title AS BLOB)) + LENGTH(CAST(e.text AS BLOB))
                         + LENGTH(CAST(e.tags AS BLOB)) >= ?
                 ORDER BY e.seq"
            ),
            vec![
                from.into(),
                to.into(),
                sql_ts(before_ts).into(),
                sql_ts(min_body).into(),
            ],
        )
    }

    /// Rows of `net` strictly after `(ts, id)`, oldest first, at most
    /// `limit`.
    pub fn net_rows_after(
        &self,
        net: &str,
        after: (u64, &str),
        limit: usize,
    ) -> Result<Vec<RowHead>> {
        row_heads(
            &self.conn(),
            &format!(
                "SELECT {ROW_HEAD} FROM events e WHERE e.net = ?1
                     AND (e.ts > ?2 OR (e.ts = ?2 AND e.id > ?3))
                 ORDER BY e.ts, e.id LIMIT ?4"
            ),
            vec![
                net.to_string().into(),
                sql_ts(after.0).into(),
                after.1.to_string().into(),
                (limit as i64).into(),
            ],
        )
    }

    /// Rows of `net` by `authors`, oldest first, strictly after `(ts, id)`,
    /// at most `limit`: one range of the index `events_net_author_ts` per
    /// author, merged.
    pub fn author_rows_after(
        &self,
        net: &str,
        authors: &[String],
        after: (u64, &str),
        limit: usize,
    ) -> Result<Vec<RowHead>> {
        let mut rows: Vec<RowHead> = Vec::new();
        let conn = self.conn();
        for author in authors {
            rows.extend(row_heads(
                &conn,
                &format!(
                    "SELECT {ROW_HEAD} FROM events e WHERE e.net = ?1 AND e.author = ?2
                         AND (e.ts > ?3 OR (e.ts = ?3 AND e.id > ?4))
                     ORDER BY e.ts, e.id LIMIT ?5"
                ),
                vec![
                    net.to_string().into(),
                    author.clone().into(),
                    sql_ts(after.0).into(),
                    after.1.to_string().into(),
                    (limit as i64).into(),
                ],
            )?);
        }
        rows.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
        rows.truncate(limit);
        Ok(rows)
    }

    /// The full, understood events among these `seq`s.
    pub fn full_events_by_seq(&self, seqs: &[i64]) -> Result<Vec<Stored>> {
        if seqs.is_empty() {
            return Ok(Vec::new());
        }
        self.query(
            &format!(
                "SELECT seq, json FROM events WHERE opaque = 0 AND seq IN ({})",
                marks(seqs.len())
            ),
            seqs.iter().map(|s| SqlValue::from(*s)).collect(),
        )
    }

    /// The networks this store holds events of.
    pub fn nets_held(&self) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare_cached("SELECT DISTINCT net FROM events")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The cut of `net`, the fill's or rotation's, whichever is higher:
    /// below it, this node holds no history on purpose but kept-forever
    /// kinds and fetches on demand (ADR 0025 §1).
    pub fn cut_ts(&self, net: &str) -> Result<Option<u64>> {
        let ts: Option<i64> = self.conn().query_row(
            "SELECT max(cursor_ts) FROM sync_ranges WHERE net = ? AND state = 'cut'",
            [net],
            |r| r.get(0),
        )?;
        Ok(ts.map(|t| t.max(0) as u64))
    }

    /// Drop these bodies, reason `evicted` (ADR 0025 §7, stage 1), in one
    /// transaction. Each is checked again inside it: still a full event,
    /// with no drop record (a stronger reason always wins), listing no blob
    /// (those go through the node's eviction and ADR 0029's last-copy
    /// rule: [`Store::lists_blobs`]), not protected ([`Keep`]), not a
    /// `react` (a signal's meaning is in its body), and saving at least
    /// [`EVICT_MIN_SAVING`] bytes.
    pub fn evict_bodies(&self, ids: &[String], keep: &Keep, now: u64) -> Result<Rotated> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut out = Rotated::default();
        let mut hashes: Vec<String> = Vec::new();
        for id in ids {
            fail_point(out.rows)?;
            let row: Option<(RowHead, String)> = tx
                .query_row(
                    &format!("SELECT {ROW_HEAD}, e.json FROM events e WHERE e.id = ?"),
                    [id],
                    |r| Ok((row_head(r)?, r.get(11)?)),
                )
                .optional()?;
            let Some((head, json)) = row else { continue };
            if head.opaque
                || head.kind == Kind::React.as_str()
                || has_blobs_or_record(&tx, &head)?
                || keep.protects_now(&tx, &head, false)?
            {
                continue;
            }
            let tombstone = serde_json::from_str::<Value>(&json)
                .map_err(anyhow::Error::from)
                .and_then(|full| Ok(crate::wire::tombstone(&full)?))
                .and_then(|t| Ok(serde_json::to_string(&t.wire)?));
            let tombstone = match tombstone {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(%id, "rotation: a row not tombstoned, skipped: {e:#}");
                    out.skipped += 1;
                    continue;
                }
            };
            if (json.len() as u64) < (tombstone.len() as u64).saturating_add(EVICT_MIN_SAVING) {
                continue;
            }
            let outcome = drop_body_inner(&tx, id, DropReason::Evicted, None, None, now, true)?;
            if outcome.body_dropped {
                out.rows += 1;
            }
            for hash in outcome.listed {
                if !hashes.contains(&hash) {
                    hashes.push(hash);
                }
            }
        }
        let kept = referenced_among(&tx, &hashes)?;
        out.freed_blobs = hashes.into_iter().filter(|h| !kept.contains(h)).collect();
        pend_unpin(&tx, &out.freed_blobs)?;
        if out.rows > 0 {
            set_meta(&tx, "history_lossy", 1)?;
        }
        tx.commit()?;
        Ok(out)
    }

    /// Delete these rows, header included (ADR 0025 §7, stages 0 and 2), in
    /// one transaction. Each is checked again inside it: not protected
    /// ([`Keep`]; strictly only, for [`Deletion::Trim`]). What they said
    /// about their key's age is kept (`key_first`). For [`Deletion::Cut`],
    /// the rotation cut of each network rises to the newest row deleted
    /// there, in the same transaction, so that a crash never leaves rows
    /// gone above the cut this node advertises.
    pub fn delete_rows(
        &self,
        ids: &[String],
        keep: &Keep,
        how: Deletion,
        now: u64,
    ) -> Result<Rotated> {
        let cut = how == Deletion::Cut;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut out = Rotated::default();
        let mut newest: HashMap<String, u64> = HashMap::new();
        let mut hashes: Vec<String> = Vec::new();
        let mut uncounted = 0u64;
        let quota = quota_sql("e");
        for id in ids {
            fail_point(out.rows)?;
            #[allow(clippy::type_complexity)]
            let row: Option<(RowHead, u64, String, String, String, String, bool)> = tx
                .query_row(
                    &format!(
                        "SELECT {ROW_HEAD}, e.received_at, e.title, e.text, e.tags, e.json,
                             {quota}
                         FROM events e WHERE e.id = ?"
                    ),
                    [id],
                    |r| {
                        Ok((
                            row_head(r)?,
                            r.get::<_, i64>(11)?.max(0) as u64,
                            r.get(12)?,
                            r.get(13)?,
                            r.get(14)?,
                            r.get(15)?,
                            r.get(16)?,
                        ))
                    },
                )
                .optional()?;
            let Some((head, received_at, title, text, tags, json, counted)) = row else {
                continue;
            };
            // A full event listing blobs is evicted first by the node, under
            // ADR 0029's last-copy rule; a tombstone whose eviction kept
            // replicas stays, so that their record stays reviewable.
            if keep.protects_now(&tx, &head, !cut)? || has_blobs_or_replicas(&tx, &head)? {
                continue;
            }
            if head.opaque {
                tx.execute(
                    "INSERT INTO events_fts (events_fts, rowid, title, text, tags)
                     VALUES ('delete', ?, '', '', '')",
                    [head.seq],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO events_fts (events_fts, rowid, title, text, tags)
                     VALUES ('delete', ?, ?, ?, ?)",
                    params![head.seq, title, text, tags],
                )?;
                match serde_json::from_str::<Value>(&json) {
                    Ok(full) => {
                        for hash in blob_hashes(&full) {
                            if !hashes.contains(&hash) {
                                hashes.push(hash);
                            }
                        }
                    }
                    // Unreadable: it may list blobs; kept, and counted.
                    Err(e) => {
                        tracing::warn!(id = %head.id, "rotation: a row not read, kept: {e:#}");
                        out.skipped += 1;
                        continue;
                    }
                }
            }
            tx.execute("DELETE FROM event_tags WHERE seq = ?", [head.seq])?;
            tx.execute("DELETE FROM event_refs WHERE seq = ?", [head.seq])?;
            tx.execute("DELETE FROM event_blobs WHERE seq = ?", [head.seq])?;
            tx.execute("DELETE FROM ask_notify WHERE ask = ?", [&head.id])?;
            // An evicted body's record goes with its row; the others stay,
            // so that a body retracted or erased here is still refused
            // when it comes back (ADR 0020 §2).
            tx.execute(
                "DELETE FROM dropped WHERE id = ? AND reason = ?",
                params![head.id, DropReason::Evicted.as_str()],
            )?;
            tx.execute("DELETE FROM events WHERE seq = ?", [head.seq])?;
            tx.execute(
                "INSERT INTO key_first (author, net, first_seen, first_ts) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (author, net) DO UPDATE SET
                     first_seen = min(first_seen, excluded.first_seen),
                     first_ts = min(first_ts, excluded.first_ts)",
                params![head.author, head.net, sql_ts(received_at), sql_ts(head.ts)],
            )?;
            if counted {
                uncounted = uncounted.saturating_add(head.len);
            }
            let top = newest.entry(head.net.clone()).or_insert(head.ts);
            *top = (*top).max(head.ts);
            out.rows += 1;
        }
        let kept = referenced_among(&tx, &hashes)?;
        out.freed_blobs = hashes.into_iter().filter(|h| !kept.contains(h)).collect();
        pend_unpin(&tx, &out.freed_blobs)?;
        if cut {
            let mut nets: Vec<(String, u64)> = newest.into_iter().collect();
            nets.sort();
            for (net, ts) in nets {
                if raise_cut_tx(&tx, &net, ts, now)? {
                    out.cuts.push((net, ts));
                }
            }
        }
        let quota = self
            .opaque_bytes
            .load(Ordering::Relaxed)
            .saturating_sub(uncounted);
        if uncounted > 0 {
            persist_quota(&tx, quota)?;
        }
        if out.rows > 0 {
            set_meta(&tx, "history_lossy", 1)?;
        }
        tx.commit()?;
        self.opaque_bytes.store(quota, Ordering::Relaxed);
        Ok(out)
    }

    /// Of `ids`, those held in full that list a blob (`event_blobs`):
    /// rotation evicts them through the node, under ADR 0029's last-copy
    /// rule, before it deletes their rows.
    pub fn lists_blobs(&self, ids: &[String]) -> Result<HashSet<String>> {
        let conn = self.conn();
        let mut out = HashSet::new();
        for chunk in ids.chunks(500) {
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT e.id FROM events e JOIN event_blobs b ON b.seq = e.seq
                 WHERE e.opaque = 0 AND e.id IN ({})",
                marks(chunk.len())
            ))?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?;
            for row in rows {
                out.insert(row?);
            }
        }
        Ok(out)
    }

    /// Whether `id` may still be rotated out: held, not protected
    /// ([`Keep`]; strictly only, with `strictly`), with no drop record of
    /// a stronger reason. Checked just before the node evicts it.
    pub fn rotatable(&self, id: &str, keep: &Keep, strictly: bool) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let head: Option<RowHead> = tx
            .query_row(
                &format!("SELECT {ROW_HEAD} FROM events e WHERE e.id = ?"),
                [id],
                row_head,
            )
            .optional()?;
        let Some(head) = head else {
            return Ok(false);
        };
        let stronger: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM dropped WHERE id = ?1 AND reason <> ?2)",
            params![id, DropReason::Evicted.as_str()],
            |r| r.get(0),
        )?;
        Ok(!stronger && !keep.protects_now(&tx, &head, strictly)?)
    }

    /// Whether a kept full event lists blob `hash`.
    pub fn blob_referenced(&self, hash: &str) -> Result<bool> {
        blob_referenced(&self.conn(), hash)
    }

    /// The blobs rotation freed and that are not settled yet, each with
    /// whether a kept full event lists it again, and why this node keeps
    /// it, if it does (`blob_retained`).
    pub fn unpin_pending(&self) -> Result<Vec<(String, bool, Option<String>)>> {
        let conn = self.conn();
        if !has_table(&conn, "unpin_pending")? {
            return Ok(Vec::new());
        }
        let hashes: Vec<String> = {
            let mut stmt = conn.prepare_cached("SELECT hash FROM unpin_pending")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let listed = referenced_among(&conn, &hashes)?;
        let mut out = Vec::new();
        for hash in hashes {
            let reason: Option<String> = conn
                .query_row(
                    "SELECT reason FROM blob_retained WHERE hash = ?",
                    [&hash],
                    |r| r.get(0),
                )
                .optional()?;
            let listed = listed.contains(&hash);
            out.push((hash, listed, reason));
        }
        Ok(out)
    }

    /// Settle blobs of `unpin_pending`, in one transaction: these are kept,
    /// for these reasons (recorded in `blob_retained` if not yet), or
    /// listed again, or untagged.
    pub fn settle_unpin(&self, kept: &[(String, String)], done: &[String], now: u64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for (hash, reason) in kept {
            tx.execute(
                "INSERT INTO blob_retained (hash, reason, at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (hash) DO NOTHING",
                params![hash, reason, sql_ts(now)],
            )?;
            tx.execute("DELETE FROM unpin_pending WHERE hash = ?", [hash])?;
        }
        for hash in done {
            tx.execute("DELETE FROM unpin_pending WHERE hash = ?", [hash])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Log that rotation kept blob `hash` for `reason` (`blob_retained`,
    /// ADR 0025 PR B notes, "Blobs"). A first reason stays.
    pub fn retain_blob(&self, hash: &str, reason: &str, now: u64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO blob_retained (hash, reason, at) VALUES (?1, ?2, ?3)
             ON CONFLICT (hash) DO NOTHING",
            params![hash, reason, sql_ts(now)],
        )?;
        Ok(())
    }

    /// Why this node keeps blob `hash`, if it recorded a reason.
    pub fn blob_retention(&self, hash: &str) -> Result<Option<String>> {
        let conn = self.conn();
        if !has_table(&conn, "blob_retained")? {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                "SELECT reason FROM blob_retained WHERE hash = ?",
                [hash],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Every blob this node keeps for a recorded reason: `(hash, reason)`.
    pub fn retained_blobs(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn();
        if !has_table(&conn, "blob_retained")? {
            return Ok(Vec::new());
        }
        let mut stmt =
            conn.prepare_cached("SELECT hash, reason FROM blob_retained ORDER BY hash")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// When rotation deleted the oldest rows of each key of `net`: author
    /// to `(first_seen, first_ts)`, what its age counts from (ADR 0025 §7).
    pub fn key_firsts(&self, net: &str) -> Result<HashMap<String, (u64, u64)>> {
        let conn = self.conn();
        if !has_table(&conn, "key_first")? {
            return Ok(HashMap::new());
        }
        let mut stmt = conn
            .prepare_cached("SELECT author, first_seen, first_ts FROM key_first WHERE net = ?")?;
        let rows = stmt.query_map([net], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, i64>(1)?.max(0) as u64,
                    r.get::<_, i64>(2)?.max(0) as u64,
                ),
            ))
        })?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>()
            .map_err(Into::into)
    }

    /// Log a rotation run; the last [`ROTATION_LOG_KEEP`] are kept.
    pub fn log_rotation(&self, run: &RotationLog) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO rotation_log (at, bodies, trimmed, rows, before, after)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                sql_ts(run.at),
                sql_ts(run.bodies),
                sql_ts(run.trimmed),
                sql_ts(run.rows),
                sql_ts(run.before),
                sql_ts(run.after)
            ],
        )?;
        conn.execute(
            "DELETE FROM rotation_log WHERE rowid NOT IN
                 (SELECT rowid FROM rotation_log ORDER BY rowid DESC LIMIT ?)",
            [ROTATION_LOG_KEEP],
        )?;
        Ok(())
    }

    /// The rotation runs logged, oldest first (the last 100).
    pub fn rotation_runs(&self) -> Result<Vec<RotationLog>> {
        let conn = self.conn();
        if !has_table(&conn, "rotation_log")? {
            return Ok(Vec::new());
        }
        let n = |r: &rusqlite::Row<'_>, i: usize| r.get::<_, i64>(i).map(|v| v.max(0) as u64);
        let mut stmt = conn.prepare_cached(
            "SELECT at, bodies, trimmed, rows, before, after FROM rotation_log ORDER BY rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(RotationLog {
                at: n(r, 0)?,
                bodies: n(r, 1)?,
                trimmed: n(r, 2)?,
                rows: n(r, 3)?,
                before: n(r, 4)?,
                after: n(r, 5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The last rotation run, if any.
    pub fn last_rotation(&self) -> Result<Option<RotationLog>> {
        let conn = self.conn();
        if !has_table(&conn, "rotation_log")? {
            return Ok(None);
        }
        let n = |r: &rusqlite::Row<'_>, i: usize| r.get::<_, i64>(i).map(|v| v.max(0) as u64);
        Ok(conn
            .query_row(
                "SELECT at, bodies, trimmed, rows, before, after FROM rotation_log
                 ORDER BY rowid DESC LIMIT 1",
                [],
                |r| {
                    Ok(RotationLog {
                        at: n(r, 0)?,
                        bodies: n(r, 1)?,
                        trimmed: n(r, 2)?,
                        rows: n(r, 3)?,
                        before: n(r, 4)?,
                        after: n(r, 5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Whether rotation ever dropped a body or deleted a row here: this
    /// node no longer holds the complete history, and does not say so.
    pub fn history_lossy(&self) -> Result<bool> {
        Ok(meta(&self.conn(), "history_lossy")?.unwrap_or(0) != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Draft;
    use ed25519_dalek::SigningKey;
    use serde_json::json;

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

    fn react(parent: &Event, key: u8, ts: u64, title: &str, data: serde_json::Value) -> Event {
        Draft {
            net: parent.net.clone(),
            thread: Some(parent.thread_root().to_string()),
            parent: Some(parent.id.clone()),
            kind: Some(Kind::React),
            title: (!title.is_empty()).then(|| title.to_string()),
            text: "react".into(),
            data: data.as_object().cloned().unwrap_or_default(),
            ..Draft::default()
        }
        .sign(&SigningKey::from_bytes(&[key; 32]), ts, 0)
        .unwrap()
    }

    /// 0.5.3: what others said of a node's posts, each key's latest react
    /// once, its own reacts left out, by when they arrived.
    #[test]
    fn signals_received_count_each_keys_latest_react() {
        let store = Store::open_in_memory().unwrap();
        let ask = post(9, 10, Kind::Ask, &[], "How to vacuum?");
        let mine = reply(&ask, 1, 11, "Run VACUUM.");
        let me = mine.author.clone();
        store.insert(&ask, 10).unwrap();
        store.insert(&mine, 11).unwrap();
        // Key 2 votes +1, then marks it used: counts once, as used.
        store
            .insert(&react(&mine, 2, 20, "", json!({"value": 1})), 20)
            .unwrap();
        store
            .insert(&react(&mine, 2, 21, "", json!({"used": true})), 21)
            .unwrap();
        store
            .insert(&react(&mine, 3, 22, "", json!({"value": 1})), 22)
            .unwrap();
        store
            .insert(
                &react(&mine, 4, 23, "fails on postgres 16", json!({"value": -1})),
                23,
            )
            .unwrap();
        store
            .insert(&react(&mine, 5, 24, "", json!({"value": -1})), 24)
            .unwrap();
        store
            .insert(&react(&mine, 9, 25, "", json!({"accept": true})), 500)
            .unwrap();
        // Our own react does not count.
        store
            .insert(&react(&mine, 1, 26, "", json!({"value": 1})), 26)
            .unwrap();
        let all = store
            .signals_received(std::slice::from_ref(&me), 0)
            .unwrap();
        assert_eq!(
            all,
            Received {
                useful: 1,
                used: 1,
                accepted: 1,
                confirmed: 0,
                challenged: 1,
                down: 1,
            }
        );
        let late = store
            .signals_received(std::slice::from_ref(&me), 100)
            .unwrap();
        assert_eq!(
            late,
            Received {
                accepted: 1,
                ..Received::default()
            }
        );
        assert!(store.signals_received(&[], 0).unwrap().is_empty());
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
    fn embeddings_follow_their_bodies() {
        let store = Store::open_in_memory().unwrap();
        let root = post(1, 10, Kind::Ask, &[], "how do I sort a list");
        let answer = reply(&root, 2, 11, "use sort()");
        let note = post(3, 12, Kind::Note, &[], "not embedded");
        for (e, at) in [(&root, 10), (&answer, 11), (&note, 12)] {
            store.insert(e, at).unwrap();
        }
        let seq = |e: &Event| store.get(&e.id).unwrap().unwrap().seq;
        let (root_seq, answer_seq) = (seq(&root), seq(&answer));
        // Newest first; the note is not a kind semantic search embeds.
        let todo = store
            .embedding_todo("m1", None, Some(i64::MAX), 10)
            .unwrap();
        let seqs: Vec<i64> = todo.iter().map(|t| t.0).collect();
        assert_eq!(seqs, [answer_seq, root_seq]);
        assert_eq!(todo[1].2, "how do I sort a list");
        // New posts: oldest first, above a seq.
        let todo = store
            .embedding_todo("m1", Some(root_seq), None, 10)
            .unwrap();
        assert_eq!(todo.len(), 1);
        assert!(store.put_embedding(root_seq, "m1", &[1, 2]).unwrap());
        assert!(store.put_embedding(answer_seq, "m1", &[3, 4]).unwrap());
        assert!(
            store
                .embedding_todo("m1", None, Some(i64::MAX), 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.embedding_counts("m1").unwrap(), (2, 2));
        assert_eq!(store.embeddings_extent("m1").unwrap(), (2, answer_seq));
        assert_eq!(
            store.embeddings_after("m1", root_seq).unwrap(),
            [(answer_seq, vec![3, 4])]
        );
        // Another model's vectors do not count, and are dropped on demand.
        assert_eq!(
            store
                .embedding_todo("m2", None, Some(i64::MAX), 10)
                .unwrap()
                .len(),
            2
        );
        // A dropped body takes its vector along, and gets no new one.
        store
            .drop_body(&root.id, DropReason::Own, None, None, 100)
            .unwrap();
        assert_eq!(store.embeddings_extent("m1").unwrap(), (1, answer_seq));
        assert!(!store.put_embedding(root_seq, "m1", &[1, 2]).unwrap());
        assert_eq!(store.embedding_counts("m1").unwrap(), (1, 1));
        // One vector per post and model: m2's beside m1's, until m1's go.
        assert!(store.put_embedding(answer_seq, "m2", &[5]).unwrap());
        assert_eq!(store.embeddings_extent("m1").unwrap().0, 1);
        assert_eq!(store.drop_other_embeddings("m2").unwrap(), 1);
        assert_eq!(store.embeddings_extent("m1").unwrap().0, 0);
        assert_eq!(store.embeddings_extent("m2").unwrap().0, 1);
        // The state survives.
        assert_eq!(store.semantic_state("below").unwrap(), None);
        store.set_semantic_state("below", 7).unwrap();
        store.set_semantic_state("below", 5).unwrap();
        assert_eq!(store.semantic_state("below").unwrap(), Some(5));
    }

    /// A 0.5.0/0.5.1 store (vectors keyed by `seq` alone) moves to the
    /// `(seq, model)` key, its vectors kept, and its triggers still drop a
    /// vector with its body.
    #[test]
    fn the_embeddings_table_moves_to_a_per_model_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let root = post(1, 10, Kind::Ask, &[], "how do I sort a list");
        {
            let store = Store::open(&path).unwrap();
            store.insert(&root, 10).unwrap();
            // As 0.5.1 made it.
            store
                .conn()
                .execute_batch(
                    "DROP TRIGGER embeddings_event_delete; DROP TRIGGER embeddings_body_change;
                     DROP TABLE embeddings;
                     CREATE TABLE embeddings (seq INTEGER PRIMARY KEY, model TEXT NOT NULL, vec BLOB NOT NULL);
                     CREATE TRIGGER embeddings_body_change AFTER UPDATE OF json, text, title, opaque
                         ON events BEGIN DELETE FROM embeddings WHERE seq = old.seq; END;",
                )
                .unwrap();
            let seq = store.get(&root.id).unwrap().unwrap().seq;
            assert!(store.put_embedding(seq, "m1", &[1]).unwrap());
        }
        let store = Store::open(&path).unwrap();
        let seq = store.get(&root.id).unwrap().unwrap().seq;
        assert_eq!(store.embeddings_extent("m1").unwrap(), (1, seq));
        assert!(store.put_embedding(seq, "m2", &[2]).unwrap());
        assert_eq!(store.embeddings_extent("m1").unwrap().0, 1);
        store
            .drop_body(&root.id, DropReason::Own, None, None, 100)
            .unwrap();
        assert_eq!(store.embeddings_extent("m1").unwrap().0, 0);
        assert_eq!(store.embeddings_extent("m2").unwrap().0, 0);
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
        assert_eq!(
            store
                .tag_counts(&Facets::default(), None, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .thread_summaries(&Facets::default(), None, 0, 10)
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
        assert!(
            store
                .tag_counts(&Facets::default(), None, 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .thread_summaries(&Facets::default(), None, 0, 10)
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
        // A tombstone is not opaque content (ADR 0025 §7).
        assert_eq!(store.opaque_bytes(), before);
        assert_eq!(store.insert(&event, 11).unwrap(), Inserted::Dropped);
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(found(&store, "here"), 0);
    }

    #[test]
    fn an_evicted_body_comes_back_on_read_only_and_its_record_expires() {
        let store = Store::open_in_memory().unwrap();
        let event = post(1, 10, Kind::Note, &["cap:pdf"], "evict me");
        store.insert(&event, 10).unwrap();
        store
            .drop_body(&event.id, DropReason::Evicted, None, None, 100)
            .unwrap();
        assert!(store.get(&event.id).unwrap().is_none());
        // Gossip, a sync page or the fill bring it back: refused, the
        // tombstone stays (ADR 0025 PR B review).
        assert_eq!(store.insert(&event, 110).unwrap(), Inserted::Dropped);
        assert!(store.get(&event.id).unwrap().is_none());
        // A `read` fetches it: taken back.
        assert!(matches!(
            store.insert_with(&event, 120, true).unwrap(),
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
    fn event_blobs_follow_inserts_drops_and_older_releases() {
        let store = Store::open_in_memory().unwrap();
        let (a, b) = ("aa".repeat(32), "bb".repeat(32));
        let first = with_blobs(1, 10, "two files", &[&a, &b]);
        let second = with_blobs(2, 11, "one of them", &[&a]);
        store.insert(&first, 10).unwrap();
        store.insert(&second, 11).unwrap();
        let listed = store.blob_listings(8).unwrap();
        assert_eq!(
            listed,
            vec![
                (a.clone(), vec![first.id.clone(), second.id.clone()]),
                (b.clone(), vec![first.id.clone()]),
            ]
        );
        assert_eq!(store.events_listing_blob(&a, 8).unwrap().len(), 2);
        assert_eq!(store.blobs_freed_by(&first.id).unwrap(), vec![b.clone()]);
        assert!(store.blob_listed(&b).unwrap());
        store
            .drop_body(&first.id, DropReason::Own, None, None, 20)
            .unwrap();
        assert!(!store.blob_listed(&b).unwrap(), "a tombstone lists nothing");
        assert_eq!(store.blobs_freed_by(&second.id).unwrap(), vec![a.clone()]);
        // Inserted by an older release, which keeps no index: filled at
        // the next open.
        let third = with_blobs(3, 12, "inserted by 0.4.2", &[&b]);
        store.insert(&third, 12).unwrap();
        let conn = store.conn.into_inner().unwrap();
        conn.execute_batch("DELETE FROM event_blobs WHERE hash = 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb';
                            UPDATE event_blobs_filled SET upto = 2;")
            .unwrap();
        let store = Store::init(conn).unwrap();
        assert!(store.blob_listed(&b).unwrap(), "backfilled");
    }

    #[test]
    fn an_eviction_never_overrides_an_erasure() {
        let store = Store::open_in_memory().unwrap();
        let hash = "dd".repeat(32);
        let event = with_blobs(1, 10, "erased while an eviction was planned", &[&hash]);
        store.insert(&event, 10).unwrap();
        // The erasure lands first; the eviction planned before it comes after.
        store
            .drop_body(&event.id, DropReason::Erasure, Some("label"), None, 20)
            .unwrap();
        let late = store
            .drop_body_keeping(
                &event.id,
                DropReason::Evicted,
                None,
                None,
                30,
                &[(hash.clone(), 3)],
            )
            .unwrap();
        assert!(late.freed_blobs.is_empty());
        let record = store.drop_record(&event.id).unwrap().unwrap();
        assert_eq!(record.reason, DropReason::Erasure, "the erasure wins");
        assert_eq!(record.trigger.as_deref(), Some("label"));
        assert!(
            store.evicted_replicas(&event.id).unwrap().is_empty(),
            "no record"
        );
    }

    #[test]
    fn a_replica_kept_through_eviction_is_freed_by_a_later_drop() {
        let store = Store::open_in_memory().unwrap();
        let hash = "cc".repeat(32);
        let event = with_blobs(1, 10, "a replica", &[&hash]);
        store.insert(&event, 10).unwrap();
        let evicted = store
            .drop_body(&event.id, DropReason::Evicted, None, None, 20)
            .unwrap();
        assert_eq!(evicted.freed_blobs, vec![hash.clone()]);
        // The node kept it: recorded.
        store
            .record_evicted_replicas(&event.id, &[(hash.clone(), 3)])
            .unwrap();
        assert_eq!(
            store.evicted_replicas(&event.id).unwrap(),
            vec![hash.clone()]
        );
        let erased = store
            .drop_body(&event.id, DropReason::Erasure, None, None, 30)
            .unwrap();
        assert!(!erased.body_dropped, "already a tombstone");
        assert_eq!(erased.freed_blobs, vec![hash], "released");
        assert!(store.evicted_replicas(&event.id).unwrap().is_empty());
        assert_eq!(
            store.drop_record(&event.id).unwrap().unwrap().reason,
            DropReason::Erasure
        );
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
    fn titles_are_searched_and_migrated_from_schema_v6() {
        let titled = |key: u8, title: &str, text: &str| {
            Draft {
                net: "public".into(),
                kind: Some(Kind::Note),
                title: Some(title.into()),
                text: text.into(),
                ..Draft::default()
            }
            .sign(&SigningKey::from_bytes(&[key; 32]), 10, 0)
            .unwrap()
        };
        let store = Store::open_in_memory().unwrap();
        let kept = titled(1, "musl needs musl-tools", "install the package");
        let dropped = titled(2, "a leaked phrase", "and its text");
        store.insert(&kept, 10).unwrap();
        store.insert(&dropped, 10).unwrap();
        assert_eq!(found(&store, "musl"), 1);
        // A dropped body leaves no title behind.
        store
            .drop_body(&dropped.id, DropReason::Own, None, None, 20)
            .unwrap();
        assert_eq!(found(&store, "leaked"), 0);
        // A schema 6 store: no title column, the old full-text table.
        let conn = store.conn.into_inner().unwrap();
        conn.execute_batch(
            "DROP TRIGGER events_fts_insert; DROP TABLE events_fts;
             ALTER TABLE events DROP COLUMN title;
             CREATE VIRTUAL TABLE events_fts USING fts5 (
                 text, tags, content = 'events', content_rowid = 'seq');
             INSERT INTO events_fts (events_fts) VALUES ('rebuild');
             PRAGMA user_version = 6;",
        )
        .unwrap();
        let store = Store::init(conn).unwrap();
        assert_eq!(found(&store, "musl"), 1, "titles indexed on migration");
        assert_eq!(found(&store, "package"), 1);
        assert_eq!(found(&store, "leaked"), 0);
        store
            .conn()
            .execute(
                "INSERT INTO events_fts (events_fts) VALUES ('integrity-check')",
                [],
            )
            .unwrap();
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
    fn range(lo: u64, hi: u64, phase: &str) -> SyncRange {
        SyncRange {
            id: 0,
            net: "public".into(),
            lo_ts: lo,
            hi_ts: hi,
            cursor_ts: hi,
            cursor_id: String::new(),
            state: RangeState::Pending,
            phase: phase.into(),
            opened: hi.saturating_sub(600),
            base_bytes: 0,
        }
    }

    #[test]
    fn descending_pages_with_filters() {
        let store = Store::open_in_memory().unwrap();
        let events: Vec<Event> = (0..10u8)
            .map(|i| {
                let kind = if i % 3 == 0 {
                    Kind::Announce
                } else {
                    Kind::Note
                };
                post(i % 2 + 1, 100 + u64::from(i), kind, &[], &format!("e{i}"))
            })
            .collect();
        for (i, e) in events.iter().enumerate() {
            store.insert(e, 1000 + i as u64).unwrap();
        }
        let ts = |rows: &[SeenRow]| -> Vec<u64> {
            rows.iter()
                .map(|(v, _)| v["ts"].as_u64().unwrap())
                .collect()
        };
        let mut q = PageQuery {
            net: "public".into(),
            before: Some((108, String::new())),
            limit: 3,
            ..PageQuery::default()
        };
        let page = store.sync_page(&q).unwrap();
        assert_eq!(ts(&page), [107, 106, 105]);
        // Each event carries when this node stored it.
        assert_eq!(page[0].1, 1007);
        // A lower bound with no id keeps its own second: a range's `lo`.
        q.after = (104, String::new());
        q.limit = 10;
        assert_eq!(ts(&store.sync_page(&q).unwrap()), [107, 106, 105, 104]);
        q.after = (0, String::new());
        q.above_ts = Some(102);
        assert_eq!(ts(&store.sync_page(&q).unwrap()), [107, 106, 105, 104, 103]);
        q.above_ts = None;
        q.kinds = vec!["announce".into()];
        assert_eq!(ts(&store.sync_page(&q).unwrap()), [106, 103, 100]);
        q.authors = vec![events[0].author.clone()];
        assert_eq!(ts(&store.sync_page(&q).unwrap()), [106, 100]);
        // Ascending, as 0.4.0 serves it.
        let up = PageQuery {
            net: "public".into(),
            after: (107, String::new()),
            limit: 10,
            ..PageQuery::default()
        };
        assert_eq!(ts(&store.sync_page(&up).unwrap()), [107, 108, 109]);
    }

    #[test]
    fn ranges_hold_their_holes_and_move_the_watermark_last() {
        let store = Store::open_in_memory().unwrap();
        store.set_sync_watermark("public", 1_000).unwrap();
        // An interrupted catch-up: its cursor half-way down.
        store.add_sync_range(&range(400, 2_000, "")).unwrap();
        let mut walked = store.sync_ranges("public").unwrap().remove(0);
        (walked.cursor_ts, walked.cursor_id) = (1_500, "ab".into());
        store.update_sync_range(&walked).unwrap();
        assert_eq!(walked.hole_top(), Some(1_500));
        assert_eq!(walked.hole_lo(), 400);
        // A newer one above it, completed first: the watermark waits.
        store.add_sync_range(&range(1_400, 3_000, "")).unwrap();
        let mut newer = store.sync_ranges("public").unwrap().remove(0);
        assert_eq!(newer.hi_ts, 3_000, "newest first");
        newer.state = RangeState::Done;
        store.update_sync_range(&newer).unwrap();
        assert_eq!(newer.hole_top(), None);
        assert_eq!(store.settle_sync_ranges("public").unwrap(), None);
        assert_eq!(store.sync_watermark("public").unwrap(), 1_000);
        // The old one resumes and completes: the newest opening wins.
        walked.state = RangeState::Done;
        store.update_sync_range(&walked).unwrap();
        assert_eq!(store.settle_sync_ranges("public").unwrap(), Some(2_400));
        assert_eq!(store.sync_watermark("public").unwrap(), 2_400);
        assert!(store.sync_ranges("public").unwrap().is_empty());
        // A cut is a hole from 0; a skipped range one of its own; both stay.
        let mut cut = range(0, 500, "");
        cut.state = RangeState::Cut;
        store.add_sync_range(&cut).unwrap();
        let mut skipped = range(2_000, 2_200, "");
        skipped.state = RangeState::Skipped;
        skipped.opened = 2_600;
        store.add_sync_range(&skipped).unwrap();
        let holes: Vec<(u64, Option<u64>)> = store
            .sync_ranges("public")
            .unwrap()
            .iter()
            .map(|r| (r.hole_lo(), r.hole_top()))
            .collect();
        assert_eq!(holes, [(2_000, Some(2_200)), (0, Some(500))]);
        assert_eq!(store.settle_sync_ranges("public").unwrap(), Some(2_600));
        assert_eq!(store.sync_ranges("public").unwrap().len(), 2);
        // The initial fill before its window holds nothing below its top.
        let fill = range(0, 9_000, "keep");
        assert_eq!(fill.hole_top(), Some(9_000));
        let mut window = fill.clone();
        (window.phase, window.cursor_ts) = (PHASE_WINDOW.into(), 7_000);
        assert_eq!(window.hole_top(), Some(7_000));
    }

    #[test]
    fn history_tables_need_no_schema_bump() {
        let store = Store::open_in_memory().unwrap();
        let event = post(1, 10, Kind::Note, &[], "kept across the migration");
        store.insert(&event, 10).unwrap();
        store.set_sync_watermark("public", 77).unwrap();
        let conn = store.conn.into_inner().unwrap();
        conn.execute_batch("DROP TABLE sync_ranges; DROP TABLE key_seen; DROP TABLE seen_peers;")
            .unwrap();
        let store = Store::init(conn).unwrap();
        let version: i64 = store
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 7, "a store 0.4.0 still opens");
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(
            store.sync_watermark("public").unwrap(),
            77,
            "its meaning kept"
        );
        assert!(store.sync_ranges("public").unwrap().is_empty());
        store.add_sync_range(&range(0, 100, "keep")).unwrap();
        assert!(store.store_bytes() > 0);
    }

    #[test]
    fn seen_reports_keep_each_peers_earliest_and_whether_it_is_full() {
        let store = Store::open_in_memory().unwrap();
        let author = "ab".repeat(32);
        let report = |peer: &str, full: bool, seen: u64| {
            store
                .record_seen(peer, full, &[(author.clone(), seen)])
                .unwrap();
        };
        let reports = || {
            let mut r = store.seen_reports().unwrap().remove(&author).unwrap();
            r.sort_unstable();
            r
        };
        report("p1", false, 2_000);
        report("p2", true, 3_000);
        // A peer's earliest report counts.
        report("p2", true, 2_500);
        report("p2", true, 9_000);
        assert_eq!(reports(), vec![(2_000, false), (2_500, true)]);
        // As its last hello says: p2 no longer has full history.
        report("p2", false, 9_000);
        assert_eq!(reports(), vec![(2_000, false), (2_500, false)]);
        // A store no 0.4.2 node opened, read-only: no reports.
        store.conn().execute_batch("DROP TABLE seen_peers").unwrap();
        assert!(store.seen_reports().unwrap().is_empty());
    }

    #[test]
    fn first_sightings_keep_the_first_eight_peers() {
        let store = Store::open_in_memory().unwrap();
        let author = "ab".repeat(32);
        let report = |peer: &str, full: bool, seen: u64| {
            store
                .record_seen(peer, full, &[(author.clone(), seen)])
                .unwrap();
        };
        for i in 0..12u64 {
            report(&format!("p{i}"), false, 1_000 + i * 10);
        }
        // An earlier report from a new peer pushes none out: which peers
        // count does not depend on what they say.
        report("early", false, 500);
        let rows = || -> Vec<(String, i64)> {
            let conn = store.conn();
            let mut stmt = conn
                .prepare("SELECT peer, seen FROM key_seen WHERE author = ? ORDER BY seen")
                .unwrap();
            stmt.query_map([&author], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        };
        let first = rows();
        assert_eq!(first.len(), SEEN_PEERS as usize);
        assert_eq!(first[0], ("p0".to_string(), 1_000));
        assert_eq!(first[7], ("p7".to_string(), 1_070));
        // A peer with full history takes the latest report's place, once.
        report("full1", true, 400);
        report("full2", true, 300);
        let now = rows();
        assert_eq!(now.len(), SEEN_PEERS as usize);
        assert_eq!(now[0], ("full1".to_string(), 400));
        assert!(now.iter().all(|(p, _)| p != "p7" && p != "full2"));
        // Too long to be a key: not recorded.
        store
            .record_seen("p1", false, &[("x".repeat(65), 1)])
            .unwrap();
        let count: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM key_seen", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, SEEN_PEERS);
    }

    /// Store bytes per event at 50k events, for `sync.initial_max`
    /// (ADR 0025 §2): `cargo test -p wats --lib -- --ignored
    /// store_bytes_at_50k --nocapture`.
    #[test]
    #[ignore]
    fn store_bytes_at_50k_events() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("events.sqlite")).unwrap();
        let empty = store.store_bytes();
        let keys: Vec<SigningKey> = (0..200u16)
            .map(|i| {
                let mut bytes = [9u8; 32];
                bytes[..2].copy_from_slice(&i.to_be_bytes());
                SigningKey::from_bytes(&bytes)
            })
            .collect();
        let text = "a typical answer, a few lines long, about linking Rust on musl. ";
        let mut roots: Vec<Event> = Vec::new();
        for i in 0..50_000usize {
            let key = &keys[i % keys.len()];
            let ts = 1_700_000_000 + i as u64;
            let event = if i % 5 == 0 || roots.is_empty() {
                Draft {
                    net: "public".into(),
                    kind: Some(Kind::Ask),
                    title: Some(format!("question {i}")),
                    text: text.repeat(3),
                    tags: vec!["rust".into(), "musl".into()],
                    ..Draft::default()
                }
                .sign(key, ts, 0)
                .unwrap()
            } else {
                let parent = &roots[roots.len() - 1 - (i % roots.len().min(20))];
                let (kind, body, data) = match i % 3 {
                    0 => (
                        Kind::React,
                        "+1".to_string(),
                        serde_json::json!({"value": 1}),
                    ),
                    _ => (Kind::Answer, text.repeat(4), serde_json::json!({})),
                };
                Draft {
                    net: "public".into(),
                    thread: Some(parent.thread_root().to_string()),
                    parent: Some(parent.id.clone()),
                    kind: Some(kind),
                    text: body,
                    data: data.as_object().unwrap().clone(),
                    ..Draft::default()
                }
                .sign(key, ts, 0)
                .unwrap()
            };
            store.insert(&event, ts).unwrap();
            if event.kind == Kind::Ask {
                roots.push(event);
            }
        }
        let bytes = store.store_bytes() - empty;
        println!(
            "50k events: {} store bytes, {} per event",
            bytes,
            bytes / 50_000
        );
    }

    // Rotation at the cap (ADR 0025 §7).

    fn long(key: u8, ts: u64, text: &str) -> Event {
        long_of(key, ts, Kind::Note, text, None)
    }

    /// A post whose body is worth dropping (over 1 KiB more than its
    /// tombstone), a reply when `to` is given.
    fn long_of(seed: u8, ts: u64, kind: Kind, title: &str, to: Option<&Event>) -> Event {
        Draft {
            net: "public".into(),
            thread: to.map(|t| t.thread_root().to_string()),
            parent: to.map(|t| t.id.clone()),
            kind: Some(kind),
            tags: vec!["topic:rotation".into()],
            title: Some(title.into()),
            text: format!("{title}: {}", "a long body worth dropping. ".repeat(60)),
            ..Draft::default()
        }
        .sign(&key(seed), ts, 0)
        .unwrap()
    }

    /// An event of a version this node does not understand.
    fn future(seed: u8, ts: u64) -> RawEvent {
        let wire = crate::wire::sign_value(
            json!({"v": 9, "ts": ts, "net": "public", "thread": null,
                   "parent": null, "kind": "note"})
            .as_object()
            .unwrap()
            .clone(),
            json!({"text": "from the future"})
                .as_object()
                .unwrap()
                .clone(),
            &key(seed),
            0,
        )
        .unwrap();
        RawEvent::from_value(wire).unwrap()
    }

    fn keep_of(store: &Store, own: &[u8]) -> Keep {
        let local = own
            .iter()
            .map(|k| crate::identity::author_id(&key(*k)))
            .collect();
        store.rotation_keep(&local).unwrap()
    }

    fn fts_ok(store: &Store) {
        store
            .conn()
            .execute(
                "INSERT INTO events_fts (events_fts) VALUES ('integrity-check')",
                [],
            )
            .unwrap();
    }

    #[test]
    fn tombstones_stay_out_of_the_opaque_quota() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let store = Store::open(&path).unwrap();
        let mut ids = Vec::new();
        for i in 0..20 {
            let event = long(1, 100 + i, &format!("post {i}"));
            store.insert(&event, 100 + i).unwrap();
            ids.push(event.id.clone());
        }
        // Many tombstones: evicted here, and received as such.
        for id in &ids[..10] {
            store
                .drop_body(id, DropReason::Evicted, None, None, 200)
                .unwrap();
        }
        let received = long(2, 300, "only its tombstone came");
        store.insert_raw(&tombstone_of(&received), 300).unwrap();
        assert_eq!(store.opaque_bytes(), 0, "tombstones are not opaque content");
        // An event of a version this node does not understand counts.
        let raw = future(3, 400);
        assert!(counts_as_opaque(&raw));
        store.insert_raw(&raw, 400).unwrap();
        let counted = store.opaque_bytes();
        assert!(counted > 0);
        // A full event replacing its tombstone leaves the count alone.
        store.insert(&received, 500).unwrap();
        assert_eq!(store.opaque_bytes(), counted);
        drop(store);
        assert_eq!(
            Store::open(&path).unwrap().opaque_bytes(),
            counted,
            "the same once reopened"
        );
    }

    #[test]
    fn reputation_reads_the_heads_of_tombstones() {
        let store = Store::open_in_memory().unwrap();
        let ask = post(1, 10, Kind::Ask, &[], "how?");
        let answer = long_of(2, 11, Kind::Answer, "the answer", Some(&ask));
        let react = Draft {
            net: "public".into(),
            thread: Some(ask.id.clone()),
            parent: Some(answer.id.clone()),
            kind: Some(Kind::React),
            text: "accepted".into(),
            data: json!({"accept": true}).as_object().unwrap().clone(),
            ..Draft::default()
        }
        .sign(&key(1), 12, 0)
        .unwrap();
        for e in [&ask, &answer, &react] {
            store.insert(e, e.ts).unwrap();
        }
        for id in [&ask.id, &answer.id] {
            store
                .drop_body(id, DropReason::Evicted, None, None, 20)
                .unwrap();
        }
        let rows = store.net_signal_rows("public", 100).unwrap();
        let (_, head) = rows
            .iter()
            .find(|(row, _)| row.id == react.id && row.target == answer.id)
            .unwrap();
        let head = head.as_ref().expect("the tombstone's head");
        assert_eq!(head.author, answer.author);
        assert_eq!(head.kind, "answer");
        assert_eq!(
            store.ask_roots("public").unwrap(),
            [(ask.id.clone(), ask.author.clone())]
        );
        assert_eq!(store.heads(std::slice::from_ref(&ask.id)).unwrap().len(), 1);
        // An opaque event of an unknown version has no head.
        let raw = future(3, 30);
        store.insert_raw(&raw, 30).unwrap();
        assert!(
            store
                .heads(std::slice::from_ref(&raw.id))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn eviction_keeps_the_never_drop_list_reacts_and_short_bodies() {
        let store = Store::open_in_memory().unwrap();
        let own = long(9, 10, "mine");
        let theirs = long(1, 11, "theirs, in a thread of mine");
        let in_my_thread = long_of(2, 12, Kind::Answer, "a long reply", Some(&theirs));
        let mine_there = reply(&theirs, 9, 13, "I posted here");
        let to_mine = long_of(2, 13, Kind::Answer, "a reply to my post", Some(&own));
        let announce = long_of(3, 14, Kind::Announce, "about me", None);
        let asked = long_of(4, 15, Kind::Ask, "the human's ask", None);
        let short = post(5, 16, Kind::Note, &[], "short");
        let react = Draft {
            net: "public".into(),
            thread: Some(theirs.id.clone()),
            parent: Some(theirs.id.clone()),
            kind: Some(Kind::React),
            text: "a long reasoned vote. ".repeat(100),
            data: json!({"value": 1}).as_object().unwrap().clone(),
            ..Draft::default()
        }
        .sign(&key(6), 17, 0)
        .unwrap();
        let other = long(7, 18, "nobody's");
        let mut with_blob = Draft {
            text: "files. ".repeat(200),
            ..Draft::default()
        };
        with_blob.net = "public".into();
        with_blob.kind = Some(Kind::Offer);
        with_blob.title = Some("files".into());
        with_blob.blobs = vec![crate::event::BlobRef {
            hash: "ab".repeat(32),
            name: "file.txt".into(),
            size: 3,
            url: None,
        }];
        let with_blob = with_blob.sign(&key(8), 19, 0).unwrap();
        for e in [
            &own,
            &theirs,
            &in_my_thread,
            &mine_there,
            &to_mine,
            &announce,
            &asked,
            &short,
            &react,
            &other,
            &with_blob,
        ] {
            store.insert(e, e.ts).unwrap();
        }
        store.set_ask_notify(&asked.id, "claude", 15).unwrap();
        let keep = keep_of(&store, &[9]);
        let all: Vec<String> = [
            &own,
            &theirs,
            &in_my_thread,
            &mine_there,
            &to_mine,
            &announce,
            &asked,
            &short,
            &react,
            &other,
            &with_blob,
        ]
        .iter()
        .map(|e| e.id.clone())
        .collect();
        let done = store.evict_bodies(&all, &keep, 100).unwrap();
        // Kept: mine, its ancestor (`theirs`), the reply to it, the
        // kept-forever kind, the human's ask, the react and the short post.
        // A sibling of my reply in the thread is anyone's. The post with a
        // blob is left to the node's eviction (ADR 0029's last-copy rule).
        assert_eq!(done.rows, 2, "only the two worth it, unprotected");
        assert_eq!(
            store.lists_blobs(&all).unwrap(),
            HashSet::from([with_blob.id.clone()])
        );
        for id in &all {
            let evicted = *id == other.id || *id == in_my_thread.id;
            assert_eq!(
                store.get(id).unwrap().is_none(),
                evicted,
                "{id}: evicted {evicted}"
            );
        }
        assert_eq!(
            store.drop_record(&other.id).unwrap().unwrap().reason,
            DropReason::Evicted
        );
        assert!(done.freed_blobs.is_empty(), "the store frees no blob");
        assert!(store.unpin_pending().unwrap().is_empty());
        assert!(store.history_lossy().unwrap());
        assert_eq!(found(&store, "nobody"), 0);
        fts_ok(&store);
    }

    #[test]
    fn deleting_rows_raises_the_cut_keeps_islands_ages_and_drop_records() {
        let store = Store::open_in_memory().unwrap();
        let announce = post(7, 40, Kind::Announce, &[], "kept forever");
        let own = long(9, 50, "mine");
        let retracted = long(5, 60, "retracted by its author");
        let evicted = long(5, 70, "evicted");
        let mut old = Vec::new();
        for i in 0..5 {
            old.push(long(5, 100 + i, &format!("old {i}")));
        }
        let blob = with_blobs(6, 110, "a file", &["cd".repeat(32).as_str()]);
        let recent = long(5, 1_000, "recent");
        for e in [&announce, &own, &retracted, &evicted, &blob, &recent]
            .into_iter()
            .chain(old.iter())
        {
            // Received when written: key 5 is first seen at 60.
            store.insert(e, e.ts).unwrap();
        }
        let future = future(4, 105);
        store.insert_raw(&future, 105).unwrap();
        assert!(store.opaque_bytes() > 0);
        store
            .drop_body(&retracted.id, DropReason::Erasure, Some("label"), None, 80)
            .unwrap();
        store
            .drop_body(&evicted.id, DropReason::Evicted, None, None, 80)
            .unwrap();
        let first = store.author_first_seen(&retracted.author).unwrap();
        assert_eq!(first, Some(60));
        store.set_sync_watermark("public", 2_000).unwrap();
        store
            .add_sync_range(&SyncRange {
                state: RangeState::Skipped,
                ..range(20, 90, "")
            })
            .unwrap();

        let keep = keep_of(&store, &[9]);
        let mut ids: Vec<String> = [&announce, &own, &retracted, &evicted, &blob]
            .iter()
            .map(|e| e.id.clone())
            .collect();
        ids.extend(old.iter().map(|e| e.id.clone()));
        ids.push(future.id.clone());
        let done = store
            .delete_rows(&ids, &keep, Deletion::Cut, 3_000)
            .unwrap();
        // All but the announce, the own post and the post with a blob (left
        // to the node's eviction, under ADR 0029's last-copy rule).
        assert_eq!(
            done.rows, 8,
            "all but the announce, the own post and the blob's"
        );
        assert!(store.get(&blob.id).unwrap().is_some());
        assert_eq!(done.cuts, [("public".to_string(), 105)]);
        assert!(done.freed_blobs.is_empty());
        assert_eq!(store.cut_ts("public").unwrap(), Some(105));
        let ranges = store.sync_ranges("public").unwrap();
        // The skipped range stays: kept-forever kinds below the rotation
        // cut are served, but not where the budget skipped them.
        assert_eq!(ranges.len(), 2, "{ranges:?}");
        let cut = ranges.iter().find(|r| r.state == RangeState::Cut).unwrap();
        assert_eq!(cut.phase, PHASE_ROTATION);
        assert_eq!(cut.opened, 3_000);
        // Islands below the cut.
        assert!(store.get(&announce.id).unwrap().is_some());
        assert!(store.get(&own.id).unwrap().is_some());
        assert!(store.get(&recent.id).unwrap().is_some());
        assert!(store.get_raw(&old[0].id).unwrap().is_none());
        assert_eq!(
            store.opaque_bytes(),
            0,
            "the opaque row's bytes went with it"
        );
        // The key's age does not go back.
        assert_eq!(store.author_first_seen(&retracted.author).unwrap(), first);
        assert_eq!(
            store.key_firsts("public").unwrap()[&retracted.author],
            (60, 60)
        );
        assert!(store.history_lossy().unwrap());
        // ADR 0020 unchanged: an erased body is still refused when it
        // comes back; an evicted one is taken again, as any event.
        assert!(store.drop_record(&evicted.id).unwrap().is_none());
        assert_eq!(store.insert(&retracted, 4_000).unwrap(), Inserted::Dropped);
        assert!(matches!(
            store.insert(&evicted, 4_000).unwrap(),
            Inserted::New(_)
        ));
        assert!(matches!(
            store.insert(&old[0], 4_000).unwrap(),
            Inserted::New(_)
        ));
        fts_ok(&store);
        // A lower cut never lowers it.
        let again = store
            .delete_rows(&[old[0].id.clone()], &keep, Deletion::Cut, 5_000)
            .unwrap();
        assert!(again.cuts.is_empty());
        assert_eq!(store.cut_ts("public").unwrap(), Some(105));
    }

    #[test]
    fn a_rotation_interrupted_halfway_changes_nothing() {
        let store = Store::open_in_memory().unwrap();
        let events: Vec<Event> = (0..10)
            .map(|i| long(5, 100 + i, &format!("n{i}")))
            .collect();
        for e in &events {
            store.insert(e, e.ts).unwrap();
        }
        let ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
        let keep = Keep::default();
        let bytes = store.store_bytes();
        FAIL_AFTER.with(|f| f.set(Some(4)));
        assert!(
            store
                .delete_rows(&ids, &keep, Deletion::Cut, 1_000)
                .is_err()
        );
        assert!(store.evict_bodies(&ids, &keep, 1_000).is_err());
        FAIL_AFTER.with(|f| f.set(None));
        assert_eq!(store.count().unwrap(), 10);
        assert!(events.iter().all(|e| store.get(&e.id).unwrap().is_some()));
        assert_eq!(store.cut_ts("public").unwrap(), None);
        assert!(!store.history_lossy().unwrap());
        assert!(store.drop_record(&ids[0]).unwrap().is_none());
        assert_eq!(store.store_bytes(), bytes);
        fts_ok(&store);
        // And then it goes through.
        assert_eq!(
            store
                .delete_rows(&ids, &keep, Deletion::Cut, 1_000)
                .unwrap()
                .rows,
            10
        );
        assert_eq!(store.cut_ts("public").unwrap(), Some(109));
    }

    #[test]
    fn deleted_rows_free_store_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("events.sqlite")).unwrap();
        let events: Vec<Event> = (0..2_000u64)
            .map(|i| long((i % 200) as u8, 1_000 + i, &format!("n{i}")))
            .collect();
        for e in &events {
            store.insert(e, e.ts).unwrap();
        }
        let full = store.store_bytes();
        let keep = Keep::default();
        let ids: Vec<String> = events[..1_000].iter().map(|e| e.id.clone()).collect();
        for chunk in ids.chunks(500) {
            let done = store
                .delete_rows(chunk, &keep, Deletion::Cut, 9_000)
                .unwrap();
            assert_eq!(done.rows, 500);
        }
        let after = store.store_bytes();
        eprintln!(
            "store bytes: {full} -> {after}, free {}",
            store.free_bytes()
        );
        assert!(after < full * 7 / 10, "{full} -> {after}");
        assert!(store.free_bytes() > 0);
        // Freed pages are reused.
        let size = std::fs::metadata(dir.path().join("events.sqlite"))
            .unwrap()
            .len();
        for e in &events[..500] {
            store.insert(e, 10_000).unwrap();
        }
        store
            .conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let grown = std::fs::metadata(dir.path().join("events.sqlite"))
            .unwrap()
            .len();
        // The 500 posts go into freed pages: the file barely grows (a few
        // pages of index and full-text segments), far less than they weigh.
        assert!(grown <= size.max(full) / 100 * 102, "{size} -> {grown}");
    }

    #[test]
    fn the_blob_index_follows_bodies_and_is_filled_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let (a, b) = ("ab".repeat(32), "cd".repeat(32));
        let first = with_blobs(1, 10, "a file", &[a.as_str()]);
        let second = with_blobs(
            2,
            11,
            "the same file, and another",
            &[a.as_str(), b.as_str()],
        );
        {
            let store = Store::open(&path).unwrap();
            store.insert(&first, 10).unwrap();
            store.insert(&second, 11).unwrap();
            assert!(store.blob_referenced(&a).unwrap());
            store
                .drop_body(&second.id, DropReason::Evicted, None, None, 20)
                .unwrap();
            assert!(
                store.blob_referenced(&a).unwrap(),
                "the first post lists it"
            );
            assert!(!store.blob_referenced(&b).unwrap());
            // As if an older release had stored the posts: no index.
            let conn = store.conn();
            conn.execute_batch("DELETE FROM event_blobs; DELETE FROM event_blobs_filled")
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(store.blob_referenced(&a).unwrap(), "indexed again at open");
        assert!(!store.blob_referenced(&b).unwrap());
        let max = store.max_seq().unwrap();
        let indexed: i64 = store
            .conn()
            .query_row("SELECT upto FROM event_blobs_filled", [], |r| r.get(0))
            .unwrap();
        assert_eq!(indexed, max);
        // Rotation's deletion leaves a post that lists blobs to the node.
        let keep = Keep::default();
        let done = store
            .delete_rows(std::slice::from_ref(&first.id), &keep, Deletion::Cut, 30)
            .unwrap();
        assert_eq!(done.rows, 0);
        let done = store
            .drop_body(&first.id, DropReason::Own, None, None, 30)
            .unwrap();
        assert_eq!(done.freed_blobs, std::slice::from_ref(&a));
        assert!(!store.blob_referenced(&a).unwrap());
    }

    #[test]
    fn the_opaque_count_is_cached_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.sqlite");
        let counted = {
            let store = Store::open(&path).unwrap();
            store.insert_raw(&future(3, 40), 40).unwrap();
            store.opaque_bytes()
        };
        assert!(counted > 0);
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.opaque_bytes(), counted);
            // An older release adds an opaque row, and records nothing.
            let raw = future(4, 50);
            let json = serde_json::to_string(&raw.wire).unwrap();
            insert_opaque_row(&store.conn(), &raw, &json, 50).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(store.opaque_bytes() > counted, "the rows added since count");
    }

    #[test]
    fn a_post_of_this_node_after_the_plan_protects_its_parent() {
        let store = Store::open_in_memory().unwrap();
        let theirs = long(1, 10, "a question");
        let other = long(2, 11, "nobody's");
        store.insert(&theirs, 10).unwrap();
        store.insert(&other, 11).unwrap();
        // The plan knew no post of key 9 in this thread...
        let keep = keep_of(&store, &[9]);
        assert!(!keep.protects(&head_of(&store, &theirs)));
        // ...which posts in it before the transaction.
        store
            .insert(&reply(&theirs, 9, 12, "my answer"), 12)
            .unwrap();
        // A profile first used since the plan, too.
        store
            .note_local_key(&crate::identity::author_id(&key(2)), "late", 12)
            .unwrap();
        let ids = [theirs.id.clone(), other.id.clone()];
        assert_eq!(store.evict_bodies(&ids, &keep, 20).unwrap().rows, 0);
        assert_eq!(
            store
                .delete_rows(&ids, &keep, Deletion::Cut, 20)
                .unwrap()
                .rows,
            0
        );
        // Stage 0 keeps only what is strictly kept: the late profile's post.
        let trimmed = store.delete_rows(&ids, &keep, Deletion::Trim, 20).unwrap();
        assert_eq!(trimmed.rows, 1);
        assert!(store.get_raw(&theirs.id).unwrap().is_none());
        assert!(store.get(&other.id).unwrap().is_some());
    }

    fn head_of(store: &Store, event: &Event) -> RowHead {
        let seq = store.get(&event.id).unwrap().unwrap().seq;
        store.rows_in_seq(seq - 1, seq).unwrap().remove(0)
    }

    #[test]
    fn a_body_redelivered_keeps_its_eviction_and_its_kept_replicas() {
        let store = Store::open_in_memory().unwrap();
        let hash = "ee".repeat(32);
        let event = with_blobs(1, 10, "a replica", &[&hash]);
        store.insert(&event, 10).unwrap();
        store
            .drop_body_keeping(
                &event.id,
                DropReason::Evicted,
                None,
                None,
                20,
                &[(hash.clone(), 3)],
            )
            .unwrap();
        // Gossip or a sync page bring the body again: refused, and the
        // record of the kept replica stays.
        assert_eq!(store.insert(&event, 30).unwrap(), Inserted::Dropped);
        assert_eq!(
            store.evicted_replicas(&event.id).unwrap(),
            std::slice::from_ref(&hash)
        );
        assert!(store.get(&event.id).unwrap().is_none());
        // A read restores it: the body lists the blob again, the record goes.
        assert!(matches!(
            store.insert_with(&event, 40, true).unwrap(),
            Inserted::New(_)
        ));
        assert!(store.evicted_replicas(&event.id).unwrap().is_empty());
        assert!(store.blob_referenced(&hash).unwrap());
    }

    #[test]
    fn rotation_never_overrides_a_stronger_reason() {
        let store = Store::open_in_memory().unwrap();
        let erased = long(1, 10, "erased");
        let free = long(2, 11, "anyone's");
        store.insert(&erased, 10).unwrap();
        store.insert(&free, 11).unwrap();
        store
            .drop_body(&erased.id, DropReason::Erasure, Some("label"), None, 20)
            .unwrap();
        let keep = Keep::default();
        assert!(!store.rotatable(&erased.id, &keep, false).unwrap());
        assert!(store.rotatable(&free.id, &keep, false).unwrap());
        // An eviction landing after it changes nothing.
        store
            .drop_body_keeping(&erased.id, DropReason::Evicted, None, None, 30, &[])
            .unwrap();
        assert_eq!(
            store.drop_record(&erased.id).unwrap().unwrap().reason,
            DropReason::Erasure
        );
    }

    #[test]
    fn any_eviction_marks_the_history_lossy() {
        let store = Store::open_in_memory().unwrap();
        let post = long(1, 10, "a post");
        let other = long(2, 11, "another");
        store.insert(&post, 10).unwrap();
        store.insert(&other, 11).unwrap();
        // A drop for another reason: still the full history, as before.
        store
            .drop_body(&other.id, DropReason::Own, None, None, 20)
            .unwrap();
        assert!(!store.history_lossy().unwrap());
        // The node's eviction path (`drop_body_keeping`), not rotation's.
        store
            .drop_body_keeping(&post.id, DropReason::Evicted, None, None, 30, &[])
            .unwrap();
        assert!(store.history_lossy().unwrap());
    }

    #[test]
    fn the_thread_root_of_an_own_post_is_kept_without_the_links_up() {
        let store = Store::open_in_memory().unwrap();
        let root = long(1, 10, "the question");
        let middle = long_of(2, 11, Kind::Answer, "an answer", Some(&root));
        let mine = reply(&middle, 9, 12, "my follow-up");
        // The middle reply is not held (trimmed, or never fetched).
        store.insert(&root, 10).unwrap();
        store.insert(&mine, 12).unwrap();
        let keep = keep_of(&store, &[9]);
        assert!(keep.protects(&head_of(&store, &root)), "{keep:?}");
        let ids = [root.id.clone()];
        assert_eq!(store.evict_bodies(&ids, &keep, 20).unwrap().rows, 0);
        // Inside the transaction too, with a plan that knew nothing.
        let blind = Keep {
            authors: keep.authors.clone(),
            ..Keep::default()
        };
        assert_eq!(
            store
                .delete_rows(&ids, &blind, Deletion::Cut, 20)
                .unwrap()
                .rows,
            0
        );
    }

    #[test]
    fn a_retraction_takes_the_refs_an_eviction_keeps() {
        let store = Store::open_in_memory().unwrap();
        let cited = long(1, 10, "a source");
        let citing = |seed: u8, ts: u64| {
            Draft {
                net: "public".into(),
                kind: Some(Kind::Note),
                title: Some("citing".into()),
                text: "see the source".into(),
                refs: vec![cited.id.clone()],
                ..Draft::default()
            }
            .sign(&key(seed), ts, 0)
            .unwrap()
        };
        let (evicted, retracted) = (citing(2, 11), citing(3, 12));
        for e in [&cited, &evicted, &retracted] {
            store.insert(e, e.ts).unwrap();
        }
        store
            .drop_body(&evicted.id, DropReason::Evicted, None, None, 20)
            .unwrap();
        store
            .drop_body(&retracted.id, DropReason::Retraction, Some("r"), None, 20)
            .unwrap();
        let rows = store
            .signal_rows(std::slice::from_ref(&cited.id), 10)
            .unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, [evicted.id.as_str()], "the retracted one gives none");
        // Retracted after its eviction: none either.
        store
            .drop_body(&evicted.id, DropReason::Retraction, Some("r"), None, 30)
            .unwrap();
        assert!(
            store
                .signal_rows(std::slice::from_ref(&cited.id), 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn erasure_labels_are_kept_forever() {
        let store = Store::open_in_memory().unwrap();
        let post = long(1, 10, "a post");
        let label = Draft {
            net: "public".into(),
            thread: Some(post.id.clone()),
            parent: Some(post.id.clone()),
            kind: Some(Kind::React),
            text: "erasure".into(),
            data: json!({"label": "erasure"}).as_object().unwrap().clone(),
            ..Draft::default()
        }
        .sign(&key(2), 11, 0)
        .unwrap();
        store.insert(&post, 10).unwrap();
        store.insert(&label, 11).unwrap();
        let keep = Keep::default();
        assert!(keep.protects_strictly(&head_of(&store, &label)));
        let done = store
            .delete_rows(
                &[post.id.clone(), label.id.clone()],
                &keep,
                Deletion::Trim,
                20,
            )
            .unwrap();
        assert_eq!(done.rows, 1);
        assert!(store.get(&label.id).unwrap().is_some());
    }
}
