//! Node names (ADR 0023 §8): a short name for each node of this machine, so
//! that `wats ps` tells them apart and `-n NAME` picks one.
//!
//! - **Local only.** A node's name lives in its data directory and in
//!   nothing it sends: never in an event, the hello or an announce. Other
//!   nodes never learn it; it says nothing about the person.
//! - **Drawn once.** A node with no name gets one at its first start (or
//!   from the setup wizard): a Docker-style `adjective_creature`
//!   ([`ADJECTIVES`] × [`CREATURES`], never a person's name), drawn again
//!   while another data directory of this machine has it. A fleet's nodes
//!   are `node-<i>`, or `<fleet>-<i>` when another node has that name.
//! - **Where.** A drawn name goes to `<data dir>/node-name` ([`FILE`]), not
//!   to config.toml: wats 0.4.4 and older refuse a config.toml with a key
//!   they do not know, so a name there would break `wats upgrade
//!   --rollback` for every node. `node.name` in config.toml, set by `wats
//!   config set node.name`, wins over the file; a rollback refuses while
//!   it is set (`update::KEYS_SINCE`) and says to unset it first.
//! - **Unnamed nodes.** A data directory whose node has not started since
//!   0.4.5 (an older node still running) shows as `default` (`~/.wats`) or
//!   its directory's name until its next start.

use std::path::{Path, PathBuf};

/// The file of a drawn name, in the data directory: the name and a
/// newline.
pub const FILE: &str = "node-name";

/// The longest node name.
pub const MAX_LEN: usize = 32;

/// The first half of a drawn name: playful but kind, never a person's
/// name.
pub const ADJECTIVES: [&str; 60] = [
    "cheeky", "sleepy", "turbo", "dizzy", "brave", "nimble", "grumpy", "jolly", "sneaky", "fuzzy",
    "zippy", "cosmic", "bouncy", "breezy", "bubbly", "chirpy", "cozy", "curious", "dapper",
    "dreamy", "eager", "fluffy", "giddy", "gentle", "gleeful", "glowing", "humble", "jazzy",
    "jumpy", "kind", "lucky", "mellow", "cheery", "mighty", "nifty", "peppy", "perky", "plucky",
    "polite", "quirky", "snappy", "snug", "sparkly", "speedy", "spry", "tidy", "tiny", "upbeat",
    "witty", "wobbly", "zesty", "clever", "cuddly", "daring", "frisky", "happy", "lively", "shiny",
    "silly", "starry",
];

/// The second half: creatures that swarm, flock or school.
pub const CREATURES: [&str; 40] = [
    "bee",
    "ant",
    "starling",
    "firefly",
    "termite",
    "wasp",
    "hornet",
    "krill",
    "sardine",
    "locust",
    "mayfly",
    "midge",
    "swift",
    "bat",
    "herring",
    "gnat",
    "moth",
    "cicada",
    "ladybug",
    "minnow",
    "anchovy",
    "mackerel",
    "capelin",
    "shrimp",
    "squid",
    "butterfly",
    "dragonfly",
    "cricket",
    "grasshopper",
    "aphid",
    "quelea",
    "dunlin",
    "swallow",
    "flamingo",
    "plover",
    "penguin",
    "pigeon",
    "parakeet",
    "goose",
    "jellyfish",
];

/// Check a node name: 1-[`MAX_LEN`] chars of `[a-z0-9_-]`, starting with
/// a letter or a digit, and not only digits (`-n 50` is never a name).
pub fn check(name: &str) -> Result<(), String> {
    let charset = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    let first = name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if name.len() > MAX_LEN || !charset || !first || name.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "node name {name:?}: 1-{MAX_LEN} chars of [a-z0-9_-], starting with a letter or a \
             digit, not only digits"
        ));
    }
    Ok(())
}

