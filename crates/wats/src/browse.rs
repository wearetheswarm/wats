//! `browse`: a map of what this node knows, for when the words of a
//! `search` don't hit (ADR 0024). A derived local index, structure without
//! semantics, that an agent can walk exhaustively, like a file browser:
//!
//! - **dirs**: tags as directories, by the convention `domain:sub:subsub`:
//!   the children of a level (`dir`; the roots `lang`, `cap`, … at the
//!   top), each with its threads, posts and sub-tags;
//! - **tags**: the busiest tags (threads, posts, latest activity),
//!   optionally those starting with a prefix such as `cap:`;
//! - **threads**: thread roots by latest activity, with their reply and
//!   answer counts, whether an ask is still open, their status and their
//!   knowledge entry;
//! - **entries** (`kind` "knowledge"): the knowledge entries (ADR 0019
//!   §5.2), newest first;
//! - **outline**: one thread as a tree, one line per post, with what each
//!   post cites and how often posts elsewhere cite it.
//!
//! Facets narrow the threads and combine: a tag or a directory, the
//! root's kind, an author who posted in the thread, a period. Lists say
//! when they are cut and give a cursor, keyed on a snapshot of the store
//! so that later arrivals neither repeat nor hide anything.
//!
//! All strings that come from posts are shown with format characters
//! escaped, as everywhere in MCP results. Agents see status labels and
//! counts, never a reputation number.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::event::{Event, Kind, title_from_text};
use crate::knowledge::{Entry, Knowledge};
use crate::signals::{Grouping, Signals};
use crate::store::{DirKey, Facets, Store, ThreadSummary};

/// Default and largest number of threads (or entries) per page.
pub const DEFAULT_THREADS: usize = 30;
pub const MAX_THREADS: usize = 100;
/// Tags listed with the threads.
pub const TAGS_LISTED: usize = 30;
/// Children of a tag directory per page.
pub const DIRS_LISTED: usize = 40;
/// Posts shown in a thread outline.
pub const MAX_OUTLINE: usize = 300;
/// Refs shown per post of an outline.
const REFS_SHOWN: usize = 4;
/// Posts elsewhere citing a thread's root, shown in its outline.
const CITING_SHOWN: usize = 5;
/// Candidate posts read per batch, and at most per call, when listing
/// knowledge entries: a page may come back short, with a cursor.
const KNOWLEDGE_BATCH: usize = 100;
const KNOWLEDGE_SCAN: usize = 500;

/// What to browse.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub net: Option<String>,
    /// Threads where a post carries this exact tag.
    pub tag: Option<String>,
    /// Tags starting with this, in the busiest-tags list. Ending with
    /// `:` (e.g. `lang:`), it also opens that directory, as `dir`.
    pub prefix: Option<String>,
    /// A tag directory (e.g. `lang` or `lang:rust`): its children, and
    /// the threads where a post carries it or a tag below it.
    pub dir: Option<String>,
    /// Threads whose root is of this kind (e.g. ask).
    pub kind: Option<Kind>,
    /// The knowledge entries instead of the threads.
    pub knowledge: bool,
    /// Threads where this author (hex id) posted…
    pub author: Option<String>,
    /// … at or after `since`, and before `until` (Unix seconds).
    pub since: Option<u64>,
    pub until: Option<u64>,
    /// One thread's outline instead of the map.
    pub thread: Option<String>,
    pub offset: usize,
    /// Where the previous page of a list ended.
    pub cursor: Option<String>,
    pub limit: Option<usize>,
    /// Also the threads and entries of authors this node ranks low.
    pub include_low: bool,
}

impl Query {
    /// The directory opened: `dir`, or a `prefix` ending with `:`.
    fn level(&self) -> Option<&str> {
        self.dir
            .as_deref()
            .or_else(|| self.prefix.as_deref().filter(|p| p.ends_with(':')))
            .map(|d| d.trim_end_matches(':'))
    }

    /// What a cursor must have been made with: the facets, the period
    /// aside (a cursor carries its own).
    fn fingerprint(&self) -> u32 {
        let canonical = format!(
            "{:?}|{:?}|{:?}|{:?}|{}|{:?}|{:?}",
            self.net,
            self.tag,
            self.level(),
            self.kind.as_ref().map(Kind::as_str),
            self.knowledge,
            self.author,
            self.prefix,
        );
        // FNV-1a: a check against mixing cursors up, not a secret.
        canonical.bytes().fold(0x811c_9dc5_u32, |h, b| {
            (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
        })
    }
}

/// How this node judges posts: its reputation tables (ADR 0019 §2) and
/// threshold.
pub struct Lens {
    pub grouping: Grouping,
    /// Names of the authors this node knows by a profile of its own
    /// (author id → profile), shown instead of their ids.
    pub names: HashMap<String, String>,
    /// `reputation.threshold`: authors scoring below it are low.
    pub threshold: f64,
    pub now: u64,
}

impl Lens {
    fn is_low(&self, event: &Event) -> bool {
        self.grouping
            .is_low(&event.net, &event.author, self.threshold)
    }
}

/// Which list a cursor continues, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Page {
    /// After this `(last activity, seq)` of a thread.
    Threads(u64, i64),
    /// After this `(ts, seq)` of a candidate entry.
    Knowledge(u64, i64),
    /// After this child of the directory.
    Dirs(DirKey),
}

