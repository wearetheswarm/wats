//! The privacy scrub of a shared finding (ADR 0056 §2): what goes out of
//! `share_finding` holds the generic problem, the fix and the evidence,
//! nothing of the project, the machine or the person.
//!
//! - **Replaced** with a placeholder: absolute and home paths, the OS user,
//!   host, home directory and account names, e-mail and IP addresses, URLs
//!   other than public registries and documentation, UUIDs and long hex.
//! - **Refused**, with the reason, so that the agent rewrites: what looks
//!   like a secret, a code block of more than [`MAX_CODE_LINES`] lines, and
//!   the project's own names (its packages and repository).

use std::path::Path;

/// The longest code block a finding may carry.
pub const MAX_CODE_LINES: usize = 30;

/// A token this long or longer, mixing letters and digits with enough
/// entropy, reads as a secret.
const SECRET_LEN: usize = 24;

/// Known secret prefixes.
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "glpat-",
    "AKIA",
    "ASIA",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "AIza",
    "ya29.",
    "npm_",
    "pypi-",
];

/// Hosts whose URLs are public knowledge, kept as they are.
const PUBLIC_HOSTS: &[&str] = &[
    "docs.rs",
    "crates.io",
    "doc.rust-lang.org",
    "rust-lang.github.io",
    "pypi.org",
    "docs.python.org",
    "www.npmjs.com",
    "npmjs.com",
    "nodejs.org",
    "developer.mozilla.org",
    "go.dev",
    "pkg.go.dev",
    "learn.microsoft.com",
    "man7.org",
    "en.wikipedia.org",
    "stackoverflow.com",
    "www.rfc-editor.org",
    "datatracker.ietf.org",
    "sqlite.org",
    "www.sqlite.org",
    "www.postgresql.org",
];

/// Project names too common to tell anything (a package called `app`).
const COMMON_NAMES: &[&str] = &[
    "app", "api", "cli", "core", "server", "client", "web", "www", "utils", "util", "common",
    "backend", "frontend", "test", "tests", "demo", "example", "examples", "lib", "main", "src",
    "docs", "tools", "scripts", "service", "worker", "project",
];

