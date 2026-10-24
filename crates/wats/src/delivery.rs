//! Routing and delivery modes (ADR 0022): how an ask wants to be served,
//! and what its thread says about it now.
//!
//! An ask names who should wake (`to:<author id>` and `cap:<x>` tags) and
//! how many should work on it (`data.mode`: `any`, `one`, `n` with
//! `data.want`, `all`; `data.max` caps the responders). A `claim` holds a
//! task for `data.lease` seconds, renewed by the claimer's `progress`, and
//! ends with its `answer` or `release`. Everything is node policy, read
//! leniently: a value this node does not understand falls back to its
//! default, and validity never depends on it.
//!
//! [`State::of`] reads an ask's thread into its state (accepted, live
//! claims, responders against the cap) and [`State::verdict`] says what an
//! agent should do about it: the runner decides from it whether to wake
//! the model, and the MCP tools show it to sessions. Pure functions of the
//! events, the key grouping (ADR 0019 §2.4) and the time.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::{Value, json};

use crate::event::{Event, Kind};
use crate::identity::{author_id_base32, author_id_from_base32};
use crate::signals::Grouping;

/// `claim.data.lease` when absent or malformed: 15 minutes, as claims
/// written before 0.4.13 meant.
pub const DEFAULT_LEASE_SECS: u64 = 900;
/// The lease a claim gets through the MCP `reply` tool when it names none:
/// 5 minutes, written out in `data.lease` (ADR 0044). `progress` renews it,
/// and heavy work sets a longer one.
pub const CLAIM_LEASE_SECS: u64 = 300;
/// The mode a new ask gets through the MCP `post` tool when it names none
/// (ADR 0044): one agent claims and answers, the others stay out. Written
/// out in `data.mode`: an ask without it still reads as `any`.
pub const DEFAULT_ASK_MODE: Mode = Mode::One;
/// The longest lease this node honours: a claim cannot hold a task for
/// days without news.
pub const MAX_LEASE_SECS: u64 = 86_400;
/// Default of `ask.max_responders`.
pub const DEFAULT_MAX_RESPONDERS: usize = 3;
/// Default of `ask.cap_grace`.
pub const DEFAULT_CAP_GRACE: Duration = Duration::from_secs(120);
/// The longest random delay before a runner wakes for an ask, so that
/// runners that saw it at the same moment count each other's claims.
pub const CLAIM_JITTER: Duration = Duration::from_secs(5);
/// At most this many `to:` tags count on an ask.
pub const MAX_TO: usize = 4;
/// The largest `want` or `max` read; larger values are read as this.
const MAX_COUNT: u64 = 1000;

/// `ask.data.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Several may answer, until the asker accepts one: how an ask without
    /// a mode reads (a new one through the MCP tools is `one`).
    Any,
    /// An exclusive claim: one worker.
    One,
    /// `want` independent answers or claims, from distinct nodes.
    N,
    /// Broadcast: every capable agent answers.
    All,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Any => "any",
            Mode::One => "one",
            Mode::N => "n",
            Mode::All => "all",
        }
    }
}

/// A positive count read leniently: an integer (event data carries no
/// other numbers) or a string of decimal figures, at least 1 (larger than
/// [`MAX_COUNT`] is read as it). Anything else is `None`.
fn count(value: Option<&Value>) -> Option<u64> {
    let n = match value? {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (n >= 1).then_some(n.min(MAX_COUNT))
}

/// A claim's lease in seconds: `data.lease` read leniently, at most
/// [`MAX_LEASE_SECS`], else [`DEFAULT_LEASE_SECS`].
pub fn lease(claim: &Event) -> u64 {
    count(claim.data.get("lease")).map_or(DEFAULT_LEASE_SECS, |n| n.min(MAX_LEASE_SECS))
}

/// Whether `tag` is `prefix:<something>` (ASCII case aside), and the rest.
fn tag_value<'t>(tag: &'t str, prefix: &str) -> Option<&'t str> {
    let head = tag.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &tag[prefix.len()..])
}

/// The tag namespaces that name a field: an agent that announces caps in
/// one of them has a field there (ADR 0044 Q2).
pub const FIELD_NAMESPACES: [&str; 2] = ["lang:", "topic:"];

/// The ask's field tags outside the field of an agent with `caps`: for
/// each [`FIELD_NAMESPACES`] where the agent announces caps and the ask
/// has tags, the ask's tags when none of them is one of the agent's caps.
/// Empty when the ask is in its field, or when the agent announces no
/// field (a generalist). ASCII case aside.
pub fn outside_field(caps: &[String], tags: &[String]) -> Vec<String> {
    let mut outside = Vec::new();
    for ns in FIELD_NAMESPACES {
        let mine: Vec<String> = caps
            .iter()
            .filter(|c| tag_value(c, ns).is_some())
            .map(|c| c.trim().to_ascii_lowercase())
            .collect();
        let asked: Vec<String> = tags
            .iter()
            .filter(|t| tag_value(t, ns).is_some_and(|v| !v.is_empty()))
            .map(|t| t.trim().to_ascii_lowercase())
            .collect();
        if !mine.is_empty() && !asked.is_empty() && !asked.iter().any(|t| mine.contains(t)) {
            outside.extend(asked);
        }
    }
    outside
}

/// How an ask wants to be served: its mode, routing tags and cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub mode: Mode,
    /// The N of mode `n`; 1 otherwise.
    pub want: usize,
    /// `data.max`, when set and valid.
    pub max: Option<usize>,
    /// The author ids its `to:` tags name (lowercase hex, at most
    /// [`MAX_TO`]).
    pub to: Vec<String>,
    /// Its `cap:` tags, lowercase.
    pub caps: Vec<String>,
}

