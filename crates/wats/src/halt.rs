//! The network halt (ADR 0016): m of n guardian keys, compiled into the
//! binary, can stop the public network on every node that accepts it.
//!
//! The power is minimal: stop only, with no code, no deletion and no
//! upgrade. Three v1 extension kinds on `public` carry it:
//!
//! - [`KIND_HALT`]: `data = {net, reason, issued_at, nonce}`, `refs = []`;
//! - [`KIND_APPROVE`]: `data = {halt}`, `refs = [halt]`, approving a halt or
//!   an unhalt;
//! - [`KIND_UNHALT`]: `data = {halt, reason, issued_at, nonce}`,
//!   `refs = [halt]`.
//!
//! A halt (or an unhalt) is complete when its author and the authors of
//! its approvals signed within [`HALT_APPROVAL_WINDOW`] of it are at least
//! [`HALT_THRESHOLD`] distinct guardians. Of the complete ones, the one with
//! the greatest `ts` decides; a halt wins a tie. Everything here is a pure
//! function of stored events, so that every node reaches the same state.

// Halt events come from the network: never panic on them.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::event::{Draft, Event, Kind};
use crate::node::PUBLIC_NET;

/// The guardians' public keys (64 hex), compiled in and changed only by a
/// release (ADR 0016 §1). Empty until the founder's key is sent: an empty
/// set disables the halt entirely.
pub const GUARDIANS: &[&str] = &[];
/// How many distinct guardians a halt or an unhalt needs.
pub const HALT_THRESHOLD: usize = 1;
/// How long after a halt (or unhalt) its approvals count. It changes with
/// the guardian set, by release.
pub const HALT_APPROVAL_WINDOW: Duration = Duration::from_secs(24 * 3600);

pub const KIND_HALT: &str = "x.wearetheswarm.halt";
pub const KIND_APPROVE: &str = "x.wearetheswarm.halt-approve";
pub const KIND_UNHALT: &str = "x.wearetheswarm.unhalt";
/// The three halt kinds.
pub const KINDS: [&str; 3] = [KIND_HALT, KIND_APPROVE, KIND_UNHALT];

/// Longest `reason`, in bytes.
pub const MAX_REASON_BYTES: usize = 512;
/// How long a halting node waits for its relay of the halt to go out.
pub const RELAY_WAIT: Duration = Duration::from_secs(30);
/// How long a node started with the marker syncs before deciding.
pub const STARTUP_CHECK: Duration = Duration::from_secs(30);

/// `<data dir>/halted.json`: this node stopped on a halt.
pub const MARKER_FILE: &str = "halted.json";
/// `<data dir>/halt-overrides.json`: halts `wats resume` overrode here.
pub const OVERRIDES_FILE: &str = "halt-overrides.json";

/// Whether `kind` is one of the three halt kinds.
pub fn is_halt_kind(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// The guardian set a node applies: the compiled one, except in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guardians {
    /// Lowercase hex public keys.
    pub keys: Vec<String>,
    pub threshold: usize,
    pub window: Duration,
}

impl Guardians {
    /// The set compiled into this binary.
    pub fn compiled() -> Self {
        Guardians {
            keys: GUARDIANS.iter().map(|k| k.to_string()).collect(),
            threshold: HALT_THRESHOLD,
            window: HALT_APPROVAL_WINDOW,
        }
    }

    /// Whether halts can happen at all: a non-empty set, and a threshold
    /// the set can reach.
    pub fn enabled(&self) -> bool {
        self.threshold > 0 && self.threshold <= self.keys.len()
    }

    pub fn contains(&self, author: &str) -> bool {
        self.keys.iter().any(|k| k == author)
    }
}

impl Default for Guardians {
    fn default() -> Self {
        Self::compiled()
    }
}

/// How a node treats halts (`halt.*` settings, and the guardian set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HaltConfig {
    /// `halt.accept` (config.toml only): false ignores guardian halts.
    pub accept: bool,
    /// `halt.serve`: on a halt, keep serving halt events instead of exiting
    /// (the router).
    pub serve: bool,
    /// The compiled set; never read from config.toml or the command line.
    pub guardians: Guardians,
}