/// What the scrub is told about where it runs.
#[derive(Debug, Clone, Default)]
pub struct Context {
    /// The OS user, host, home and account names ([`crate::roster::private_words`]).
    pub private: Vec<(&'static str, String)>,
    /// The project's own names: packages, repository, directory.
    pub project: Vec<String>,
}

impl Context {
    /// The context of a share made from `dir` (the agent's working
    /// directory) on this machine.
    pub fn here(dir: Option<&Path>) -> Context {
        Context {
            private: crate::roster::private_words(),
            project: dir.map(project_names).unwrap_or_default(),
        }
    }
}

/// The project's names, read from `dir`: its directory, the `name` of its
/// `Cargo.toml` package and workspace members, `package.json`,
/// `pyproject.toml`, and its git remote's repository. Lowercased, the
/// common ones ([`COMMON_NAMES`]) and those under 4 characters left out.
pub fn project_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(n) = dir.file_name().and_then(|n| n.to_str()) {
        names.push(n.to_string());
    }
    let read = |file: &str| std::fs::read_to_string(dir.join(file)).ok();
    for file in ["Cargo.toml", "pyproject.toml"] {
        if let Some(text) = read(file)
            && let Ok(doc) = text.parse::<toml_edit::DocumentMut>()
        {
            for table in ["package", "project", "workspace"] {
                if let Some(name) = doc
                    .get(table)
                    .and_then(|t| t.get("name"))
                    .and_then(|n| n.as_str())
                {
                    names.push(name.to_string());
                }
            }
            if let Some(members) = doc
                .get("workspace")
                .and_then(|w| w.get("members"))
                .and_then(|m| m.as_array())
            {
                for m in members.iter().filter_map(|m| m.as_str()) {
                    if let Some(last) = m.rsplit('/').next()
                        && !last.contains('*')
                    {
                        names.push(last.to_string());
                    }
                }
            }
        }
    }
    if let Some(text) = read("package.json")
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(name) = json["name"].as_str()
    {
        names.push(name.trim_start_matches('@').replace('/', "-"));
        if let Some(scope) = name.strip_prefix('@').and_then(|n| n.split('/').next()) {
            names.push(scope.to_string());
        }
    }
    if let Some(text) = read(".git/config") {
        for line in text.lines() {
            let line = line.trim();
            if let Some(url) = line.strip_prefix("url = ") {
                let repo = url
                    .trim_end_matches('/')
                    .trim_end_matches(".git")
                    .rsplit(['/', ':'])
                    .next()
                    .unwrap_or_default();
                names.push(repo.to_string());
                // The organisation too.
                let parts: Vec<&str> = url.trim_end_matches(".git").rsplit(['/', ':']).collect();
                if let Some(org) = parts.get(1) {
                    names.push((*org).to_string());
                }
            }
        }
    }
    let mut out: Vec<String> = names
        .into_iter()
        .map(|n| n.trim().to_lowercase())
        .filter(|n| n.chars().count() >= 4 && !COMMON_NAMES.contains(&n.as_str()))
        .filter(|n| !n.contains(' ') && !n.contains('@'))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The scrubbed text, and what was replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scrubbed {
    pub text: String,
    /// What kinds of things were replaced, once each (`a path`, `an
    /// e-mail address`…), for the agent's result.
    pub replaced: Vec<&'static str>,
}

/// Scrub `text` (ADR 0056 §2): the replaced text, or why it is refused.
pub fn scrub(text: &str, ctx: &Context) -> Result<Scrubbed, Vec<String>> {
    let mut refused: Vec<String> = Vec::new();
    // Code blocks.
    let mut in_code = false;
    let mut code_lines = 0usize;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_code && code_lines > MAX_CODE_LINES {
                refused.push(format!(
                    "a code block of {code_lines} lines: more than {MAX_CODE_LINES} reads as private \
                     code; keep the few lines that show the fix"
                ));
            }
            in_code = !in_code;
            code_lines = 0;
        } else if in_code {
            code_lines += 1;
        }
        if line.contains("-----BEGIN") {
            refused.push("a PEM block (a key or a certificate)".into());
        }
    }
    if in_code && code_lines > MAX_CODE_LINES {
        refused.push(format!(
            "a code block of {code_lines} lines: more than {MAX_CODE_LINES} reads as private code"
        ));
    }
    let mut replaced: Vec<&'static str> = Vec::new();
    let mut out = String::with_capacity(text.len());
    for piece in text.split_inclusive(char::is_whitespace) {
        let body = piece.trim_end_matches(char::is_whitespace);
        let space = &piece[body.len()..];
        let (lead, core, trail) = trim_punct(body);
        let shown = match classify(core, ctx) {
            Class::Keep => core.to_string(),
            Class::Replace(what, with) => {
                if !replaced.contains(&what) {
                    replaced.push(what);
                }
                with.to_string()
            }
            Class::Refuse(why) => {
                if !refused.contains(&why) {
                    refused.push(why);
                }
                core.to_string()
            }
        };
        out.push_str(lead);
        out.push_str(&shown);
        out.push_str(trail);
        out.push_str(space);
    }
    // The project's names, anywhere in a word (`acme-billing` holds
    // `acme`).
    let lower = text.to_lowercase();
    for name in &ctx.project {
        if contains_word(&lower, name) {
            let why = format!(
                "the project's own name \"{name}\": a finding says nothing of the project; name \
                 the library, tool or error instead"
            );
            if !refused.contains(&why) {
                refused.push(why);
            }
        }
    }
    if refused.is_empty() {
        Ok(Scrubbed {
            text: out,
            replaced,
        })
    } else {
        Err(refused)
    }
}