/// Draw an `adjective_creature` name that is not in `taken`, `random`
/// giving the random numbers: a few draws, then a number after the name
/// (`cheeky_bee_2`) when they all clash.
pub fn draw(mut random: impl FnMut() -> u32, taken: &[String]) -> String {
    let pick = |n: u32, len: usize| usize::try_from(n).unwrap_or(0) % len;
    let mut name = String::new();
    for _ in 0..64 {
        name = format!(
            "{}_{}",
            ADJECTIVES[pick(random(), ADJECTIVES.len())],
            CREATURES[pick(random(), CREATURES.len())]
        );
        if !taken.contains(&name) {
            return name;
        }
    }
    (2..)
        .map(|n| format!("{name}_{n}"))
        .find(|n| !taken.contains(n))
        .unwrap_or(name)
}

/// [`draw`] with the OS's random numbers.
pub fn draw_random(taken: &[String]) -> String {
    draw(
        || {
            let mut bytes = [0u8; 4];
            // No randomness: the first name, and a number on a clash.
            let _ = getrandom::fill(&mut bytes);
            u32::from_le_bytes(bytes)
        },
        taken,
    )
}

/// `node.name` in `data_dir`'s config.toml, when set and valid.
pub fn configured(data_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join(crate::settings::FILE_NAME)).ok()?;
    let table: toml::Table = toml::from_str(&text).ok()?;
    let name = table.get("node")?.get("name")?.as_str()?;
    check(name).ok().map(|()| name.to_string())
}

/// The name drawn for `data_dir` ([`FILE`]), when there is one.
pub fn generated(data_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join(FILE)).ok()?;
    let name = text.trim();
    check(name).ok().map(|()| name.to_string())
}

/// The node's name: config.toml's `node.name`, else the drawn one.
pub fn stored(data_dir: &Path) -> Option<String> {
    configured(data_dir).or_else(|| generated(data_dir))
}

/// What an unnamed node is called: `default` for `~/.wats`, else its
/// directory's name.
pub fn fallback(data_dir: &Path, default_dir: Option<&Path>) -> String {
    if default_dir.is_some_and(|d| same_dir(d, data_dir)) {
        return "default".to_string();
    }
    data_dir
        .file_name()
        .map_or_else(|| "-".to_string(), |n| n.to_string_lossy().into_owned())
}

/// The name shown for `data_dir`: [`stored`], else [`fallback`].
pub fn display(data_dir: &Path, default_dir: Option<&Path>) -> String {
    stored(data_dir).unwrap_or_else(|| fallback(data_dir, default_dir))
}

/// `~/.wats`, the default data directory.
pub fn default_dir() -> Option<PathBuf> {
    std::env::home_dir().map(|h| h.join(".wats"))
}

/// Whether two paths are the same directory.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    let key = |p: &Path| {
        std::fs::canonicalize(p)
            .or_else(|_| std::path::absolute(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    key(a) == key(b)
}

/// The data directories of this machine: `default_dir`, then those of
/// the registry, running ([`crate::background::registered`]) or not
/// ([`crate::background::known`]), once each.
pub fn machine_dirs(registry: Option<&Path>, default_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = default_dir.map(Path::to_path_buf).into_iter().collect();
    if let Some(registry) = registry {
        dirs.extend(crate::background::registered(registry));
        dirs.extend(crate::background::known(registry));
    }
    let mut unique: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let dir = std::path::absolute(&dir).unwrap_or(dir);
        if !unique.iter().any(|d| same_dir(d, &dir)) {
            unique.push(dir);
        }
    }
    unique
}

/// The names of `dirs` but `except`.
pub fn taken(dirs: &[PathBuf], except: &Path, default_dir: Option<&Path>) -> Vec<String> {
    dirs.iter()
        .filter(|d| !same_dir(d, except))
        .map(|d| display(d, default_dir))
        .collect()
}

