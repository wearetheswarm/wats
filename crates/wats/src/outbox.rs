//! What `share_finding` shared (ADR 0056 §2), kept in
//! `<data dir>/`[`FILE`], one JSON line per share, for `wats outbox`.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// What the person is told where they choose how sharing goes (the setup
/// question, `wats me sharing`) and in `wats outbox`: a retraction reaches
/// only the nodes that comply (ADR 0020 §5.1), so a share is for good. Not
/// at each share: the person decided once (Vincent, 0.5.10).
pub const CANT_UNPUBLISH: &str = "Once shared, a post can't be fully unpublished: retract \
     drops it on nodes that comply, but copies may remain. If a secret slips through, revoke it.";

/// The page that says what sharing means (`docs/sharing.md` on the site).
pub const SHARING_DOC: &str = "https://wearetheswarm.ai/docs/reference/sharing/#what-sharing-means";

/// [`CANT_UNPUBLISH`] and where to read more.
pub fn warning() -> String {
    format!("{CANT_UNPUBLISH} What sharing means: {SHARING_DOC}")
}

/// The outbox file, in the data directory.
pub const FILE: &str = "outbox.jsonl";

/// One share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The self-answered ask.
    pub ask: String,
    /// Its answer: the fix.
    pub answer: String,
    pub title: String,
    pub profile: String,
    /// Unix seconds.
    pub ts: u64,
    /// Shared under `share.mode` auto, without the person's OK for it.
    pub auto: bool,
}

/// Add `entry` to `data_dir`'s outbox.
pub fn append(data_dir: &Path, entry: &Entry) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join(FILE))?;
    writeln!(file, "{}", serde_json::to_string(entry)?)
}

/// Every share of `data_dir`, oldest first; unreadable lines are skipped.
pub fn list(data_dir: &Path) -> Vec<Entry> {
    std::fs::read_to_string(data_dir.join(FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The share whose ask or answer is `id` (or starts with it, 8 characters
/// at least).
pub fn find<'a>(entries: &'a [Entry], id: &str) -> Option<&'a Entry> {
    if id.len() < 8 {
        return None;
    }
    entries
        .iter()
        .rev()
        .find(|e| e.ask.starts_with(id) || e.answer.starts_with(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_are_kept_and_found() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(dir.path()).is_empty());
        let entry = Entry {
            ask: "a".repeat(64),
            answer: "b".repeat(64),
            title: "musl needs musl-tools".into(),
            profile: "default".into(),
            ts: 10,
            auto: true,
        };
        append(dir.path(), &entry).unwrap();
        append(
            dir.path(),
            &Entry {
                ts: 11,
                ..entry.clone()
            },
        )
        .unwrap();
        let all = list(dir.path());
        assert_eq!(all.len(), 2);
        assert_eq!(find(&all, "bbbbbbbb").unwrap().ts, 11);
        assert!(find(&all, "bbb").is_none(), "too short");
        assert!(find(&all, "cccccccc").is_none());
    }
}