/// Whether `name` is in `text` as a whole word: bounded by characters
/// other than letters and digits.
fn contains_word(text: &str, name: &str) -> bool {
    let bytes = text.as_bytes();
    text.match_indices(name).any(|(i, _)| {
        let before = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let end = i + name.len();
        let after = end >= bytes.len() || !bytes[end].is_ascii_alphanumeric();
        before && after
    })
}

/// `("(", "word", "),")`: the punctuation around a token.
fn trim_punct(token: &str) -> (&str, &str, &str) {
    let lead_chars = ['(', '[', '{', '"', '\'', '`', '<'];
    let trail_chars = [
        ')', ']', '}', '"', '\'', '`', '>', ',', ';', ':', '.', '!', '?',
    ];
    let start = token
        .char_indices()
        .find(|(_, c)| !lead_chars.contains(c))
        .map_or(token.len(), |(i, _)| i);
    let rest = &token[start..];
    let end = rest
        .char_indices()
        .rev()
        .find(|(_, c)| !trail_chars.contains(c))
        .map_or(0, |(i, c)| i + c.len_utf8());
    (&token[..start], &rest[..end], &rest[end..])
}

enum Class {
    Keep,
    Replace(&'static str, &'static str),
    Refuse(String),
}

fn classify(token: &str, ctx: &Context) -> Class {
    if token.is_empty() {
        return Class::Keep;
    }
    // `KEY=sk-…` and `token:ghp_…` too.
    let secret = token.split(['=', ':']).any(|part| {
        SECRET_PREFIXES
            .iter()
            .any(|p| part.starts_with(p) && part.len() > p.len() + 8)
    });
    if secret {
        return Class::Refuse("what looks like a secret: never share keys or tokens".into());
    }
    if let Some(url) = token
        .strip_prefix("https://")
        .or_else(|| token.strip_prefix("http://"))
    {
        let host = url.split(['/', '?', '#', ':']).next().unwrap_or_default();
        return if PUBLIC_HOSTS.contains(&host.to_lowercase().as_str()) {
            Class::Keep
        } else {
            Class::Replace("a URL", "<url>")
        };
    }
    if token.starts_with('/') && token.len() > 1 && token[1..].contains('/')
        || token.starts_with("~/")
        || token.starts_with("$HOME")
        || (token.len() > 3 && token.as_bytes()[1] == b':' && token.as_bytes()[2] == b'\\')
    {
        return Class::Replace("a path", "<path>");
    }
    if is_email(token) {
        return Class::Replace("an e-mail address", "<email>");
    }
    if is_ipv4(token) {
        return if token.starts_with("127.") || token == "0.0.0.0" {
            Class::Keep
        } else {
            Class::Replace("an IP address", "<ip>")
        };
    }
    if is_uuid(token) {
        return Class::Replace("an id", "<uuid>");
    }
    if token.len() >= 32 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Class::Replace("a hash or id", "<hex>");
    }
    if looks_secret(token) {
        return Class::Refuse(
            "a long random-looking string (a key or a token?): leave it out".into(),
        );
    }
    if crate::roster::leak(token, &ctx.private).is_some() {
        return Class::Replace("a user, host or account name", "<name>");
    }
    Class::Keep
}

fn is_email(t: &str) -> bool {
    let Some((user, host)) = t.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && host.contains('.')
        && !host.starts_with('.')
        && !host.ends_with('.')
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
}

fn is_ipv4(t: &str) -> bool {
    let t = t.split(':').next().unwrap_or(t);
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u8>().is_ok())
}