impl Default for HaltConfig {
    fn default() -> Self {
        HaltConfig {
            accept: true,
            serve: false,
            guardians: Guardians::compiled(),
        }
    }
}

/// What a valid guardian event says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Halt { reason: String },
    Approve { target: String },
    Unhalt { target: String, reason: String },
}

fn is_hex(s: &str, chars: usize) -> bool {
    s.len() == chars
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Exactly these keys in `data`.
fn has_keys(data: &Map<String, Value>, keys: &[&str]) -> bool {
    data.len() == keys.len() && keys.iter().all(|k| data.contains_key(*k))
}

fn reason_of(data: &Map<String, Value>) -> Option<String> {
    let reason = data.get("reason")?.as_str()?;
    (!reason.is_empty() && reason.len() <= MAX_REASON_BYTES).then(|| reason.to_string())
}

/// The action of a guardian event following ADR 0016 §2, or `None`: other
/// events, including ones of these kinds from anyone else, are plain notes
/// to the halt logic.
fn action(event: &Event, guardians: &Guardians) -> Option<Action> {
    if !is_halt_kind(event.kind.as_str())
        || !guardians.contains(&event.author)
        || event.net != PUBLIC_NET
        || event.thread.is_some()
        || event.parent.is_some()
    {
        return None;
    }
    let data = &event.data;
    let issued_ok = || data.get("issued_at").and_then(Value::as_u64) == Some(event.ts);
    let nonce_ok = || {
        data.get("nonce")
            .and_then(Value::as_str)
            .is_some_and(|n| is_hex(n, 32))
    };
    let target = || {
        let target = data.get("halt")?.as_str()?;
        (is_hex(target, 64) && event.refs == [target.to_string()]).then(|| target.to_string())
    };
    match event.kind.as_str() {
        KIND_HALT => {
            let ok = has_keys(data, &["net", "reason", "issued_at", "nonce"])
                && data.get("net").and_then(Value::as_str) == Some(event.net.as_str())
                && issued_ok()
                && nonce_ok()
                && event.refs.is_empty();
            ok.then(|| reason_of(data))
                .flatten()
                .map(|reason| Action::Halt { reason })
        }
        KIND_APPROVE => has_keys(data, &["halt"])
            .then(target)
            .flatten()
            .map(|target| Action::Approve { target }),
        KIND_UNHALT => {
            let ok = has_keys(data, &["halt", "reason", "issued_at", "nonce"])
                && issued_ok()
                && nonce_ok();
            let target = ok.then(target).flatten()?;
            reason_of(data).map(|reason| Action::Unhalt { target, reason })
        }
        _ => None,
    }
}

/// Whether `event` is a valid guardian halt event of `guardians`.
pub fn is_guardian_event(event: &Event, guardians: &Guardians) -> bool {
    action(event, guardians).is_some()
}

/// A halt or an unhalt that reached the threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    /// `true` for a halt, `false` for an unhalt.
    pub halt: bool,
    pub ts: u64,
    pub reason: String,
    /// The distinct guardians who signed it or approved it in time.
    pub signed_by: Vec<String>,
    /// The halt an unhalt names.
    pub target: Option<String>,
}

impl Decision {
    /// `{id, reason, signed_by, since}`, as `status` shows a halt.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "reason": self.reason,
            "signed_by": self.signed_by,
            "since": crate::runs::format_time(self.ts),
        })
    }
}

/// The public network's halt state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HaltState {
    /// The complete halt or unhalt with the greatest `ts`.
    pub latest: Option<Decision>,
}

impl HaltState {
    /// The halt in effect, if the latest decision is a halt.
    pub fn in_effect(&self) -> Option<&Decision> {
        self.latest.as_ref().filter(|d| d.halt)
    }
}

