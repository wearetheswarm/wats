//! What `share_finding` shared (ADR 0056 §2), kept in
//! `<data dir>/`[`FILE`], one JSON line per share, for `wats outbox`.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

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
    /// Shared under `share.auto`, without the person's OK for it.
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