impl Request {
    /// Read from an ask, leniently: an unknown or malformed `mode` is
    /// `any`, and so is `n` without a valid `want`; a malformed `max` is
    /// unset; a `to:` tag that does not carry a full author id in base32
    /// is ignored.
    pub fn of(ask: &Event) -> Self {
        let mut mode = match ask.data.get("mode").and_then(Value::as_str) {
            Some(m) => match m.trim().to_ascii_lowercase().as_str() {
                "one" => Mode::One,
                "n" => Mode::N,
                "all" => Mode::All,
                _ => Mode::Any,
            },
            None => Mode::Any,
        };
        let want = match (mode, count(ask.data.get("want"))) {
            (Mode::N, Some(n)) => n as usize,
            (Mode::N, None) => {
                mode = Mode::Any;
                1
            }
            _ => 1,
        };
        let mut to: Vec<String> = Vec::new();
        let mut caps: Vec<String> = Vec::new();
        for tag in &ask.tags {
            if let Some(value) = tag_value(tag, "to:") {
                if let Some(id) = author_id_from_base32(value)
                    && to.len() < MAX_TO
                    && !to.contains(&id)
                {
                    to.push(id);
                }
            } else if let Some(cap) = tag_value(tag, "cap:") {
                let cap = format!("cap:{}", cap.to_ascii_lowercase());
                if cap.len() > 4 && !caps.contains(&cap) {
                    caps.push(cap);
                }
            }
        }
        Self {
            mode,
            want,
            max: count(ask.data.get("max")).map(|n| n as usize),
            to,
            caps,
        }
    }

    /// The cap on responders with the node default `default`
    /// (`ask.max_responders`; `None`: no default cap): 1 for `one`; for
    /// `n`, `data.max` or the default, never below `want`; for `any`,
    /// `data.max` or the default; for `all`, `data.max` only.
    pub fn cap(&self, default: Option<usize>) -> Option<usize> {
        match self.mode {
            Mode::One => Some(1),
            Mode::N => Some(self.max.or(default).map_or(self.want, |m| m.max(self.want))),
            Mode::Any => self.max.or(default),
            Mode::All => self.max,
        }
    }

    /// The exclusive slots: 1 for `one`, `want` for `n`; claims beyond
    /// them lose the tie-break. `None` for `any` and `all`.
    pub fn slots(&self) -> Option<usize> {
        match self.mode {
            Mode::One => Some(1),
            Mode::N => Some(self.want),
            Mode::Any | Mode::All => None,
        }
    }

    /// Whether a `to:` tag names the author `me` (an author id).
    pub fn addresses(&self, me: &str) -> bool {
        self.to.iter().any(|id| id.eq_ignore_ascii_case(me))
    }

    /// Whether an agent with these capability tags (`agent.caps` as
    /// announced, e.g. `cap:pdf`) has every `cap:` the ask names.
    pub fn capable(&self, caps: &[String]) -> bool {
        self.caps
            .iter()
            .all(|c| caps.iter().any(|mine| mine.eq_ignore_ascii_case(c)))
    }
}

/// A live claim on an ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub id: String,
    pub author: String,
    pub ts: u64,
    /// When its lease runs out (unix seconds): its time or its claimer's
    /// latest `progress` in the thread, plus its lease.
    pub expires: u64,
}

/// What an agent is expected to do about an ask it wakes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Its own claim holds: go on, `progress` renews the lease.
    Continue,
    /// `one` or `n`: claim before any work.
    ClaimFirst,
    /// Open: light answers directly, a claim before heavy work.
    Answer,
    /// Others hold live claims (`any`): leave it unless bringing something
    /// clearly different.
    AbstainUnlessDifferent,
}

impl Expect {
    pub fn label(self) -> &'static str {
        match self {
            Expect::Continue => "continue_your_claim",
            Expect::ClaimFirst => "claim_first",
            Expect::Answer => "answer",
            Expect::AbstainUnlessDifferent => "abstain_unless_different",
        }
    }

    /// The instruction in a runner prompt.
    pub fn instruction(self) -> &'static str {
        match self {
            Expect::Continue => {
                "your claim holds: finish the work, post `progress` on long work (it renews \
                 the lease), then `answer`; `release` it if you give up"
            }
            Expect::ClaimFirst => {
                "reply with kind `claim` (its lease is 300 s unless you set `data.lease`) before \
                 any work, then `read` the thread again: if an \
                 older claim fills the slots, stop (the node releases yours); otherwise do the \
                 work and `answer`"
            }
            Expect::Answer => {
                "a light answer (knowledge, an explanation, a pointer to a digest or a \
                 confirmed answer) goes out directly; before heavy work (compiling, \
                 generating, several turns), reply with kind `claim` first"
            }
            Expect::AbstainUnlessDifferent => {
                "others are on it: leave it, unless you bring something clearly different"
            }
        }
    }
}

/// Whether an agent should wake for an ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Wake(Expect),
    /// No wake, and why. `until`: when the state may change on its own (a
    /// lease runs out); `None`: only a new post in the thread changes it.
    Skip {
        why: String,
        until: Option<u64>,
    },
}

/// An asker's withdrawal of its own ask (0.4.12, ADR 0022 amended): a
/// `release` replying to the ask, by the asker's key group, with
/// `data.withdraw` true. It closes the ask for everyone, like an accept,
/// and deletes nothing. Said "withdrawn" to people: "release" stays the
/// wire kind, with no format change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    /// The withdrawal's id.
    pub id: String,
    /// `data.reason` (e.g. "duplicate", "no longer needed").
    pub reason: Option<String>,
    /// Its `refs`: where the ask is answered, for readers.
    pub refs: Vec<String>,
}

impl Withdrawal {
    /// `withdrawn (duplicate; see 3f16ab12…)`.
    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        if let Some(reason) = &self.reason {
            parts.push(reason.clone());
        }
        if let Some(first) = self.refs.first() {
            parts.push(format!("see {}…", short(first)));
        }
        match parts.is_empty() {
            true => "withdrawn".to_string(),
            false => format!("withdrawn ({})", parts.join("; ")),
        }
    }
}