/// The halt state given the stored events (ADR 0016 §2). Events that are
/// not valid guardian events of `guardians` are ignored; so are tombstones,
/// which never reach this function (their `data` is gone).
pub fn evaluate(events: &[Event], guardians: &Guardians) -> HaltState {
    if !guardians.enabled() {
        return HaltState::default();
    }
    let mut decisions: Vec<(&Event, bool, String, Option<String>)> = Vec::new();
    let mut approvals: HashMap<String, Vec<&Event>> = HashMap::new();
    let mut seen = BTreeSet::new();
    for event in events {
        if !seen.insert(event.id.as_str()) {
            continue;
        }
        match action(event, guardians) {
            Some(Action::Halt { reason }) => decisions.push((event, true, reason, None)),
            Some(Action::Unhalt { target, reason }) => {
                decisions.push((event, false, reason, Some(target)));
            }
            Some(Action::Approve { target }) => {
                approvals.entry(target).or_default().push(event);
            }
            None => {}
        }
    }
    let window = guardians.window.as_secs();
    let mut latest: Option<Decision> = None;
    for (event, halt, reason, target) in decisions {
        let end = event.ts.saturating_add(window);
        let mut signers = BTreeSet::from([event.author.clone()]);
        for approval in approvals.get(&event.id).into_iter().flatten() {
            if (event.ts..=end).contains(&approval.ts) {
                signers.insert(approval.author.clone());
            }
        }
        if signers.len() < guardians.threshold {
            continue;
        }
        let decision = Decision {
            id: event.id.clone(),
            halt,
            ts: event.ts,
            reason,
            signed_by: signers.into_iter().collect(),
            target,
        };
        // Greatest ts wins; on a tie a halt wins (fail safe), then the
        // greatest id, so that every node picks the same one.
        let key = |d: &Decision| (d.ts, d.halt, d.id.clone());
        if latest.as_ref().is_none_or(|l| key(&decision) > key(l)) {
            latest = Some(decision);
        }
    }
    HaltState { latest }
}

/// The halt events a halting node relays for `decision`: the event itself
/// and its approvals.
pub fn relay_set<'a>(
    events: &'a [Event],
    decision: &Decision,
    guardians: &Guardians,
) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|e| {
            e.id == decision.id
                || matches!(action(e, guardians), Some(Action::Approve { target }) if target == decision.id)
        })
        .collect()
}

/// What this node knows of the halt, as `status` shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HaltView {
    /// The quorum halt in effect on `public`, if any.
    pub in_effect: Option<Decision>,
    /// `wats resume` overrode it on this node.
    pub overridden: bool,
}

impl HaltView {
    /// The halt this node must act on (when it accepts halts).
    pub fn enforced(&self) -> Option<&Decision> {
        self.in_effect.as_ref().filter(|_| !self.overridden)
    }
}

/// `halted.json`: written when the node stops on a halt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub halt: String,
    pub reason: String,
    pub signed_by: Vec<String>,
    /// The halt's own time (unix seconds).
    pub since: u64,
    /// When this node stopped (local time, unix seconds).
    pub at: u64,
}

impl Marker {
    pub fn new(decision: &Decision, at: u64) -> Self {
        Marker {
            halt: decision.id.clone(),
            reason: decision.reason.clone(),
            signed_by: decision.signed_by.clone(),
            since: decision.ts,
            at,
        }
    }

    /// The log line of a node that stays halted.
    pub fn message(&self) -> String {
        format!(
            "halted by guardians: {} (halt {}); `wats resume` overrides it locally",
            self.reason, self.halt
        )
    }

    pub fn to_json(&self) -> Value {
        json!({
            "halt": self.halt,
            "reason": self.reason,
            "signed_by": self.signed_by,
            "since": crate::runs::format_time(self.since),
            "at": crate::runs::format_time(self.at),
        })
    }
}

pub fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join(MARKER_FILE)
}

/// The marker, if present and readable (an unreadable one counts as
/// present: the node stays halted until `wats resume`).
pub fn read_marker(data_dir: &Path) -> Option<Marker> {
    let text = match std::fs::read_to_string(marker_path(data_dir)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("{}: {e}", marker_path(data_dir).display());
            String::new()
        }
    };
    Some(serde_json::from_str(&text).unwrap_or_else(|_| Marker {
        halt: "unknown".into(),
        reason: format!("unreadable {MARKER_FILE}"),
        signed_by: Vec::new(),
        since: 0,
        at: 0,
    }))
}