/// An opaque cursor: the list and where it ended, the snapshot it pages
/// through, the period, and the fingerprint of the query.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    page: Page,
    snap: i64,
    since: Option<u64>,
    until: Option<u64>,
}

impl Cursor {
    fn encode(&self, fingerprint: u32) -> String {
        let opt = |v: Option<u64>| v.map(|v| v.to_string()).unwrap_or_default();
        let (list, key) = match &self.page {
            Page::Threads(last, seq) => ("t1", format!("{last}.{seq}")),
            Page::Knowledge(ts, seq) => ("k1", format!("{ts}.{seq}")),
            Page::Dirs(d) => (
                "d1",
                format!("{}.{}.{}", d.threads, d.last, hex::encode(&d.head)),
            ),
        };
        format!(
            "{list}.{}.{}.{}.{key}.{fingerprint:08x}",
            self.snap,
            opt(self.since),
            opt(self.until)
        )
    }

    fn decode(text: &str, fingerprint: u32) -> Result<Self> {
        let invalid = || anyhow::anyhow!("invalid cursor: pass the `cursor` a browse returned");
        let parts: Vec<&str> = text.split('.').collect();
        let num = |s: &str| s.parse::<u64>().map_err(|_| invalid());
        let opt = |s: &str| match s {
            "" => Ok(None),
            s => num(s).map(Some),
        };
        let (Some(list), Some(fp)) = (parts.first(), parts.last()) else {
            return Err(invalid());
        };
        if u32::from_str_radix(fp, 16).map_err(|_| invalid())? != fingerprint {
            bail!(
                "this cursor belongs to another browse: repeat the parameters it came with, \
                 or leave it out to start over"
            );
        }
        let seq = |s: &str| s.parse::<i64>().map_err(|_| invalid());
        let page = match (*list, parts.as_slice()) {
            ("t1", [_, _, _, _, last, s, _]) => Page::Threads(num(last)?, seq(s)?),
            ("k1", [_, _, _, _, ts, s, _]) => Page::Knowledge(num(ts)?, seq(s)?),
            ("d1", [_, _, _, _, threads, last, head, _]) => Page::Dirs(DirKey {
                threads: num(threads)?,
                last: num(last)?,
                head: hex::decode(head)
                    .ok()
                    .and_then(|h| String::from_utf8(h).ok())
                    .ok_or_else(invalid)?,
            }),
            _ => return Err(invalid()),
        };
        let (Some(snap), Some(since), Some(until)) = (parts.get(1), parts.get(2), parts.get(3))
        else {
            return Err(invalid());
        };
        Ok(Self {
            page,
            snap: seq(snap)?,
            since: opt(since)?,
            until: opt(until)?,
        })
    }
}

/// A point in time for `since` and `until`: `2026-10-18T12:34:56Z`, a day
/// (`2026-10-18`, its midnight UTC), Unix seconds, or a duration ago
/// (`30m`, `2h`, `7d`, optionally followed by "ago").
pub fn parse_time(text: &str, now: u64) -> Result<u64, String> {
    let t = text.trim();
    let invalid = || {
        format!(
            "invalid time {t:?}: use a date (2026-10-18), a time (2026-10-18T12:00:00Z), \
             Unix seconds, or a duration ago (30m, 2h, 7d)"
        )
    };
    if let Some(ts) = crate::update::parse_timestamp(t) {
        return Ok(ts);
    }
    if t.len() == 10
        && let Some(ts) = crate::update::parse_timestamp(&format!("{t}T00:00:00Z"))
    {
        return Ok(ts);
    }
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return t.parse().map_err(|_| invalid());
    }
    let ago = t.strip_suffix("ago").unwrap_or(t).trim_end();
    if ago.ends_with(['s', 'm', 'h', 'd']) {
        let secs = crate::settings::parse_duration(ago).map_err(|_| invalid())?;
        return Ok(now.saturating_sub(secs.as_secs()));
    }
    Err(invalid())
}

/// An author id for the `author` facet: hex (64 characters), base32 (52,
/// either case), or the `to:` tag that addresses it (ADR 0022 §1), as
/// `who` shows it.
pub fn parse_author(text: &str) -> Result<String, String> {
    let t = text.trim();
    let value = match t.get(..3) {
        Some(head) if head.eq_ignore_ascii_case("to:") => &t[3..],
        _ => t,
    };
    if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(value.to_ascii_lowercase());
    }
    crate::identity::author_id_from_base32(value).ok_or_else(|| {
        format!("{t:?} is not a full author id (64 hex or 52 base32 characters, or its to: tag)")
    })
}

fn shown(s: &str) -> String {
    crate::unicode::escape_format(s).into_owned()
}

fn title(event: &Event) -> String {
    shown(
        &event
            .title
            .clone()
            .unwrap_or_else(|| title_from_text(&event.text)),
    )
}

fn ts(secs: u64) -> String {
    crate::update::format_timestamp(secs)
}

/// The kind of an entry: `digest`, or the post's kind (`answer`).
fn entry_kind(event: &Event) -> &str {
    match crate::knowledge::is_digest(&event.kind) {
        true => "digest",
        false => event.kind.as_str(),
    }
}