/// Whether `e` withdraws the post it replies to: a `release` with
/// `data.withdraw` true.
pub fn is_withdrawal(e: &Event) -> bool {
    e.kind == Kind::Release && e.data.get("withdraw").and_then(Value::as_bool) == Some(true)
}

/// The state of an ask, from its thread.
#[derive(Debug, Clone)]
pub struct State {
    pub ask: String,
    pub request: Request,
    /// The cap on responders (see [`Request::cap`]).
    pub cap: Option<usize>,
    /// Its asker accepted an answer.
    pub accepted: bool,
    /// Its asker withdrew it ([`Withdrawal`]).
    pub withdrawn: Option<Withdrawal>,
    /// Live claims, oldest first (`ts`, then id).
    pub claims: Vec<Claim>,
    /// The claims that hold a slot of `one` or `n`, by the tie-break:
    /// earliest `ts`, then lowest id, one per group of keys.
    pub winners: Vec<String>,
    /// Groups of keys (ADR 0019 §2.4: distinct nodes, as far as this node
    /// can tell) that answered or hold a live claim, the asker left out.
    pub responders: usize,
    /// Authors of answers, the asker left out.
    pub answered: Vec<String>,
    /// Groups that answered: they take slots of `one` and `n` first.
    answered_groups: usize,
    /// The time the state was read at.
    now: u64,
}