/// Write a file through a temporary file and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn write_marker(data_dir: &Path, marker: &Marker) -> Result<()> {
    write_atomic(&marker_path(data_dir), &serde_json::to_vec_pretty(marker)?)
}

pub fn remove_marker(data_dir: &Path) -> Result<()> {
    match std::fs::remove_file(marker_path(data_dir)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// One halt overridden by `wats resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Override {
    pub halt: String,
    pub reason: String,
    /// When (unix seconds).
    pub at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Overrides {
    overrides: Vec<Override>,
}

fn read_overrides(data_dir: &Path) -> Overrides {
    std::fs::read_to_string(data_dir.join(OVERRIDES_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Whether `wats resume` overrode this halt on this node.
pub fn is_overridden(data_dir: &Path, halt: &str) -> bool {
    read_overrides(data_dir)
        .overrides
        .iter()
        .any(|o| o.halt == halt)
}

/// `wats resume`: clear the marker and record its halt in the overrides,
/// so that this node does not halt again on it. Returns the marker it
/// cleared, if any.
pub fn resume(data_dir: &Path, now: u64) -> Result<Option<Marker>> {
    let Some(marker) = read_marker(data_dir) else {
        return Ok(None);
    };
    let mut overrides = read_overrides(data_dir);
    if !overrides.overrides.iter().any(|o| o.halt == marker.halt) {
        overrides.overrides.push(Override {
            halt: marker.halt.clone(),
            reason: marker.reason.clone(),
            at: now,
        });
        write_atomic(
            &data_dir.join(OVERRIDES_FILE),
            &serde_json::to_vec_pretty(&overrides)?,
        )?;
    }
    remove_marker(data_dir)?;
    Ok(Some(marker))
}

/// What `status` shows of the halt on a node that is not running.
pub fn offline_status(data_dir: &Path, accept: bool) -> Value {
    let mut status = Map::new();
    status.insert("accept".into(), accept.into());
    if let Some(marker) = read_marker(data_dir) {
        status.insert("marker".into(), marker.to_json());
    }
    Value::Object(status)
}

/// Draw a guardian key: its 32-byte seed in hex goes to a new file (mode
/// 0600, never overwriting one). Returns the key.
pub fn keygen(path: &Path) -> Result<SigningKey> {
    use std::io::Write;
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    let key = SigningKey::from_bytes(&seed);
    seed = [0; 32];
    std::hint::black_box(&seed);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).with_context(|| {
        format!(
            "create {} (an existing file is never replaced)",
            path.display()
        )
    })?;
    let mut line = hex::encode(key.to_bytes()).into_bytes();
    line.push(b'\n');
    let written = file.write_all(&line);
    line.iter_mut().for_each(|b| *b = 0);
    std::hint::black_box(&line);
    written?;
    Ok(key)
}

/// Load a guardian key from a file, or from standard input with `-`.
pub fn load_key(path: &Path) -> Result<SigningKey> {
    let secret = crate::identity::read_secret(path, "guardian key")?;
    let mut seed = [0u8; 32];
    let parsed = hex::decode_to_slice(secret.as_str()?, &mut seed);
    let key = SigningKey::from_bytes(&seed);
    seed = [0; 32];
    std::hint::black_box(&seed);
    parsed.map_err(|_| anyhow::anyhow!("a guardian key is 64 hex chars (a 32-byte seed)"))?;
    Ok(key)
}

fn nonce() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(hex::encode(bytes))
}

fn check_reason(reason: &str) -> Result<()> {
    if reason.is_empty() || reason.len() > MAX_REASON_BYTES {
        bail!("a reason is 1-{MAX_REASON_BYTES} bytes");
    }
    Ok(())
}

fn check_id(id: &str) -> Result<()> {
    if !is_hex(id, 64) {
        bail!("{id:?} is not an event id (64 lowercase hex)");
    }
    Ok(())
}

/// The draft of a halt at `ts`.
pub fn halt_draft(reason: &str, ts: u64) -> Result<Draft> {
    check_reason(reason)?;
    let mut data = Map::new();
    data.insert("net".into(), PUBLIC_NET.into());
    data.insert("reason".into(), reason.into());
    data.insert("issued_at".into(), ts.into());
    data.insert("nonce".into(), nonce()?.into());
    Ok(Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Ext(KIND_HALT.into())),
        title: Some("Guardian halt of the public network".into()),
        text: format!(
            "WeAreTheSwarm guardians halt the public network: {reason}\n\nNodes that accept \
             guardian halts stop until an unhalt, or a local `wats resume` (ADR 0016)."
        ),
        data,
        ..Draft::default()
    })
}

/// The draft of an approval of `target` (a halt or an unhalt).
pub fn approve_draft(target: &str) -> Result<Draft> {
    check_id(target)?;
    let mut data = Map::new();
    data.insert("halt".into(), target.into());
    Ok(Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Ext(KIND_APPROVE.into())),
        title: Some("Guardian approval".into()),
        text: format!("A WeAreTheSwarm guardian approves {target} (ADR 0016)."),
        data,
        refs: vec![target.to_string()],
        ..Draft::default()
    })
}