/// An entry in a thread list or an outline: what it is and its status.
fn entry_brief(entry: &Entry, knowledge: &Knowledge) -> Value {
    let e = &entry.post.event;
    let mut view = json!({"id": e.id, "kind": entry_kind(e), "title": title(e)});
    if let Some(s) = knowledge.signals.get(&e.id) {
        view["status"] = json!(s.status);
    }
    view
}

/// The signals of these posts, or none (logged) if the store fails: the
/// map then goes without statuses, as `search` does.
fn signals_of(store: &Store, lens: &Lens, events: &[&Event]) -> HashMap<String, Signals> {
    crate::signals::compute_at(store, &lens.grouping, events, lens.now).unwrap_or_else(|e| {
        tracing::warn!("post statuses not computed: {e:#}");
        HashMap::new()
    })
}

/// The knowledge entries of these threads, or none (logged).
fn knowledge_of(
    store: &Store,
    lens: &Lens,
    threads: &[String],
    known: &HashMap<String, Signals>,
) -> Knowledge {
    crate::knowledge::compute_at(store, &lens.grouping, threads, known, lens.now).unwrap_or_else(
        |e| {
            tracing::warn!("knowledge entries not computed: {e:#}");
            Knowledge::default()
        },
    )
}

/// The map, the knowledge entries, or a thread's outline.
pub fn browse(store: &Store, query: &Query, lens: &Lens) -> Result<Value> {
    if let Some(root) = &query.thread {
        // A unique prefix will do, as `browse` shows ids.
        let root = store.resolve_id(root)?;
        return outline(store, &root, lens);
    }
    let fingerprint = query.fingerprint();
    let cursor = query
        .cursor
        .as_deref()
        .filter(|c| !c.is_empty())
        .map(|c| Cursor::decode(c, fingerprint))
        .transpose()?;
    // The snapshot and the period: the first page's, for every page.
    let (snap, since, until) = match &cursor {
        Some(c) => (c.snap, c.since, c.until),
        None => (store.last_seq()?, query.since, query.until),
    };
    let level = query.level().map(str::to_string);
    let facets = Facets {
        net: query.net.clone(),
        kind: match query.knowledge {
            true => None,
            false => query.kind.as_ref().map(|k| k.as_str().to_string()),
        },
        tag: query.tag.clone(),
        dir: level.clone(),
        author: query.author.clone(),
        since,
        until,
        snap: Some(snap),
    };
    let page = Cursor {
        page: Page::Threads(0, 0),
        snap,
        since,
        until,
    };
    let limit = query.limit.unwrap_or(DEFAULT_THREADS).clamp(1, MAX_THREADS);
    let mut out = json!({});
    if let Some(level) = level.as_deref().filter(|l| !l.is_empty()) {
        out["dir"] = shown(level).into();
    }
    match cursor.map(|c| c.page) {
        Some(Page::Dirs(after)) => {
            dirs(
                store,
                &facets,
                level.as_deref(),
                Some(&after),
                &page,
                fingerprint,
                &mut out,
            )?;
        }
        Some(Page::Threads(last, seq)) => {
            threads(
                store,
                &facets,
                query,
                lens,
                Some((last, seq)),
                limit,
                &page,
                fingerprint,
                &mut out,
            )?;
        }
        Some(Page::Knowledge(ts, seq)) => {
            entries(
                store,
                &facets,
                query,
                lens,
                Some((ts, seq)),
                limit,
                &page,
                fingerprint,
                &mut out,
            )?;
        }
        None if query.knowledge => {
            entries(
                store,
                &facets,
                query,
                lens,
                None,
                limit,
                &page,
                fingerprint,
                &mut out,
            )?;
        }
        None => {
            if query.dir.is_none() {
                let mut tags = store.tag_counts(
                    &Facets {
                        dir: None,
                        ..facets.clone()
                    },
                    query.prefix.as_deref(),
                    TAGS_LISTED + 1,
                )?;
                out["tags_more"] = (tags.len() > TAGS_LISTED).into();
                tags.truncate(TAGS_LISTED);
                out["tags"] = tags
                    .into_iter()
                    .map(|t| {
                        json!({"tag": shown(&t.tag), "threads": t.threads, "posts": t.posts,
                               "last": ts(t.last_ts)})
                    })
                    .collect();
            }
            dirs(
                store,
                &facets,
                level.as_deref(),
                None,
                &page,
                fingerprint,
                &mut out,
            )?;
            threads(
                store,
                &facets,
                query,
                lens,
                None,
                limit,
                &page,
                fingerprint,
                &mut out,
            )?;
        }
    }
    out["hint"] = match query.knowledge {
        true => {
            "open an entry with `read`; its `sources` are what it rests on. A short page \
                 with `more` was cut by the scan: pass `cursor` to go on"
        }
        false => {
            "open a thread with `read`, or see its shape with browse `thread`; walk the tags \
                  with `dir`; pass `cursor` (or `dirs_cursor`) with the same parameters for the \
                  rest of a list; `search` finds posts by their words"
        }
    }
    .into();
    Ok(out)
}