impl State {
    /// Read the state of `ask` from its thread (`thread`: its events, the
    /// ask included or not, in any order) at `now`, with the node's key
    /// grouping and its default cap (`ask.max_responders`).
    pub fn of<'e>(
        ask: &Event,
        thread: impl IntoIterator<Item = &'e Event>,
        grouping: &Grouping,
        default_max: Option<usize>,
        now: u64,
    ) -> Self {
        let net = ask.net.as_str();
        let asker = ask.author.as_str();
        let request = Request::of(ask);
        let mut events: Vec<&Event> = replies_to(ask, thread);
        events.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
        let by_asker = |e: &&Event| grouping.same_key(net, asker, &e.author);

        // The asker's accept: the latest `data.accept` true, unless a later
        // `data.accept` false on the same answer withdrew it (as in
        // `signals::Threads`).
        let accepts: Vec<(&Event, bool)> = events
            .iter()
            .filter(|e| e.kind == Kind::React && by_asker(e))
            .filter_map(|e| Some((*e, e.data.get("accept")?.as_bool()?)))
            .collect();
        let withdrawn = events
            .iter()
            .rev()
            .find(|e| {
                by_asker(e) && is_withdrawal(e) && e.parent.as_deref() == Some(ask.id.as_str())
            })
            .map(|e| Withdrawal {
                id: e.id.clone(),
                reason: e
                    .data
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(|r| r.chars().take(120).collect()),
                refs: e.refs.clone(),
            });
        let accepted = accepts.iter().rposition(|(_, yes)| *yes).is_some_and(|i| {
            let parent = &accepts[i].0.parent;
            !accepts[i + 1..]
                .iter()
                .any(|(e, yes)| !yes && &e.parent == parent)
        });

        // Each key's latest claim, unless answered, released or expired.
        let mut claims: Vec<Claim> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for claim in events.iter().rev().filter(|e| e.kind == Kind::Claim) {
            if by_asker(claim) || !seen.insert(claim.author.as_str()) {
                continue;
            }
            let mine = |e: &&&Event| e.author == claim.author && e.id != claim.id;
            let done = events.iter().filter(mine).any(|e| match e.kind {
                Kind::Answer => e.ts >= claim.ts,
                // A release of this claim, or of everything before it.
                Kind::Release => match e.parent.as_deref() {
                    Some(p) if p == claim.id => true,
                    Some(p) if events.iter().any(|c| c.id == p && c.kind == Kind::Claim) => false,
                    _ => e.ts >= claim.ts,
                },
                _ => false,
            });
            if done {
                continue;
            }
            let renewed = events
                .iter()
                .filter(mine)
                .filter(|e| e.kind == Kind::Progress && e.ts >= claim.ts)
                .map(|e| e.ts)
                .max()
                .unwrap_or(claim.ts);
            let expires = renewed.saturating_add(lease(claim));
            if expires > now {
                claims.push(Claim {
                    id: claim.id.clone(),
                    author: claim.author.clone(),
                    ts: claim.ts,
                    expires,
                });
            }
        }
        claims.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));

        let mut answered: Vec<String> = Vec::new();
        for e in events
            .iter()
            .filter(|e| e.kind == Kind::Answer && !by_asker(e))
        {
            if !answered.contains(&e.author) {
                answered.push(e.author.clone());
            }
        }

        // Groups that answered take the slots of `one` and `n` first.
        let mut responders: Vec<&str> = Vec::new();
        for author in answered.iter().map(String::as_str) {
            if !responders.iter().any(|g| grouping.same(net, g, author)) {
                responders.push(author);
            }
        }
        let answered_groups = responders.len();
        // The tie-break of `one` and `n`: earliest claims first, one per
        // group, in the slots answers left.
        let mut winners = Vec::new();
        let free = request
            .slots()
            .map(|slots| slots.saturating_sub(answered_groups));
        for claim in &claims {
            let new_group = !responders
                .iter()
                .any(|g| grouping.same(net, g, &claim.author));
            if new_group {
                responders.push(&claim.author);
            }
            if let Some(free) = free
                && new_group
                && winners.len() < free
            {
                winners.push(claim.id.clone());
            }
        }
        let responders = responders.len();
        State {
            ask: ask.id.clone(),
            cap: request.cap(default_max),
            request,
            accepted,
            withdrawn,
            claims,
            winners,
            responders,
            answered,
            answered_groups,
            now,
        }
    }

    /// The live claim of `me`, if any.
    pub fn claim_of(&self, me: &str) -> Option<&Claim> {
        self.claims.iter().find(|c| c.author == me)
    }

    /// The live claim of `me` when it lost the tie-break of `one` or `n`:
    /// it holds no slot.
    pub fn lost(&self, me: &str) -> Option<&Claim> {
        self.request.slots()?;
        self.claim_of(me).filter(|c| !self.winners.contains(&c.id))
    }

    /// The soonest lease to run out.
    pub fn next_expiry(&self) -> Option<u64> {
        self.claims.iter().map(|c| c.expires).min()
    }

    /// Whether the slots of `one` or `n` are all taken (answers and
    /// winning claims).
    fn full(&self) -> bool {
        self.request
            .slots()
            .is_some_and(|slots| self.answered_groups + self.winners.len() >= slots)
    }

    /// What the agent `me` should do about the ask (ADR 0022 §3).
    pub fn verdict(&self, me: &str) -> Verdict {
        let skip = |why: String, until: Option<u64>| Verdict::Skip { why, until };
        if self.accepted {
            return skip("its asker accepted an answer".into(), None);
        }
        if let Some(w) = &self.withdrawn {
            return skip(format!("its asker {} it", w.label()), None);
        }
        if self.claim_of(me).is_some() {
            return match self.lost(me) {
                Some(lost) => skip(
                    format!(
                        "your claim {} lost the tie-break (an older claim holds it)",
                        short(&lost.id)
                    ),
                    None,
                ),
                None => Verdict::Wake(Expect::Continue),
            };
        }
        if self.answered.iter().any(|a| a == me) {
            return skip("you answered it already".into(), None);
        }
        if self.full() {
            let held: Vec<&Claim> = self
                .claims
                .iter()
                .filter(|c| self.winners.contains(&c.id))
                .collect();
            let why = match held.first() {
                Some(c) => format!(
                    "mode {}: claimed by {} (lease {} s left)",
                    self.request.mode.as_str(),
                    short(&c.author),
                    c.expires.saturating_sub(self.now)
                ),
                None => format!(
                    "mode {}: answered by {}",
                    self.request.mode.as_str(),
                    count_text(self.answered_groups, "responder", "responders")
                ),
            };
            return skip(why, self.next_expiry());
        }
        if let Some(cap) = self.cap
            && self.responders >= cap
        {
            return skip(
                format!(
                    "at the cap: {} of {cap}",
                    count_text(self.responders, "responder", "responders")
                ),
                self.next_expiry(),
            );
        }
        Verdict::Wake(match self.request.mode {
            Mode::One | Mode::N => Expect::ClaimFirst,
            Mode::Any if self.claims.iter().any(|c| c.author != me) => {
                Expect::AbstainUnlessDifferent
            }
            Mode::Any | Mode::All => Expect::Answer,
        })
    }

    /// One line for a runner prompt: the mode, live claims with their
    /// leases, the count against the cap, and what is expected.
    pub fn prompt_line(&self, me: &str, now: u64) -> String {
        let mode = match self.request.mode {
            Mode::Any => "any (several may answer until the asker accepts one)".to_string(),
            Mode::One => "one (a single worker, by exclusive claim)".to_string(),
            Mode::N => format!(
                "n (wants {} independent answers, from distinct nodes)",
                self.request.want
            ),
            Mode::All => "all (every capable agent answers)".to_string(),
        };
        let claims = if self.claims.is_empty() {
            "no live claim".to_string()
        } else {
            let list: Vec<String> = self
                .claims
                .iter()
                .map(|c| {
                    format!(
                        "{}{} ({} s left)",
                        short(&c.author),
                        if c.author == me { ", yours" } else { "" },
                        c.expires.saturating_sub(now)
                    )
                })
                .collect();
            format!("live claims: {}", list.join(", "))
        };
        let count = match self.cap {
            Some(cap) => format!("{} of {cap} responders", self.responders),
            None => format!("{} responders, no cap", self.responders),
        };
        let expected = match self.verdict(me) {
            Verdict::Wake(expect) => expect.instruction().to_string(),
            Verdict::Skip { why, .. } => format!("leave it ({why})"),
        };
        format!(
            "- {}: mode {mode}; {claims}; {count}. Expected: {expected}.",
            self.ask
        )
    }

    /// The state as MCP tools show it to a session of `me`: labels and
    /// counts only.
    pub fn view(&self, me: &str, now: u64) -> Value {
        let mut view = json!({
            "mode": self.request.mode.as_str(),
            "responders": self.responders,
            "claims": self.claims.iter().map(|c| json!({
                "id": c.id,
                "author": c.author,
                "lease_left_secs": c.expires.saturating_sub(now),
            })).collect::<Vec<_>>(),
        });
        if self.request.mode == Mode::N {
            view["want"] = json!(self.request.want);
        }
        if let Some(cap) = self.cap {
            view["cap"] = json!(cap);
        }
        if self.accepted {
            view["accepted"] = json!(true);
        }
        if let Some(w) = &self.withdrawn {
            view["withdrawn"] =
                json!({"id": w.id, "reason": w.reason, "refs": w.refs, "label": w.label()});
        }
        match self.verdict(me) {
            Verdict::Wake(expect) => view["expected"] = json!(expect.label()),
            Verdict::Skip { why, .. } => {
                view["expected"] = json!("abstain");
                view["why"] = json!(why);
            }
        }
        if let Some(lost) = self.lost(me) {
            view["your_claim_lost"] = json!(lost.id);
        }
        view
    }
}

/// The `to:` tag that addresses the author `id` (64 hex characters):
/// `to:` and the id in lowercase base32, 55 bytes, so that it fits in a
/// tag (ADR 0022 §1). `None` when `id` is not an author id.
pub fn to_tag(id: &str) -> Option<String> {
    author_id_base32(id).map(|b32| format!("to:{b32}"))
}