/// The draft of an unhalt of `target` at `ts`.
pub fn unhalt_draft(target: &str, reason: &str, ts: u64) -> Result<Draft> {
    check_id(target)?;
    check_reason(reason)?;
    let mut data = Map::new();
    data.insert("halt".into(), target.into());
    data.insert("reason".into(), reason.into());
    data.insert("issued_at".into(), ts.into());
    data.insert("nonce".into(), nonce()?.into());
    Ok(Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Ext(KIND_UNHALT.into())),
        title: Some("Guardian unhalt of the public network".into()),
        text: format!(
            "WeAreTheSwarm guardians lift halt {target} of the public network: {reason} (ADR 0016)."
        ),
        data,
        refs: vec![target.to_string()],
        ..Draft::default()
    })
}

/// Sign and mine a guardian draft at `ts` to what receivers require of
/// it: `base_bits` (the schedule, or `--pow-bits`), the new-author bits a
/// guardian key pays (it rarely posts), and the size bits.
pub fn sign(draft: Draft, key: &SigningKey, ts: u64, pow_bits: Option<u32>) -> Result<Event> {
    let base = pow_bits.unwrap_or_else(|| {
        crate::event::scheduled_pow_bits(crate::event::POW_SCHEDULE, ts)
            .saturating_add(crate::event::new_author_extra_bits(None))
    });
    let bits = crate::event::required_pow_bits(base, draft.estimated_size(key)?);
    Ok(draft.sign(key, ts, bits)?)
}

/// Check an event a guardian command is asked to publish: a valid v1
/// guardian event of `guardians`.
pub fn check_publishable(value: &Value, guardians: &Guardians) -> Result<Event> {
    let event = match crate::wire::AnyEvent::from_value(value.clone())? {
        crate::wire::AnyEvent::Known(event) => event,
        crate::wire::AnyEvent::Opaque(_) => bail!("not a v1 event"),
    };
    if !is_halt_kind(event.kind.as_str()) {
        bail!("not a halt event (kind {})", event.kind);
    }
    if !guardians.contains(&event.author) {
        bail!(
            "{} is not a guardian of this release: nodes would treat the event as a plain note",
            event.author
        );
    }
    if action(&event, guardians).is_none() {
        bail!("the event does not follow ADR 0016 §2");
    }
    Ok(event)
}

