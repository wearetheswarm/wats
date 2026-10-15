//! Local event store and index (SPEC §2.6): SQLite with FTS5.
//!
//! Every verified event is stored once, with a local sequence number
//! (`seq`) that gives a stable cursor for `wait` and incremental reads.
//! The index is computed locally from the events; nothing here is shared.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, bail};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use tokio::sync::broadcast;

use crate::event::{Event, Kind};

const SCHEMA_VERSION: i64 = 1;

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
    received_at INTEGER NOT NULL
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
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        let (new_events, _) = broadcast::channel(1024);
        Ok(Self {
            conn: Mutex::new(conn),
            new_events,
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
            if tx
                .query_row("SELECT 1 FROM events WHERE id = ?", [&event.id], |_| Ok(()))
                .optional()?
                .is_some()
            {
                return Ok(Inserted::Duplicate);
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
                    event.tags.join(" "),
                    String::from_utf8(event.to_json())?,
                    received_at as i64,
                ],
            )?;
            let seq = tx.last_insert_rowid();
            for tag in &event.tags {
                tx.execute(
                    "INSERT OR IGNORE INTO event_tags (seq, tag) VALUES (?, ?)",
                    params![seq, tag],
                )?;
            }
            tx.commit()?;
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

    pub fn get(&self, id: &str) -> Result<Option<Stored>> {
        Ok(self
            .query(
                "SELECT seq, json FROM events WHERE id = ?",
                vec![id.to_string().into()],
            )?
            .pop())
    }

    /// The root and every reply of a thread, oldest first.
    pub fn thread(&self, root: &str, limit: usize) -> Result<Vec<Stored>> {
        self.query(
            "SELECT seq, json FROM events WHERE id = ?1 OR thread = ?1 ORDER BY ts, seq LIMIT ?2",
            vec![root.to_string().into(), (limit as i64).into()],
        )
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
    ) -> Result<Vec<Stored>> {
        self.query(
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
                     AND a.kind = 'announce' AND a.net = e.net)
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
        let first = &page[0].event;
        let rest = store.page_after("public", first.ts, &first.id, 10).unwrap();
        assert_eq!(rest.len(), 2);
        assert_eq!(store.sync_watermark("public").unwrap(), 0);
        store.set_sync_watermark("public", 50).unwrap();
        store.set_sync_watermark("public", 40).unwrap();
        assert_eq!(store.sync_watermark("public").unwrap(), 50);
    }

    #[test]
    fn subscribers_see_new_events() {
        let store = Store::open_in_memory().unwrap();
        let mut rx = store.subscribe();
        let e = post(1, 10, Kind::Note, &[], "hello");
        store.insert(&e, 10).unwrap();
        assert_eq!(rx.try_recv().unwrap().event.id, e.id);
    }
}