/// A `to:` tag in the form nodes read: an author id in hex (64
/// characters) or base32 (52, either case) becomes `to:<base32>`; any
/// other tag as is. With a note when a `to:` tag addresses nobody.
pub fn normalize_to_tag(tag: String) -> (String, Option<String>) {
    let Some(value) = tag_value(&tag, "to:") else {
        return (tag, None);
    };
    let id = match value.len() {
        64 => value.to_ascii_lowercase(),
        _ => author_id_from_base32(value).unwrap_or_default(),
    };
    match to_tag(&id) {
        Some(normalized) => (normalized, None),
        None => {
            let note = format!(
                "{} is not a full author id (64 hex or 52 base32 characters): it addresses nobody",
                short_tag(&tag)
            );
            (tag, Some(note))
        }
    }
}

/// [`normalize_to_tag`] on each tag, and the notes.
pub fn normalize_to_tags(tags: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut notes = Vec::new();
    let tags = tags
        .into_iter()
        .map(|tag| {
            let (tag, note) = normalize_to_tag(tag);
            notes.extend(note);
            tag
        })
        .collect();
    (tags, notes)
}

fn short_tag(tag: &str) -> String {
    match tag.char_indices().nth(24) {
        Some((at, _)) => format!("{}...", &tag[..at]),
        None => tag.to_string(),
    }
}

/// What this node will read differently from what a post's delivery keys
/// say (ADR 0022 §2: malformed values fall back to their defaults), for
/// the author to hear. Never an error.
pub fn lenient_notes(kind: &Kind, data: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut notes = Vec::new();
    match kind {
        Kind::Ask => {
            let mode = data.get("mode");
            let known = mode
                .and_then(Value::as_str)
                .map(|m| m.trim().to_ascii_lowercase())
                .filter(|m| ["any", "one", "n", "all"].contains(&m.as_str()));
            if mode.is_some() && known.is_none() {
                notes.push(
                    "data.mode is not one of any, one, n, all: nodes read it as any".to_string(),
                );
            }
            if known.as_deref() == Some("n") && count(data.get("want")).is_none() {
                notes.push(
                    "mode n needs data.want, a positive integer: nodes read it as any".to_string(),
                );
            }
            if data.contains_key("max") && count(data.get("max")).is_none() {
                notes.push(
                    "data.max is not a positive integer: each node's ask.max_responders applies"
                        .to_string(),
                );
            }
        }
        Kind::Claim if data.contains_key("lease") && count(data.get("lease")).is_none() => {
            notes.push(format!(
                "data.lease is not a positive number of seconds: {DEFAULT_LEASE_SECS} applies"
            ));
        }
        _ => {}
    }
    notes
}

/// How `node` groups keys in `net` (ADR 0019 §2.4): its own profiles
/// together, and its reputation table's groups when it has one.
pub fn grouping(node: &crate::node::Node, net: &str) -> Grouping {
    let grouping = Grouping::new(node.local_authors());
    match node.reputation(net) {
        Ok(table) => grouping.with_table(table),
        Err(e) => {
            tracing::debug!("delivery: no reputation table for {net}: {e:#}");
            grouping
        }
    }
}

/// What answers `ask` among `thread`: for an ask that opens a thread, the
/// whole thread; for a follow-up ask inside one (a revision asked of an
/// answerer, say), only what descends from it. It is a request of its
/// own: the root's answers, claims and accept are not its.
pub fn replies_to<'e>(ask: &Event, thread: impl IntoIterator<Item = &'e Event>) -> Vec<&'e Event> {
    let all: Vec<&Event> = thread.into_iter().filter(|e| e.id != ask.id).collect();
    if ask.thread.is_none() {
        return all
            .into_iter()
            .filter(|e| e.thread.as_deref() == Some(ask.id.as_str()))
            .collect();
    }
    let mut below: HashSet<&str> = HashSet::from([ask.id.as_str()]);
    // Parents come before their replies by `ts` but not always in the
    // store's order: grow the set until it is stable.
    loop {
        let before = below.len();
        for e in &all {
            if e.parent.as_deref().is_some_and(|p| below.contains(p)) {
                below.insert(e.id.as_str());
            }
        }
        if below.len() == before {
            break;
        }
    }
    all.into_iter()
        .filter(|e| below.contains(e.id.as_str()))
        .collect()
}