/// Serve an MCP client while the node is halted, without a node: every
/// request gets an answer that says so (`initialize` carries it in its
/// instructions, every tool call fails with it), until the client leaves.
pub async fn serve_halted_mcp<R, W>(
    mut lines: tokio::io::Lines<R>,
    mut out: W,
    message: &str,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    while let Some(line) = lines.next_line().await? {
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = request.get("id").cloned() else {
            continue; // a notification
        };
        let result = match request.get("method").and_then(Value::as_str) {
            Some("initialize") => json!({
                "protocolVersion": request
                    .pointer("/params/protocolVersion")
                    .cloned()
                    .unwrap_or_else(|| "2025-06-18".into()),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "wats", "version": env!("CARGO_PKG_VERSION")},
                "instructions": message,
            }),
            Some("ping") => json!({}),
            Some("tools/list") => json!({"tools": [{
                "name": "status",
                "description": "WeAreTheSwarm (wats): why this node is stopped.",
                "inputSchema": {"type": "object", "properties": {}},
            }]}),
            Some("tools/call") => json!({
                "content": [{"type": "text", "text": message}],
                "isError": true,
            }),
            _ => {
                let reply = json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": message}});
                out.write_all(format!("{reply}\n").as_bytes()).await?;
                out.flush().await?;
                continue;
            }
        };
        let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
        out.write_all(format!("{reply}\n").as_bytes()).await?;
        out.flush().await?;
    }
    Ok(())
}