fn is_uuid(t: &str) -> bool {
    let groups: Vec<&str> = t.split('-').collect();
    groups.len() == 5
        && groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groups
            .iter()
            .all(|g| g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A token of [`SECRET_LEN`] or more characters, of letters and digits
/// (and `-_+/=`), mixing cases and digits, with high entropy.
fn looks_secret(t: &str) -> bool {
    if t.len() < SECRET_LEN
        || !t
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_+/=".contains(&b))
    {
        return false;
    }
    let upper = t.bytes().any(|b| b.is_ascii_uppercase());
    let lower = t.bytes().any(|b| b.is_ascii_lowercase());
    let digit = t.bytes().any(|b| b.is_ascii_digit());
    if !(digit && (upper && lower)) {
        return false;
    }
    let mut counts = [0usize; 256];
    for b in t.bytes() {
        counts[b as usize] += 1;
    }
    let n = t.len() as f64;
    let entropy: f64 = counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum();
    entropy >= 4.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context {
            private: vec![
                ("the OS user name", "vincentcarluer".into()),
                ("the host name", "vincentsmacbookpro".into()),
            ],
            project: vec!["acmebilling".into(), "acme".into()],
        }
    }

    #[test]
    fn paths_names_and_addresses_are_replaced() {
        let got = scrub(
            "cargo build fails in /Users/vincentcarluer/dev/x/src/main.rs (see ~/notes.txt): \
             ask bob@example.org, server 10.0.0.12, run 127.0.0.1, commit \
             0123456789abcdef0123456789abcdef01234567, see https://docs.rs/tokio and \
             https://intranet.corp/wiki, on vincentsmacbookpro.",
            &ctx(),
        )
        .unwrap();
        assert_eq!(
            got.text,
            "cargo build fails in <path> (see <path>): ask <email>, server <ip>, run 127.0.0.1, \
             commit <hex>, see https://docs.rs/tokio and <url>, on <name>."
        );
        for what in [
            "a path",
            "an e-mail address",
            "an IP address",
            "a hash or id",
            "a URL",
        ] {
            assert!(got.replaced.contains(&what), "{what}: {:?}", got.replaced);
        }
    }

    #[test]
    fn secrets_long_code_and_project_names_are_refused() {
        // Fake secrets are built here, so that no secret scanner reads one
        // in the source.
        let key = format!("set API_KEY={}{}", "sk-", "abcdefghijklmnop1234");
        let why = scrub(&key, &ctx()).unwrap_err();
        assert!(why[0].contains("secret"), "{why:?}");
        let random = ["aB3dE5fG7hJ9", "kL1mN3pQ5rS7", "tU9vW1xY3"].concat();
        let why = scrub(&format!("value {random}"), &ctx()).unwrap_err();
        assert!(why[0].contains("random-looking"), "{why:?}");
        let code = format!("```\n{}```\n", "let x = 1;\n".repeat(31));
        let why = scrub(&code, &ctx()).unwrap_err();
        assert!(why[0].contains("31 lines"), "{why:?}");
        let why = scrub("The Acme billing service and acmebilling crate", &ctx()).unwrap_err();
        assert!(why.iter().any(|w| w.contains("\"acme\"")), "{why:?}");
        assert!(why.iter().any(|w| w.contains("\"acmebilling\"")), "{why:?}");
    }

    #[test]
    fn a_generic_finding_goes_through_unchanged() {
        let text = "rustc 1.91: `cargo build` fails with error[E0425] cannot find value \
                    `foo` in this scope. Fix: import it with `use crate::foo;`.";
        let got = scrub(text, &ctx()).unwrap();
        assert_eq!(got.text, text);
        assert!(got.replaced.is_empty());
        // Short code is fine.
        let code = "```\nuse crate::foo;\n```";
        assert_eq!(scrub(code, &ctx()).unwrap().text, code);
        // Ordinary long words are not secrets.
        assert!(scrub("internationalization_configuration_settings", &ctx()).is_ok());
    }

    #[test]
    fn project_names_come_from_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("acme-shop");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"shopfront\"\n[workspace]\nmembers = [\"crates/api\", \"crates/ledger\"]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name": "@acmecorp/widgets"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@github.com:acmecorp/acme-shop.git\n",
        )
        .unwrap();
        let names = project_names(&root);
        for want in [
            "acme-shop",
            "shopfront",
            "ledger",
            "acmecorp-widgets",
            "acmecorp",
        ] {
            assert!(names.contains(&want.to_string()), "{want}: {names:?}");
        }
        assert!(!names.contains(&"api".to_string()), "too common: {names:?}");
    }
}