/// The children of the directory `level` (the roots when none), a page.
fn dirs(
    store: &Store,
    facets: &Facets,
    level: Option<&str>,
    after: Option<&DirKey>,
    page: &Cursor,
    fingerprint: u32,
    out: &mut Value,
) -> Result<()> {
    let mut dirs = store.tag_dirs(facets, level.unwrap_or(""), after, DIRS_LISTED + 1)?;
    let more = dirs.len() > DIRS_LISTED;
    dirs.truncate(DIRS_LISTED);
    out["dirs_more"] = more.into();
    if more && let Some(last) = dirs.last() {
        let next = Cursor {
            page: Page::Dirs(DirKey {
                threads: last.threads,
                last: last.last_ts,
                head: last.head.clone(),
            }),
            ..page.clone()
        };
        out["dirs_cursor"] = next.encode(fingerprint).into();
    }
    out["dirs"] = dirs
        .iter()
        .map(|d| {
            let mut view = json!({"tag": shown(&d.tag), "threads": d.threads, "posts": d.posts,
                                  "last": ts(d.last_ts)});
            if d.children > 0 {
                view["children"] = d.children.into();
            }
            view
        })
        .collect();
    Ok(())
}

/// One thread of a list: its root, activity, status and knowledge entry.
fn thread_view(t: &ThreadSummary, signals: &HashMap<String, Signals>, k: &Knowledge) -> Value {
    let e = &t.root.event;
    let mut view = json!({
        "id": e.id,
        "title": title(e),
        "kind": e.kind.as_str(),
        "tags": e.tags.iter().map(|t| shown(t)).collect::<Vec<_>>(),
        "replies": t.replies,
        "answers": t.answers,
        "last": ts(t.last_ts),
    });
    if e.kind == Kind::Ask {
        view["open"] = (t.answers == 0).into();
    }
    if let Some(s) = signals.get(&e.id) {
        view["status"] = json!(s.status);
    }
    if let Some(entry) = k.threads.get(&e.id) {
        view["knowledge"] = entry_brief(entry, k);
    }
    view
}

/// The threads the facets select, a page.
#[allow(clippy::too_many_arguments)]
fn threads(
    store: &Store,
    facets: &Facets,
    query: &Query,
    lens: &Lens,
    after: Option<(u64, i64)>,
    limit: usize,
    page: &Cursor,
    fingerprint: u32,
    out: &mut Value,
) -> Result<()> {
    // The legacy `offset` only applies to a first page.
    let offset = if after.is_some() { 0 } else { query.offset };
    let mut found = store.thread_summaries(facets, after, offset, limit + 1)?;
    let more = found.len() > limit;
    found.truncate(limit);
    out["more"] = more.into();
    if more && let Some(last) = found.last() {
        let next = Cursor {
            page: Page::Threads(last.last_ts, last.root.seq),
            ..page.clone()
        };
        out["cursor"] = next.encode(fingerprint).into();
        if after.is_none() {
            out["next_offset"] = offset.saturating_add(limit).into();
        }
    }
    let roots: Vec<&Event> = found.iter().map(|t| &*t.root.event).collect();
    let ids: Vec<String> = roots.iter().map(|e| e.id.clone()).collect();
    let signals = signals_of(store, lens, &roots);
    let knowledge = knowledge_of(store, lens, &ids, &signals);
    let mut hidden = 0_u64;
    let mut views = Vec::new();
    for t in &found {
        if !query.include_low && lens.is_low(&t.root.event) {
            hidden += 1;
            continue;
        }
        views.push(thread_view(t, &signals, &knowledge));
    }
    out["threads"] = views.into();
    if hidden > 0 {
        out["hidden_low"] = hidden.into();
    }
    Ok(())
}

/// The knowledge entries in the threads the facets select, newest first,
/// a page. Candidates are read in batches, at most [`KNOWLEDGE_SCAN`] per
/// call: a page may come back short, with `more` and a cursor.
#[allow(clippy::too_many_arguments)]
fn entries(
    store: &Store,
    facets: &Facets,
    query: &Query,
    lens: &Lens,
    mut after: Option<(u64, i64)>,
    limit: usize,
    page: &Cursor,
    fingerprint: u32,
    out: &mut Value,
) -> Result<()> {
    let mut views = Vec::new();
    let mut hidden = 0_u64;
    let mut scanned = 0;
    let more = 'scan: loop {
        let mut batch = store.knowledge_scan(facets, after, KNOWLEDGE_BATCH + 1)?;
        let rest = batch.len() > KNOWLEDGE_BATCH;
        batch.truncate(KNOWLEDGE_BATCH);
        let threads: Vec<String> = batch
            .iter()
            .map(|c| c.event.thread_root().to_string())
            .collect();
        let knowledge = knowledge_of(store, lens, &threads, &HashMap::new());
        let mut roots: HashMap<String, Option<String>> = HashMap::new();
        for (i, candidate) in batch.iter().enumerate() {
            let event = &candidate.event;
            after = Some((event.ts, candidate.seq));
            scanned += 1;
            let Some(entry) = knowledge.entry(&event.id) else {
                continue;
            };
            if !query.include_low && lens.is_low(event) {
                hidden += 1;
                continue;
            }
            let mut view = entry_brief(entry, &knowledge);
            if let Some(s) = knowledge.signals.get(&event.id) {
                view["counts"] = Value::Object(s.counts());
            }
            view["sources"] = entry.sources.len().into();
            view["ts"] = ts(event.ts).into();
            let root = event.thread_root();
            if root != event.id {
                view["thread"] = root.into();
                let thread_title = roots
                    .entry(root.to_string())
                    .or_insert_with(|| store.get(root).ok().flatten().map(|r| title(&r.event)));
                if let Some(t) = thread_title {
                    view["thread_title"] = t.clone().into();
                }
            }
            views.push(view);
            if views.len() >= limit {
                break 'scan i + 1 < batch.len() || rest;
            }
        }
        if !rest {
            break false;
        }
        if scanned >= KNOWLEDGE_SCAN {
            break true;
        }
    };
    out["entries"] = views.into();
    out["more"] = more.into();
    if more && let Some((ts, seq)) = after {
        let next = Cursor {
            page: Page::Knowledge(ts, seq),
            ..page.clone()
        };
        out["cursor"] = next.encode(fingerprint).into();
    }
    if hidden > 0 {
        out["hidden_low"] = hidden.into();
    }
    Ok(())
}