/// The name of a fleet's node (ADR 0028), when `data_dir` is one:
/// `node-<i>`, else `<fleet>-<i>` (the fleet directory's name) when
/// another node has it. None when it is no fleet node, or both are taken.
pub fn fleet_name(data_dir: &Path, taken: &[String]) -> Option<String> {
    let base = data_dir.parent()?;
    let own = data_dir.file_name()?.to_str()?;
    let manifest = crate::fleet::Manifest::load(base).ok()??;
    if !manifest.nodes.iter().any(|n| n.name == own) {
        return None;
    }
    let i: usize = own.strip_prefix("node-")?.parse().ok()?;
    let plain = crate::fleet::node_name(i);
    if !taken.contains(&plain) {
        return Some(plain);
    }
    let suffix = format!("-{i}");
    let fleet: String = base
        .file_name()?
        .to_string_lossy()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '_' | '-' => c,
            _ => '-',
        })
        .collect();
    let fleet = fleet.trim_matches(|c| c == '-' || c == '_');
    let fleet: String = fleet
        .chars()
        .take(MAX_LEN.saturating_sub(suffix.len()))
        .collect();
    let prefixed = format!("{fleet}{suffix}");
    (check(&prefixed).is_ok() && !taken.contains(&prefixed)).then_some(prefixed)
}

/// Write `name` as `data_dir`'s drawn name ([`FILE`]), replacing any.
pub fn write(data_dir: &Path, name: &str) -> std::io::Result<()> {
    check(name).map_err(std::io::Error::other)?;
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(data_dir.join(FILE), format!("{name}\n"))
}