/// What an MCP client is told about a halted node.
pub fn mcp_message(marker: &Marker) -> String {
    format!(
        "The WeAreTheSwarm public network is halted by its guardians: {} (halt {}). \
         This node stopped; tell the user. `wats resume` overrides the halt on this \
         machine (ADR 0016).",
        marker.reason, marker.halt
    )
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3600;

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    fn set(keys: &[u8], threshold: usize) -> Guardians {
        Guardians {
            keys: keys
                .iter()
                .map(|n| crate::identity::author_id(&key(*n)))
                .collect(),
            threshold,
            window: HALT_APPROVAL_WINDOW,
        }
    }

    fn halt(n: u8, ts: u64) -> Event {
        sign(halt_draft("runaway", ts).unwrap(), &key(n), ts, Some(0)).unwrap()
    }

    fn approve(n: u8, target: &Event, ts: u64) -> Event {
        sign(approve_draft(&target.id).unwrap(), &key(n), ts, Some(0)).unwrap()
    }

    fn unhalt(n: u8, target: &Event, ts: u64) -> Event {
        sign(
            unhalt_draft(&target.id, "fixed", ts).unwrap(),
            &key(n),
            ts,
            Some(0),
        )
        .unwrap()
    }

    fn in_effect(events: &[Event], guardians: &Guardians) -> Option<String> {
        evaluate(events, guardians)
            .in_effect()
            .map(|d| d.id.clone())
    }

    #[test]
    fn one_of_one_needs_no_approval() {
        let g = set(&[1], 1);
        let h = halt(1, 1000);
        let state = evaluate(std::slice::from_ref(&h), &g);
        let d = state.in_effect().unwrap();
        assert_eq!(d.id, h.id);
        assert_eq!(d.reason, "runaway");
        assert_eq!(d.signed_by, g.keys);
        assert!(is_guardian_event(&h, &g));
    }

    #[test]
    fn an_empty_set_disables_halts() {
        let h = halt(1, 1000);
        let mut g = set(&[], 1);
        assert!(!g.enabled());
        assert_eq!(in_effect(std::slice::from_ref(&h), &g), None);
        g = set(&[1], 2);
        assert!(!g.enabled(), "a threshold above the set");
        assert!(!Guardians::compiled().enabled() || !GUARDIANS.is_empty());
    }

    #[test]
    fn two_of_three_within_the_window() {
        let g = set(&[1, 2, 3], 2);
        let h = halt(1, 1000);
        assert_eq!(in_effect(std::slice::from_ref(&h), &g), None, "alone");
        // Approvals from the same guardian count once.
        let own = approve(1, &h, 1001);
        assert_eq!(in_effect(&[h.clone(), own.clone()], &g), None);
        let again = approve(1, &h, 1002);
        assert_eq!(in_effect(&[h.clone(), own, again], &g), None);
        // In the window, at its end, too late, too early.
        let ok = approve(2, &h, 1000 + DAY);
        assert_eq!(in_effect(&[h.clone(), ok], &g), Some(h.id.clone()));
        let late = approve(2, &h, 1001 + DAY);
        assert_eq!(in_effect(&[h.clone(), late], &g), None);
        let early = approve(2, &h, 999);
        assert_eq!(in_effect(&[h.clone(), early], &g), None);
        let state = evaluate(&[h.clone(), approve(3, &h, 2000)], &g);
        assert_eq!(state.in_effect().unwrap().signed_by.len(), 2);
    }

    #[test]
    fn non_guardians_and_other_nets_are_ignored() {
        let g = set(&[1], 1);
        let stranger = halt(9, 1000);
        assert_eq!(in_effect(std::slice::from_ref(&stranger), &g), None);
        assert!(!is_guardian_event(&stranger, &g));
        // A stranger's approval does not count either.
        let g2 = set(&[1, 2], 2);
        let h = halt(1, 1000);
        assert_eq!(in_effect(&[h.clone(), approve(9, &h, 1001)], &g2), None);
        // A halt on another network.
        let mut draft = halt_draft("elsewhere", 1000).unwrap();
        draft.net = "lab".into();
        draft.data.insert("net".into(), "lab".into());
        let other = sign(draft, &key(1), 1000, Some(0)).unwrap();
        assert_eq!(in_effect(std::slice::from_ref(&other), &g), None);
        // data.net differs from the header.
        let mut draft = halt_draft("liar", 1000).unwrap();
        draft.data.insert("net".into(), "lab".into());
        let liar = sign(draft, &key(1), 1000, Some(0)).unwrap();
        assert_eq!(in_effect(std::slice::from_ref(&liar), &g), None);
        // issued_at differs from ts.
        let skewed = sign(halt_draft("x", 999).unwrap(), &key(1), 1000, Some(0)).unwrap();
        assert_eq!(in_effect(std::slice::from_ref(&skewed), &g), None);
        // Extra data keys.
        let mut draft = halt_draft("x", 1000).unwrap();
        draft.data.insert("code".into(), "rm -rf".into());
        let extra = sign(draft, &key(1), 1000, Some(0)).unwrap();
        assert_eq!(in_effect(std::slice::from_ref(&extra), &g), None);
    }

    #[test]
    fn the_latest_decision_wins_and_a_tie_halts() {
        let g = set(&[1], 1);
        let h1 = halt(1, 1000);
        let u = unhalt(1, &h1, 2000);
        assert_eq!(in_effect(&[h1.clone(), u.clone()], &g), None);
        let state = evaluate(&[h1.clone(), u.clone()], &g);
        assert_eq!(state.latest.as_ref().unwrap().id, u.id);
        assert_eq!(state.latest.unwrap().target, Some(h1.id.clone()));
        // A later halt applies again; the unhalt lifted older halts only.
        let h2 = halt(1, 3000);
        assert_eq!(
            in_effect(&[h1.clone(), u.clone(), h2.clone()], &g),
            Some(h2.id.clone())
        );
        // An unhalt lifts every older halt, not only the one it names.
        let h0 = halt(1, 500);
        assert_eq!(in_effect(&[h0, h1.clone(), u.clone()], &g), None);
        // Tie: the halt wins.
        let tie = halt(1, 2000);
        assert_eq!(in_effect(&[h1, u, tie.clone()], &g), Some(tie.id));
    }

    #[test]
    fn an_unhalt_needs_its_own_quorum() {
        let g = set(&[1, 2, 3], 2);
        let h = halt(1, 1000);
        let a = approve(2, &h, 1100);
        let u = unhalt(3, &h, 2000);
        assert_eq!(
            in_effect(&[h.clone(), a.clone(), u.clone()], &g),
            Some(h.id.clone())
        );
        // Approving the halt does not approve the unhalt.
        let ua = approve(1, &u, 2100);
        assert_eq!(in_effect(&[h, a, u, ua], &g), None);
    }

    #[test]
    fn tombstoned_halts_do_not_count() {
        use crate::wire::{BODY_HASH_KEY, HEADER_KEYS, REFS_KEY, UNHASHED_KEYS, digest};
        let g = set(&[1], 1);
        let h = halt(1, 1000);
        let full = match serde_json::to_value(&h).unwrap() {
            Value::Object(map) => map,
            _ => unreachable!(),
        };
        let mut tomb: Map<String, Value> = full
            .iter()
            .filter(|(k, _)| {
                HEADER_KEYS.contains(&k.as_str())
                    || k.as_str() == REFS_KEY
                    || UNHASHED_KEYS.contains(&k.as_str())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        tomb.insert(
            BODY_HASH_KEY.into(),
            digest(&full).unwrap().body_hash.into(),
        );
        let raw = crate::wire::RawEvent::from_value(Value::Object(tomb)).unwrap();
        assert!(raw.tombstoned);
        let store = crate::store::Store::open_in_memory().unwrap();
        store.insert_raw(&raw, 1).unwrap();
        let events = store.halt_events(PUBLIC_NET, &g.keys).unwrap();
        assert!(events.is_empty());
        assert_eq!(in_effect(&events, &g), None);
        // The full event, once known, counts (a tombstone never downgrades
        // it: a forged tombstone cannot lift a halt).
        store.insert(&h, 2).unwrap();
        let events = store.halt_events(PUBLIC_NET, &g.keys).unwrap();
        assert_eq!(in_effect(&events, &g), Some(h.id));
    }

    #[test]
    fn relay_set_is_the_halt_and_its_approvals() {
        let g = set(&[1, 2], 2);
        let h = halt(1, 1000);
        let a = approve(2, &h, 1001);
        let other = halt(2, 1500);
        let events = [h.clone(), a.clone(), other];
        let d = evaluate(&events, &g).in_effect().cloned().unwrap();
        let ids: Vec<&str> = relay_set(&events, &d, &g)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(ids, [h.id.as_str(), a.id.as_str()]);
    }

    #[test]
    fn marker_resume_and_overrides() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resume(dir.path(), 5).unwrap().is_none());
        let d = Decision {
            id: "ab".repeat(32),
            halt: true,
            ts: 1000,
            reason: "runaway".into(),
            signed_by: vec!["cd".repeat(32)],
            target: None,
        };
        write_marker(dir.path(), &Marker::new(&d, 2000)).unwrap();
        assert_eq!(read_marker(dir.path()).unwrap().halt, d.id);
        assert!(!is_overridden(dir.path(), &d.id));
        let cleared = resume(dir.path(), 3000).unwrap().unwrap();
        assert_eq!(cleared.reason, "runaway");
        assert!(read_marker(dir.path()).is_none());
        assert!(is_overridden(dir.path(), &d.id));
        assert!(!is_overridden(dir.path(), &"ef".repeat(32)));
    }

    #[test]
    fn keys_from_files_and_drafts_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.key");
        let k = keygen(&path).unwrap();
        assert!(keygen(&path).is_err(), "never overwritten");
        assert_eq!(load_key(&path).unwrap().to_bytes(), k.to_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(halt_draft("", 1).is_err());
        assert!(halt_draft(&"x".repeat(513), 1).is_err());
        assert!(approve_draft("not-an-id").is_err());
        let h = halt(1, 1000);
        let g = set(&[1], 1);
        let value = serde_json::to_value(&h).unwrap();
        assert_eq!(check_publishable(&value, &g).unwrap().id, h.id);
        assert!(check_publishable(&value, &set(&[2], 1)).is_err());
    }

    #[tokio::test]
    async fn a_halted_mcp_server_tells_the_client() {
        use tokio::io::AsyncBufReadExt;
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search"}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
            "\n",
        );
        let lines = tokio::io::BufReader::new(input.as_bytes()).lines();
        let mut out = Vec::new();
        serve_halted_mcp(lines, &mut out, "halted: test")
            .await
            .unwrap();
        let replies: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(replies.len(), 3, "no answer to a notification");
        assert_eq!(replies[0]["result"]["instructions"], "halted: test");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(replies[1]["id"], 2);
        assert_eq!(replies[1]["result"]["isError"], true);
        assert_eq!(replies[1]["result"]["content"][0]["text"], "halted: test");
        assert_eq!(replies[2]["error"]["code"], -32601);
    }
}