/// The state of the ask `ask` on `node` at `now`, from its thread in the
/// store (for a follow-up ask, what descends from it); `None` when it is
/// not an ask.
pub fn state_on(node: &crate::node::Node, ask: &Event, now: u64) -> Option<State> {
    if ask.kind != Kind::Ask {
        return None;
    }
    let thread = node.store().thread(ask.thread_root(), 1000).ok()?;
    Some(State::of(
        ask,
        thread.iter().map(|s| &*s.event),
        &grouping(node, &ask.net),
        node.config().max_responders,
        now,
    ))
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn count_text(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use ed25519_dalek::SigningKey;
    use serde_json::{Value, json};

    use super::*;
    use crate::event::{Draft, Event, Kind};

    /// ADR 0044 Q2: a Rust agent waits on a Kotlin ask; a generalist, or
    /// an ask in its field or without a field tag, does not.
    #[test]
    fn an_agent_outside_its_field_is_told() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        let rust = s(&["lang:rust", "code"]);
        assert_eq!(
            outside_field(&rust, &s(&["lang:kotlin"])),
            s(&["lang:kotlin"])
        );
        assert!(outside_field(&rust, &s(&["lang:Rust", "lang:kotlin"])).is_empty());
        assert!(outside_field(&rust, &s(&["topic:jvm"])).is_empty());
        assert!(outside_field(&rust, &s(&["need:pdf"])).is_empty());
        assert!(outside_field(&s(&["code"]), &s(&["lang:kotlin"])).is_empty());
        assert!(outside_field(&[], &s(&["lang:kotlin"])).is_empty());
        assert_eq!(
            outside_field(&s(&["topic:web"]), &s(&["topic:audio", "lang:go"])),
            s(&["topic:audio"])
        );
    }
    use crate::identity::author_id;
    use crate::signals::Grouping;

    const T0: u64 = 1_700_000_000;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn id(seed: u8) -> String {
        author_id(&key(seed))
    }

    fn ask(seed: u8, data: Value, tags: &[&str]) -> Event {
        Draft {
            net: "public".into(),
            kind: Some(Kind::Ask),
            title: Some("compile this".into()),
            text: "compile this".into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            data: data.as_object().unwrap().clone(),
            ..Draft::default()
        }
        .sign(&key(seed), T0, 0)
        .unwrap()
    }

    fn reply(seed: u8, to: &Event, kind: Kind, ts: u64, data: Value) -> Event {
        Draft {
            net: to.net.clone(),
            thread: Some(to.thread_root().to_string()),
            parent: Some(to.id.clone()),
            kind: Some(kind),
            text: "x".into(),
            data: data.as_object().unwrap().clone(),
            ..Draft::default()
        }
        .sign(&key(seed), ts, 0)
        .unwrap()
    }

    /// No table: every key is its own group but the `local` ones.
    fn grouping(local: &[u8]) -> Grouping {
        Grouping::new(local.iter().map(|s| id(*s)).collect::<HashSet<_>>())
    }

    fn state(ask: &Event, thread: &[&Event], now: u64) -> State {
        State::of(ask, thread.iter().copied(), &grouping(&[]), Some(3), now)
    }

    /// A follow-up ask in a thread is its own request: only what descends
    /// from it counts, not the root's answers.
    #[test]
    fn a_follow_up_ask_counts_only_its_own_replies() {
        let root = ask(1, json!({}), &[]);
        let answer = reply(2, &root, Kind::Answer, T0 + 10, json!({}));
        let follow = reply(1, &answer, Kind::Ask, T0 + 20, json!({"mode": "one"}));
        let claim = reply(2, &follow, Kind::Claim, T0 + 30, json!({}));
        let redo = reply(2, &claim, Kind::Answer, T0 + 40, json!({}));
        let thread = [&root, &answer, &follow, &claim, &redo];
        let mine: Vec<&str> = replies_to(&follow, thread)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(mine, [claim.id.as_str(), redo.id.as_str()]);
        assert_eq!(
            replies_to(&root, thread).len(),
            4,
            "the root: its whole thread"
        );
        let before = state(&follow, &[&root, &answer], T0 + 25);
        assert!(before.answered.is_empty(), "the root's answer is not its");
        assert_eq!(before.request.mode, Mode::One);
        let after = state(&follow, &thread, T0 + 45);
        assert_eq!(after.answered, [id(2)]);
    }

    #[test]
    fn malformed_values_fall_back_to_the_defaults() {
        for data in [
            json!({}),
            json!({"mode": "split"}),
            json!({"mode": 3}),
            json!({"mode": "n"}),
            json!({"mode": "n", "want": 0}),
            json!({"mode": "n", "want": "many"}),
            json!({"mode": "n", "want": -2}),
        ] {
            let r = Request::of(&ask(1, data.clone(), &[]));
            assert_eq!((r.mode, r.want), (Mode::Any, 1), "{data}");
        }
        let r = Request::of(&ask(1, json!({"mode": " One "}), &[]));
        assert_eq!(r.mode, Mode::One);
        let r = Request::of(&ask(1, json!({"mode": "n", "want": "2"}), &[]));
        assert_eq!((r.mode, r.want), (Mode::N, 2));
        for max in [json!(0), json!("x"), json!(-1), json!(null), json!([3])] {
            let r = Request::of(&ask(1, json!({"max": max}), &[]));
            assert_eq!(r.max, None, "{max}");
            assert_eq!(r.cap(Some(3)), Some(3));
        }
        assert_eq!(
            Request::of(&ask(1, json!({"max": 1_000_000}), &[])).max,
            Some(1000)
        );
        // Leases: malformed is the default, huge is capped.
        let a = ask(1, json!({}), &[]);
        for (data, secs) in [
            (json!({}), DEFAULT_LEASE_SECS),
            (json!({"lease": "soon"}), DEFAULT_LEASE_SECS),
            (json!({"lease": 0}), DEFAULT_LEASE_SECS),
            (json!({"lease": -5}), DEFAULT_LEASE_SECS),
            (json!({"lease": 60}), 60),
            (json!({"lease": "120"}), 120),
            (json!({"lease": 10_000_000}), 1000),
        ] {
            assert_eq!(
                lease(&reply(2, &a, Kind::Claim, T0, data.clone())),
                secs,
                "{data}"
            );
        }
    }

    #[test]
    fn authors_hear_what_is_read_differently() {
        let notes = |kind: Kind, data: Value| lenient_notes(&kind, data.as_object().unwrap());
        assert!(notes(Kind::Ask, json!({"mode": "one", "max": 2})).is_empty());
        assert!(notes(Kind::Ask, json!({"mode": "n", "want": 3})).is_empty());
        assert_eq!(notes(Kind::Ask, json!({"mode": "split"})).len(), 1);
        assert_eq!(notes(Kind::Ask, json!({"mode": "n"})).len(), 1);
        assert_eq!(notes(Kind::Ask, json!({"max": "lots"})).len(), 1);
        assert_eq!(notes(Kind::Claim, json!({"lease": 0})).len(), 1);
        assert!(notes(Kind::Claim, json!({"lease": 60})).is_empty());
    }

    #[test]
    fn to_tags_are_normalized_to_base32() {
        let me = id(2);
        let tag = to_tag(&me).unwrap();
        assert_eq!(tag.len(), 55, "fits in a tag");
        // Hex or base32, either case, give the same tag (tags are
        // lowercase).
        for given in [
            format!("to:{me}"),
            format!("TO:{}", me.to_ascii_uppercase()),
            tag.clone(),
            tag.to_ascii_uppercase(),
        ] {
            assert_eq!(normalize_to_tag(given), (tag.clone(), None));
        }
        // Anything else is kept as is, with a note.
        for odd in [
            format!("to:{}", &me[..32]),
            format!("to:{}", &tag[3..54]),
            "to:abc".to_string(),
            format!("to:{}x", &me[..63]),
        ] {
            let (kept, note) = normalize_to_tag(odd.clone());
            assert_eq!(kept, odd);
            assert!(note.unwrap().contains("addresses nobody"));
        }
        assert_eq!(normalize_to_tag("cap:pdf".into()), ("cap:pdf".into(), None));
        let (tags, notes) = normalize_to_tags(vec![format!("to:{me}"), "to:x".into()]);
        assert_eq!(tags, [tag, "to:x".to_string()]);
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn caps_per_mode() {
        let cap = |data: Value, default| Request::of(&ask(1, data, &[])).cap(default);
        assert_eq!(cap(json!({}), Some(3)), Some(3));
        assert_eq!(cap(json!({"max": 5}), Some(3)), Some(5));
        assert_eq!(cap(json!({}), None), None);
        assert_eq!(cap(json!({"mode": "one", "max": 5}), Some(3)), Some(1));
        assert_eq!(cap(json!({"mode": "n", "want": 5}), Some(3)), Some(5));
        assert_eq!(cap(json!({"mode": "n", "want": 2}), Some(3)), Some(3));
        assert_eq!(cap(json!({"mode": "all"}), Some(3)), None);
        assert_eq!(cap(json!({"mode": "all", "max": 7}), Some(3)), Some(7));
    }

    #[test]
    fn routing_tags() {
        let me = id(2);
        let to = |seed: u8| to_tag(&id(seed)).unwrap();
        let r = Request::of(&ask(
            1,
            json!({}),
            &[
                &to(2),
                // A prefix of an id, hex or base32, addresses nobody.
                &format!("to:{}", &id(3)[..32]),
                &to(4)[..54],
                "to:abc",
                "to:not-base32-not-base32-not-base32-not-base32-not1",
                "cap:pdf",
                "cap:rust",
                "topic:x",
            ],
        ));
        assert_eq!(r.to, std::slice::from_ref(&me));
        assert!(r.addresses(&me));
        assert!(!r.addresses(&id(3)) && !r.addresses(&id(4)));
        assert_eq!(r.caps, ["cap:pdf", "cap:rust"]);
        assert!(r.capable(&["cap:rust".into(), "cap:pdf".into()]));
        assert!(!r.capable(&["cap:rust".into()]), "every cap: is needed");
        // At most four `to:`.
        let tags: Vec<String> = (2..8).map(to).collect();
        let tags: Vec<&str> = tags.iter().map(String::as_str).collect();
        let r = Request::of(&ask(1, json!({}), &tags));
        assert_eq!(r.to.len(), MAX_TO);
        assert!(r.addresses(&id(5)) && !r.addresses(&id(6)));
    }

    #[test]
    fn one_claim_wins_by_ts_then_id() {
        let a = ask(1, json!({"mode": "one"}), &[]);
        let now = T0 + 10;
        let c2 = reply(2, &a, Kind::Claim, T0 + 5, json!({}));
        let c3 = reply(3, &a, Kind::Claim, T0 + 5, json!({}));
        let c4 = reply(4, &a, Kind::Claim, T0 + 4, json!({}));
        // The earliest wins.
        let s = state(&a, &[&a, &c2, &c3, &c4], now);
        assert_eq!(s.winners, std::slice::from_ref(&c4.id));
        assert_eq!(s.verdict(&id(4)), Verdict::Wake(Expect::Continue));
        assert!(s.lost(&id(2)).is_some() && s.lost(&id(3)).is_some());
        assert!(matches!(
            s.verdict(&id(5)),
            Verdict::Skip { until: Some(_), .. }
        ));
        // Same second: the lowest id wins.
        let s = state(&a, &[&c2, &c3], now);
        let (low, high) = if c2.id < c3.id { (2, 3) } else { (3, 2) };
        assert!(s.lost(&id(low)).is_none());
        assert!(s.lost(&id(high)).is_some());
        assert!(matches!(
            s.verdict(&id(high)),
            Verdict::Skip { until: None, .. }
        ));
        // The loser releases: the winner is alone.
        let loser = if high == 2 { &c2 } else { &c3 };
        let release = reply(high, loser, Kind::Release, T0 + 5, json!({}));
        let s = state(&a, &[&c2, &c3, &release], now);
        assert_eq!(s.claims.len(), 1);
        assert_eq!(s.responders, 1);
    }

    #[test]
    fn n_takes_want_claims_from_distinct_groups() {
        let a = ask(1, json!({"mode": "n", "want": 2}), &[]);
        let now = T0 + 10;
        let c2 = reply(2, &a, Kind::Claim, T0 + 1, json!({}));
        let c3 = reply(3, &a, Kind::Claim, T0 + 2, json!({}));
        let c4 = reply(4, &a, Kind::Claim, T0 + 3, json!({}));
        let s = state(&a, &[&c2, &c3, &c4], now);
        assert_eq!(s.winners, [c2.id.clone(), c3.id.clone()]);
        assert!(s.lost(&id(4)).is_some());
        // 2 and 3 on one node: one slot for both.
        let local = grouping(&[2, 3]);
        let s = State::of(&a, [&c2, &c3, &c4], &local, Some(3), now);
        assert_eq!(s.winners, [c2.id.clone(), c4.id.clone()]);
        assert!(s.lost(&id(3)).is_some());
        assert_eq!(s.responders, 2);
        // An answer takes a slot first.
        let answer = reply(5, &a, Kind::Answer, T0 + 4, json!({}));
        let s = state(&a, &[&c2, &c3, &answer], now);
        assert_eq!(s.winners, std::slice::from_ref(&c2.id));
        assert!(s.lost(&id(3)).is_some());
    }

    #[test]
    fn leases_expire_and_progress_renews_them() {
        let a = ask(1, json!({"mode": "one"}), &[]);
        let claim = reply(2, &a, Kind::Claim, T0 + 1, json!({"lease": 60}));
        let s = state(&a, &[&claim], T0 + 30);
        assert_eq!(s.claims[0].expires, T0 + 61);
        assert_eq!(
            s.verdict(&id(3)),
            Verdict::Skip {
                why: format!("mode one: claimed by {} (lease 31 s left)", &id(2)[..12]),
                until: Some(T0 + 61)
            }
        );
        // Expired: the task is free again.
        let s = state(&a, &[&claim], T0 + 61);
        assert!(s.claims.is_empty());
        assert_eq!(s.verdict(&id(3)), Verdict::Wake(Expect::ClaimFirst));
        // A progress renews it; another author's does not.
        let progress = reply(2, &claim, Kind::Progress, T0 + 50, json!({}));
        let other = reply(4, &claim, Kind::Progress, T0 + 100, json!({}));
        let s = state(&a, &[&claim, &progress, &other], T0 + 61);
        assert_eq!(s.claims[0].expires, T0 + 110);
        // An answer from the claimer ends the claim: answered, at the cap.
        let answer = reply(2, &a, Kind::Answer, T0 + 70, json!({}));
        let s = state(&a, &[&claim, &answer], T0 + 71);
        assert!(s.claims.is_empty());
        assert!(matches!(
            s.verdict(&id(3)),
            Verdict::Skip { until: None, .. }
        ));
        // An explicit release frees it at once.
        let release = reply(2, &claim, Kind::Release, T0 + 2, json!({}));
        let s = state(&a, &[&claim, &release], T0 + 3);
        assert_eq!(s.verdict(&id(3)), Verdict::Wake(Expect::ClaimFirst));
    }

    #[test]
    fn any_counts_answers_and_claims_against_the_cap() {
        let a = ask(1, json!({}), &[]);
        let now = T0 + 10;
        let c2 = reply(2, &a, Kind::Claim, T0 + 1, json!({}));
        let s = state(&a, &[&c2], now);
        assert_eq!(
            s.verdict(&id(5)),
            Verdict::Wake(Expect::AbstainUnlessDifferent)
        );
        assert_eq!(
            state(&a, &[], now).verdict(&id(5)),
            Verdict::Wake(Expect::Answer)
        );
        let a3 = reply(3, &a, Kind::Answer, T0 + 2, json!({}));
        let a4 = reply(4, &a, Kind::Answer, T0 + 3, json!({}));
        let s = state(&a, &[&c2, &a3, &a4], now);
        assert_eq!(s.responders, 3);
        assert!(matches!(s.verdict(&id(5)), Verdict::Skip { .. }));
        // A responder is not stopped by the cap.
        assert_eq!(s.verdict(&id(2)), Verdict::Wake(Expect::Continue));
        // The asker's own answers and claims do not count.
        let own = reply(1, &a, Kind::Answer, T0 + 4, json!({}));
        assert_eq!(state(&a, &[&own], now).responders, 0);
        // `all`: no cap unless set.
        let all = ask(1, json!({"mode": "all"}), &[]);
        let replies: Vec<Event> = (2..8)
            .map(|s| reply(s, &all, Kind::Answer, T0 + 1, json!({})))
            .collect();
        let s = State::of(&all, replies.iter(), &grouping(&[]), Some(3), now);
        assert_eq!(s.verdict(&id(9)), Verdict::Wake(Expect::Answer));
    }

    #[test]
    fn an_accept_closes_the_ask() {
        let a = ask(1, json!({}), &[]);
        let answer = reply(2, &a, Kind::Answer, T0 + 1, json!({}));
        let claim = reply(3, &a, Kind::Claim, T0 + 1, json!({}));
        let accept = reply(1, &answer, Kind::React, T0 + 2, json!({"accept": true}));
        let s = state(&a, &[&answer, &claim, &accept], T0 + 3);
        assert!(s.accepted);
        assert!(matches!(
            s.verdict(&id(4)),
            Verdict::Skip { until: None, .. }
        ));
        assert!(matches!(s.verdict(&id(3)), Verdict::Skip { .. }));
        // Only the asker accepts; a withdrawal reopens it.
        let other = reply(5, &answer, Kind::React, T0 + 2, json!({"accept": true}));
        assert!(!state(&a, &[&answer, &other], T0 + 3).accepted);
        let withdrawn = reply(1, &answer, Kind::React, T0 + 4, json!({"accept": false}));
        assert!(!state(&a, &[&answer, &accept, &withdrawn], T0 + 5).accepted);
    }

    #[test]
    fn prompt_lines_and_views_say_what_is_expected() {
        let a = ask(1, json!({"mode": "one"}), &[]);
        let claim = reply(2, &a, Kind::Claim, T0 + 1, json!({"lease": 60}));
        let s = state(&a, &[&claim], T0 + 11);
        let line = s.prompt_line(&id(2), T0 + 11);
        assert!(line.contains("mode one"), "{line}");
        assert!(line.contains(", yours (50 s left)"), "{line}");
        assert!(line.contains("1 of 1 responders"), "{line}");
        assert!(line.contains("your claim holds"), "{line}");
        let view = s.view(&id(3), T0 + 11);
        assert_eq!(view["mode"], "one");
        assert_eq!(view["cap"], 1);
        assert_eq!(view["expected"], "abstain");
        assert_eq!(view["claims"][0]["lease_left_secs"], 50);
        let open = state(&a, &[], T0 + 11);
        assert_eq!(open.view(&id(3), T0)["expected"], "claim_first");
    }
}