/// The name of `data_dir`'s node, drawn and written now if it has none
/// (its first start): a fleet's node name, else an `adjective_creature`
/// that no other data directory of this machine has. The machine's
/// directories are those of `registry`, and `~/.wats` with a registry only
/// (a hermetic process without one reads nothing of the home).
pub fn ensure(data_dir: &Path, registry: Option<&Path>) -> std::io::Result<String> {
    if let Some(name) = stored(data_dir) {
        return Ok(name);
    }
    let default = registry.and(default_dir());
    let dirs = machine_dirs(registry, default.as_deref());
    let taken = taken(&dirs, data_dir, default.as_deref());
    let name = fleet_name(data_dir, &taken).unwrap_or_else(|| draw_random(&taken));
    std::fs::create_dir_all(data_dir)?;
    // Another process drawing at the same time: the first name stays.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(data_dir.join(FILE))
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(format!("{name}\n").as_bytes())?;
            Ok(name)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(generated(data_dir).unwrap_or(name))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lists_are_kind_unique_and_valid() {
        assert!(ADJECTIVES.len() >= 60 && CREATURES.len() >= 40);
        for list in [&ADJECTIVES[..], &CREATURES[..]] {
            let mut sorted = list.to_vec();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), list.len(), "no word twice");
        }
        // Never a person's name, nor a word that reads as one.
        for name in [
            "robin", "jay", "martin", "wren", "finch", "drake", "ray", "sunny", "rusty", "sandy",
            "misty", "frank", "earnest", "merry", "piper", "sparrow",
        ] {
            assert!(
                !ADJECTIVES.contains(&name) && !CREATURES.contains(&name),
                "{name}"
            );
        }
        for adjective in ADJECTIVES {
            for creature in CREATURES {
                let name = format!("{adjective}_{creature}");
                check(&name).unwrap();
            }
        }
    }

    #[test]
    fn names_are_checked() {
        for good in [
            "cheeky_starling",
            "node-1",
            "a",
            "x1",
            "1x",
            &"a".repeat(32),
        ] {
            check(good).unwrap();
        }
        for bad in [
            "",
            "50",
            "-a",
            "_a",
            "Cheeky",
            "a b",
            "a/b",
            "a.b",
            "é",
            &"a".repeat(33),
        ] {
            assert!(check(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_draw_is_adjective_creature_and_never_taken() {
        let mut n = 0u32;
        let counter = || {
            n += 1;
            n
        };
        let first = draw(counter, &[]);
        let (adjective, creature) = first.split_once('_').unwrap();
        assert!(ADJECTIVES.contains(&adjective) && CREATURES.contains(&creature));
        // A clash draws again.
        let zero = || 0;
        let taken = vec![draw(zero, &[])];
        assert_eq!(taken[0], "cheeky_bee");
        let mut calls = 0u32;
        let again = draw(
            || {
                calls += 1;
                // The first draw (two numbers) clashes, the second does not.
                if calls <= 2 { 0 } else { 1 }
            },
            &taken,
        );
        assert_eq!(again, "sleepy_ant");
        // Every draw clashing: a number after the name.
        let taken = vec!["cheeky_bee".to_string(), "cheeky_bee_2".to_string()];
        assert_eq!(draw(|| 0, &taken), "cheeky_bee_3");
        // Random draws: valid, and none of a long taken list.
        let taken: Vec<String> = (0..2000).map(|_| draw_random(&[])).collect();
        let name = draw_random(&taken);
        assert!(!taken.contains(&name));
        check(&name).unwrap();
    }

    #[test]
    fn a_name_is_drawn_once_and_config_toml_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        assert_eq!(stored(&dir), None);
        assert_eq!(fallback(&dir, None), "data");
        assert_eq!(fallback(&dir, Some(&dir)), "default");
        let name = ensure(&dir, None).unwrap();
        check(&name).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(FILE)).unwrap(),
            format!("{name}\n")
        );
        // Every later start keeps it.
        for _ in 0..3 {
            assert_eq!(ensure(&dir, None).unwrap(), name);
        }
        // config.toml's node.name wins; unset, the drawn one is back.
        std::fs::write(dir.join("config.toml"), "[node]\nname = \"hive-1\"\n").unwrap();
        assert_eq!(stored(&dir).as_deref(), Some("hive-1"));
        assert_eq!(ensure(&dir, None).unwrap(), "hive-1");
        std::fs::write(dir.join("config.toml"), "").unwrap();
        assert_eq!(display(&dir, None), name);
        // An invalid file is no name.
        std::fs::write(dir.join(FILE), "Not A Name\n").unwrap();
        assert_eq!(stored(&dir), None);
    }

    #[test]
    fn a_draw_avoids_the_names_of_the_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = tmp.path().join("run");
        let others: Vec<PathBuf> = (0..3).map(|i| tmp.path().join(format!("n{i}"))).collect();
        for (i, dir) in others.iter().enumerate() {
            write(dir, &format!("taken_{i}")).unwrap();
            crate::background::register(&registry, dir).unwrap();
            crate::background::unregister(&registry, dir);
        }
        let dirs = machine_dirs(Some(&registry), None);
        assert_eq!(dirs.len(), 3, "stopped nodes are still known: {dirs:?}");
        let mine = tmp.path().join("mine");
        let taken = taken(&dirs, &mine, None);
        assert_eq!(taken, ["taken_0", "taken_1", "taken_2"]);
        let name = ensure(&mine, Some(&registry)).unwrap();
        assert!(!taken.contains(&name));
    }

    #[test]
    fn fleet_nodes_are_node_i_else_fleet_i() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join(".Support Fleet");
        let manifest = serde_json::json!({
            "version": 1, "created": "2026-10-22 00:00:00Z", "template": "/t",
            "nodes": [{"name": "node-1", "endpoint_id": "x", "port": 1},
                      {"name": "node-2", "endpoint_id": "y", "port": 2}],
        });
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("fleet.json"), manifest.to_string()).unwrap();
        let node = base.join("node-2");
        assert_eq!(fleet_name(&node, &[]).as_deref(), Some("node-2"));
        let taken = vec!["node-2".to_string()];
        assert_eq!(
            fleet_name(&node, &taken).as_deref(),
            Some("support-fleet-2")
        );
        assert_eq!(ensure(&node, None).unwrap(), "node-2");
        // Not in the manifest: no fleet name.
        assert_eq!(fleet_name(&base.join("node-3"), &[]), None);
        assert_eq!(fleet_name(&tmp.path().join("node-1"), &[]), None);
    }
}