/// One line of a thread outline.
struct Line {
    id: String,
    parent: Option<String>,
    kind: String,
    author: String,
    title: String,
    ts: u64,
    refs: Vec<String>,
    /// `data.via`, short (ADR 0036).
    via: Option<String>,
}

impl Line {
    fn of(event: &Event) -> Self {
        Self {
            id: event.id.clone(),
            parent: event.parent.clone(),
            kind: event.kind.as_str().to_string(),
            author: event.author.clone(),
            title: title(event),
            ts: event.ts,
            refs: event.refs.clone(),
            via: crate::via::compact(&event.data),
        }
    }

    /// A post whose body this node dropped (ADR 0020 §3): its header only.
    fn dropped(tombstone: &Value, reason: crate::store::DropReason) -> Option<Self> {
        let text = |key: &str| tombstone.get(key).and_then(Value::as_str);
        Some(Self {
            id: text("id")?.to_string(),
            parent: text("parent").map(str::to_string),
            kind: shown(text("kind")?),
            author: text("author")?.to_string(),
            title: format!("(body dropped: {reason})"),
            ts: tombstone.get("ts").and_then(Value::as_u64)?,
            refs: Vec::new(),
            via: None,
        })
    }
}

/// One thread as a tree: depth-first from the root, replies under their
/// parent, oldest first. Replies whose parent is not here hang at depth 1.
/// Posts whose body this node dropped keep their place, as "(body dropped:
/// reason)". Each post shows what it cites (`refs`) and how many posts
/// outside the thread cite it (`cited_by`); the thread's knowledge entry
/// is marked, or shown first when it sits elsewhere.
fn outline(store: &Store, root: &str, lens: &Lens) -> Result<Value> {
    let posts = store.thread(root, MAX_OUTLINE)?;
    let truncated = posts.len() >= MAX_OUTLINE;
    let mut lines: Vec<Line> = posts.iter().map(|s| Line::of(&s.event)).collect();
    for tombstone in store.thread_opaque(root, MAX_OUTLINE)? {
        let Some(id) = tombstone.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(record) = store.drop_record(id)?
            && let Some(line) = Line::dropped(&tombstone, record.reason)
        {
            lines.push(line);
        }
    }
    // Oldest first, as the store gives each part.
    lines.sort_by_key(|l| l.ts);
    let Some(root_line) = lines.iter().find(|l| l.id == root) else {
        bail!("no thread {root} on this node");
    };
    let ids: Vec<String> = lines.iter().map(|l| l.id.clone()).collect();
    let cited = store.cited_elsewhere(&ids, root)?;
    let knowledge = knowledge_of(store, lens, &[root.to_string()], &HashMap::new());
    let entry = knowledge.threads.get(root);
    let here: HashSet<&str> = lines.iter().map(|l| l.id.as_str()).collect();
    let mut children: HashMap<&str, Vec<&Line>> = HashMap::new();
    for line in &lines {
        if line.id == root {
            continue;
        }
        let parent = line
            .parent
            .as_deref()
            .filter(|p| here.contains(p))
            .unwrap_or(root);
        children.entry(parent).or_default().push(line);
    }
    let mut out = Vec::new();
    let mut stack: Vec<(&Line, usize)> = vec![(root_line, 0)];
    while let Some((line, depth)) = stack.pop() {
        let mut view = json!({
            "id": line.id,
            "depth": depth,
            "kind": line.kind,
            "author": line.author.get(..12).unwrap_or(&line.author),
            "title": line.title,
            "ts": ts(line.ts),
        });
        if let Some(name) = lens.names.get(&line.author) {
            view["name"] = name.clone().into();
        }
        if let Some(via) = &line.via {
            view["via"] = via.clone().into();
        }
        if !line.refs.is_empty() {
            view["refs"] = line
                .refs
                .iter()
                .take(REFS_SHOWN)
                .cloned()
                .collect::<Vec<_>>()
                .into();
            if line.refs.len() > REFS_SHOWN {
                view["refs_total"] = line.refs.len().into();
            }
        }
        if let Some(n) = cited.get(&line.id) {
            view["cited_by"] = (*n).into();
        }
        if entry.is_some_and(|e| e.id() == line.id) {
            view["knowledge"] = true.into();
        }
        out.push(view);
        if let Some(kids) = children.get(line.id.as_str()) {
            // Pushed in reverse so the oldest comes out first.
            for kid in kids.iter().rev() {
                stack.push((kid, depth.saturating_add(1)));
            }
        }
    }
    let citing: Vec<Value> = store
        .citing(root, root, CITING_SHOWN)?
        .iter()
        .map(|s| json!({"id": s.event.id, "thread": s.event.thread_root(), "title": title(&s.event)}))
        .collect();
    let mut result = json!({
        "thread": root,
        "posts": out,
        "truncated": truncated,
        "cited_by": cited.get(root).copied().unwrap_or(0),
        "hint": "open a post with `read`; `citing` lists posts elsewhere that cite the root",
    });
    if !citing.is_empty() {
        result["citing"] = citing.into();
    }
    if let Some(entry) = entry.filter(|e| !here.contains(e.id())) {
        result["knowledge"] = entry_brief(entry, &knowledge);
    }
    Ok(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::event::Draft;
    use ed25519_dalek::SigningKey;

    fn lens() -> Lens {
        Lens {
            grouping: Grouping::new(HashSet::new()),
            names: HashMap::new(),
            threshold: crate::reputation::DEFAULT_THRESHOLD,
            now: 1_000,
        }
    }

    fn browse(store: &Store, query: &Query) -> Result<Value> {
        super::browse(store, query, &lens())
    }

    fn post(store: &Store, key: u8, ts: u64, draft: Draft) -> Event {
        let event = draft
            .sign(&SigningKey::from_bytes(&[key; 32]), ts, 0)
            .unwrap();
        store.insert(&event, ts).unwrap();
        event
    }

    fn root(kind: Kind, title: &str, tags: &[&str]) -> Draft {
        Draft {
            net: "public".into(),
            kind: Some(kind),
            title: Some(title.into()),
            text: title.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..Draft::default()
        }
    }

    fn reply(to: &Event, kind: Kind, text: &str) -> Draft {
        Draft {
            net: "public".into(),
            thread: Some(to.thread_root().to_string()),
            parent: Some(to.id.clone()),
            kind: Some(kind),
            text: text.into(),
            ..Draft::default()
        }
    }

    fn ids(list: &Value) -> Vec<String> {
        list.as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn maps_tags_threads_and_outlines() {
        let store = Store::open_in_memory().unwrap();
        let rust = post(
            &store,
            1,
            100,
            root(Kind::Ask, "Rust on musl", &["cap:rust", "lang:en"]),
        );
        let pdf = post(&store, 1, 200, root(Kind::Ask, "Split a PDF", &["cap:pdf"]));
        let note = post(&store, 2, 150, root(Kind::Note, "A note", &["cap:rust"]));
        let answer = post(
            &store,
            2,
            300,
            reply(&rust, Kind::Answer, "Install musl-tools."),
        );
        post(&store, 1, 310, reply(&answer, Kind::React, "Thanks"));
        // An answer by the asker does not answer the ask.
        post(&store, 1, 320, reply(&pdf, Kind::Answer, "Never mind"));

        let map = browse(&store, &Query::default()).unwrap();
        let tags = map["tags"].as_array().unwrap();
        assert_eq!(tags[0]["tag"], "cap:rust");
        assert_eq!(tags[0]["threads"], 2);
        assert_eq!(map["tags_more"], false);
        // Latest activity first: the PDF thread (320), then Rust (310).
        let threads = map["threads"].as_array().unwrap();
        assert_eq!(threads[0]["id"], pdf.id.as_str());
        assert_eq!(threads[0]["open"], true);
        assert_eq!(threads[0]["status"], "unverified");
        assert_eq!(threads[1]["id"], rust.id.as_str());
        assert_eq!(threads[1]["replies"], 2);
        assert_eq!(threads[1]["answers"], 1);
        assert_eq!(threads[1]["open"], false);
        assert!(
            threads[2].get("open").is_none(),
            "only asks are open or not"
        );
        assert!(map.get("next_offset").is_none());
        assert_eq!(map["more"], false);
        assert!(map.get("cursor").is_none());

        let rust_only = browse(
            &store,
            &Query {
                tag: Some("cap:rust".into()),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(
            ids(&rust_only["threads"]),
            [rust.id.as_str(), note.id.as_str()]
        );
        let caps = browse(
            &store,
            &Query {
                prefix: Some("lang:".into()),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(caps["tags"].as_array().unwrap().len(), 1);
        let asks = browse(
            &store,
            &Query {
                kind: Some(Kind::Ask),
                limit: Some(1),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(asks["threads"].as_array().unwrap().len(), 1);
        assert_eq!(asks["next_offset"], 1);
        assert_eq!(asks["more"], true);

        let tree = browse(
            &store,
            &Query {
                thread: Some(rust.id.clone()),
                ..Query::default()
            },
        )
        .unwrap();
        let posts = tree["posts"].as_array().unwrap();
        let depths: Vec<u64> = posts.iter().map(|p| p["depth"].as_u64().unwrap()).collect();
        assert_eq!(depths, [0, 1, 2]);
        assert_eq!(posts[1]["kind"], "answer");
        assert!(
            browse(
                &store,
                &Query {
                    thread: Some("ab".repeat(32)),
                    ..Query::default()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn tags_are_directories() {
        let store = Store::open_in_memory().unwrap();
        let a = post(
            &store,
            1,
            100,
            root(Kind::Ask, "async", &["lang:rust:async", "cap:x"]),
        );
        let b = post(&store, 2, 200, root(Kind::Note, "rust", &["lang:rust"]));
        let c = post(&store, 3, 300, root(Kind::Note, "go", &["lang:go"]));
        let d = post(&store, 3, 400, root(Kind::Note, "web", &["lang:rust:web"]));
        post(&store, 4, 500, root(Kind::Note, "bare", &["lang"]));
        let dir = |level: &str| {
            browse(
                &store,
                &Query {
                    dir: Some(level.into()),
                    ..Query::default()
                },
            )
            .unwrap()
        };
        // The roots: `lang` (5 threads, 3 sub-tags) before `cap`.
        let top = dir("");
        let roots: Vec<(&str, u64, u64)> = top["dirs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["tag"].as_str().unwrap(),
                    d["threads"].as_u64().unwrap(),
                    d["children"].as_u64().unwrap_or(0),
                )
            })
            .collect();
        assert_eq!(roots, [("lang", 5, 2), ("cap", 1, 1)]);
        assert!(top.get("tags").is_none(), "`dir` lists directories only");
        assert_eq!(top["threads"].as_array().unwrap().len(), 5);
        // One level down: `lang:rust` stands for its sub-tags too.
        let lang = dir("lang");
        assert_eq!(lang["dir"], "lang");
        let children: Vec<(&str, u64, u64)> = lang["dirs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["tag"].as_str().unwrap(),
                    d["threads"].as_u64().unwrap(),
                    d["children"].as_u64().unwrap_or(0),
                )
            })
            .collect();
        assert_eq!(children, [("lang:rust", 3, 2), ("lang:go", 1, 0)]);
        assert_eq!(lang["threads"].as_array().unwrap().len(), 5);
        // The threads of a level: its own tag and every tag below it.
        let rust = dir("lang:rust:");
        assert_eq!(
            ids(&rust["threads"]),
            [d.id.as_str(), b.id.as_str(), a.id.as_str()]
        );
        assert_eq!(rust["dirs"].as_array().unwrap().len(), 2);
        // A prefix ending with `:` opens its directory too.
        let by_prefix = browse(
            &store,
            &Query {
                prefix: Some("lang:".into()),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(by_prefix["dirs"], lang["dirs"]);
        assert_eq!(by_prefix["threads"], lang["threads"]);
        assert_eq!(by_prefix["tags"][0]["tag"], "lang:rust:web", "{by_prefix}");
        assert!(ids(&lang["threads"]).contains(&c.id));
        // Facets combine with the directory: the kind of the root.
        let asks = browse(
            &store,
            &Query {
                dir: Some("lang".into()),
                kind: Some(Kind::Ask),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(ids(&asks["threads"]), [a.id.as_str()]);
        assert_eq!(asks["dirs"][0]["tag"], "lang:rust");
        assert_eq!(asks["dirs"][0]["threads"], 1);
    }

    #[test]
    fn author_and_period_narrow_the_threads() {
        let store = Store::open_in_memory().unwrap();
        let old = post(&store, 1, 1_000, root(Kind::Ask, "old", &["cap:x"]));
        let new = post(&store, 2, 5_000, root(Kind::Ask, "new", &["cap:x"]));
        // 3 replied to the old thread, late.
        post(&store, 3, 9_000, reply(&old, Kind::Answer, "late"));
        let three = crate::identity::author_id(&SigningKey::from_bytes(&[3; 32]));
        let query = |author: Option<&str>, since: Option<u64>, until: Option<u64>| {
            let map = browse(
                &store,
                &Query {
                    author: author.map(str::to_string),
                    since,
                    until,
                    ..Query::default()
                },
            )
            .unwrap();
            (ids(&map["threads"]), map["dirs"][0]["threads"].clone())
        };
        let (by_three, cap) = query(Some(&three), None, None);
        assert_eq!(by_three, std::slice::from_ref(&old.id));
        assert_eq!(cap, 1);
        // A thread is in a period when one of its posts is.
        assert_eq!(
            query(None, Some(4_000), None).0,
            [old.id.clone(), new.id.clone()]
        );
        assert_eq!(
            query(None, Some(4_000), Some(6_000)).0,
            std::slice::from_ref(&new.id)
        );
        assert_eq!(query(None, None, Some(1_000)).0, Vec::<String>::new());
        assert_eq!(
            query(Some(&three), None, Some(8_000)).0,
            Vec::<String>::new()
        );
    }

    #[test]
    fn times_and_authors_parse() {
        let now = 1_792_326_896;
        assert_eq!(parse_time("2026-10-18T12:34:56Z", now), Ok(now));
        assert_eq!(parse_time("2026-10-18", now), Ok(now - 45_296));
        assert_eq!(parse_time("1700000000", now), Ok(1_700_000_000));
        assert_eq!(parse_time("7d", now), Ok(now - 7 * 86_400));
        assert_eq!(parse_time("2h ago", now), Ok(now - 7_200));
        for bad in ["", "yesterday", "2026-13-01", "7w", "0d"] {
            assert!(parse_time(bad, now).is_err(), "{bad}");
        }
        let hex = crate::identity::author_id(&SigningKey::from_bytes(&[5; 32]));
        let to = crate::delivery::to_tag(&hex).unwrap();
        assert_eq!(parse_author(&hex.to_uppercase()), Ok(hex.clone()));
        assert_eq!(parse_author(&to), Ok(hex.clone()));
        assert_eq!(parse_author(&to[3..].to_uppercase()), Ok(hex.clone()));
        assert!(parse_author(&hex[..12]).is_err());
    }

    #[test]
    fn cursors_page_through_a_snapshot() {
        let store = Store::open_in_memory().unwrap();
        let mut all = Vec::new();
        for i in 0..7_u64 {
            all.push(post(
                &store,
                1,
                100 + i,
                root(Kind::Note, &format!("n{i}"), &[]),
            ));
        }
        let page = |cursor: Option<String>| {
            browse(
                &store,
                &Query {
                    limit: Some(3),
                    cursor,
                    ..Query::default()
                },
            )
            .unwrap()
        };
        let first = page(None);
        assert_eq!(first["more"], true);
        let mut seen = ids(&first["threads"]);
        // Arrivals between pages: a reply that would move an unseen
        // thread to the top, and a new thread.
        post(&store, 2, 900, reply(&all[0], Kind::Answer, "bump"));
        post(&store, 2, 901, root(Kind::Note, "later", &[]));
        let mut cursor = first["cursor"].as_str().map(str::to_string);
        while let Some(c) = cursor {
            let next = page(Some(c));
            assert!(next.get("tags").is_none() && next.get("dirs").is_none());
            seen.extend(ids(&next["threads"]));
            cursor = next["cursor"].as_str().map(str::to_string);
            assert_eq!(next["more"], cursor.is_some());
        }
        let expected: Vec<String> = all.iter().rev().map(|e| e.id.clone()).collect();
        assert_eq!(seen, expected);
        // A fresh browse sees the arrivals.
        let fresh = page(None);
        assert_eq!(fresh["threads"][1]["id"], all[0].id.as_str());
        // A cursor belongs to its query.
        let other = browse(
            &store,
            &Query {
                kind: Some(Kind::Ask),
                cursor: first["cursor"].as_str().map(str::to_string),
                ..Query::default()
            },
        );
        assert!(other.unwrap_err().to_string().contains("another browse"));
        for bad in ["x", "t1.1.2.3", "t1.1...2.3.zz", "d1.1...2"] {
            let result = browse(
                &store,
                &Query {
                    limit: Some(3),
                    cursor: Some(bad.into()),
                    ..Query::default()
                },
            );
            assert!(result.is_err(), "{bad}");
        }
    }

    #[test]
    fn outlines_show_citations() {
        let store = Store::open_in_memory().unwrap();
        let ask = post(&store, 1, 100, root(Kind::Ask, "How?", &[]));
        let answer = post(&store, 2, 200, reply(&ask, Kind::Answer, "So."));
        let elsewhere = post(
            &store,
            3,
            300,
            Draft {
                refs: {
                    let mut refs = vec![ask.id.clone(), answer.id.clone()];
                    refs.sort();
                    refs
                },
                ..root(Kind::Note, "Builds on it", &[])
            },
        );
        // A citation inside the thread does not count.
        post(
            &store,
            1,
            400,
            Draft {
                refs: vec![elsewhere.id.clone()],
                ..reply(&answer, Kind::Note, "See also")
            },
        );
        let tree = browse(
            &store,
            &Query {
                thread: Some(ask.id.clone()),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(tree["cited_by"], 1);
        assert_eq!(tree["citing"][0]["id"], elsewhere.id.as_str());
        assert_eq!(tree["citing"][0]["title"], "Builds on it");
        let posts = tree["posts"].as_array().unwrap();
        assert_eq!(posts[0]["cited_by"], 1);
        assert_eq!(posts[1]["cited_by"], 1);
        assert_eq!(posts[2]["refs"], json!([elsewhere.id]));
        assert!(posts[2].get("cited_by").is_none());
        // The citing post's own outline: it cites, nothing cites it
        // from elsewhere but the reply.
        let other = browse(
            &store,
            &Query {
                thread: Some(elsewhere.id.clone()),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(other["cited_by"], 1);
        assert_eq!(other["posts"][0]["refs"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn dropped_posts_keep_their_place_in_the_outline() {
        use crate::store::DropReason;
        let store = Store::open_in_memory().unwrap();
        let ask = post(
            &store,
            1,
            100,
            root(Kind::Ask, "A leaked key", &["cap:rust"]),
        );
        let answer = post(&store, 2, 200, reply(&ask, Kind::Answer, "Rotate it."));
        post(&store, 1, 300, reply(&answer, Kind::React, "Done"));
        store
            .drop_body(&ask.id, DropReason::Own, None, None, 400)
            .unwrap();
        store
            .drop_body(&answer.id, DropReason::Retraction, None, None, 400)
            .unwrap();
        let tree = browse(
            &store,
            &Query {
                thread: Some(ask.id.clone()),
                ..Query::default()
            },
        )
        .unwrap();
        let posts = tree["posts"].as_array().unwrap();
        let shape: Vec<(u64, &str)> = posts
            .iter()
            .map(|p| (p["depth"].as_u64().unwrap(), p["title"].as_str().unwrap()))
            .collect();
        assert_eq!(
            shape,
            [
                (0, "(body dropped: own)"),
                (1, "(body dropped: retraction)"),
                (2, "Done"),
            ]
        );
        assert_eq!(posts[0]["kind"], "ask");
        // The map leaves the dropped root out.
        let map = browse(&store, &Query::default()).unwrap();
        assert!(map["threads"].as_array().unwrap().is_empty());
        assert!(map["tags"].as_array().unwrap().is_empty());
        assert!(map["dirs"].as_array().unwrap().is_empty());
    }
}
