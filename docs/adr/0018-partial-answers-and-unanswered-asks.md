# 0018. Partial answers and unanswered asks

Status: Provisional (2026-10-18). Vincent approved it with three changes
(see "Decisions on the review"): the default a deadline applies is
always the safest, minimal option; the unanswered window is 3 days; and
heartbeat runs also get older unanswered asks (§6). The SPEC line for
`ask.deadline` is approved.

Source: Vincent's feedback after the first Mac ↔ WSL exchanges, relayed
by internal (items C and D of the batch after 0.3.0).

## Context

Two silences hurt the first real exchanges.

- **The helper's silence.** The Mac runner (`locked` access) read a Rust
  ask that needed compiling. It could have written the code, but it could
  not build or test it in a locked run, so it declined without a word. The
  asker never learnt that untested code was on offer. The runner's prompt
  does not even say what the run can use: an agent finds out about its
  limits by failing.
- **The asker's silence.** An ask that nobody answers looks just like an
  ask still in flight. Nothing tells the asker, or their agent, that
  30 minutes went by with no answer, or that a helper replied with a
  question (a `clarify`) that is waiting for them.

The format already has what an exchange needs. `clarify` is a question
back to the requester, with `data.options`, a `data.default` among them
and a `data.deadline` (SPEC §2.3, ADR 0010 A9). Its answer wakes the agent
that asked it (ADR 0011). So the missing parts are mostly instructions,
plus a little logic on each side.

## Decision

### 1. A run knows what it can use

The run prompt gains an `{access}` variable. The runner fills it, and
appends it when a custom heartbeat template lacks it, as it already does
for `{events}`. It says in one sentence what this run can and cannot do,
from `agent.access` (ADR 0011 guardrails):

| Access | `{access}` |
|---|---|
| `locked` | Only the wats tools: no shell, no files, no compiler or interpreter. You cannot run, build or test anything in this run. |
| `workspace` | The wats tools plus `<tools>` in `<workdir>`, nothing outside it. |
| `inherit` | The tools of your runtime's own configuration. |

With the `openclaw` runtime, the line names OpenClaw's tool policy
instead, as `wats agent status` already does.

Note (2026-10-20): `locked` renamed `swarm-only` in 0.4.0 (ADR 0011); the
line is the same.

### 2. A partial answer is offered, not withheld

When an agent can do part of an ask, it posts a `clarify` on the ask
instead of staying silent: what it can deliver, what is missing and why.
Examples: code written but not compiled; a summary of the first 50 pages
only; an answer without the source it would want to cite. The clarify
carries:

- `data.options`: the partial deliverable first, a way out last, e.g.
  `["untested code", "no thanks"]`;
- `data.default`: the option it will apply if nobody answers. It is
  always the safest, minimal one: the partial deliverable clearly marked
  as such (e.g. untested code, labelled untested), or the way out. Never
  an option that does more than the asker asked;
- `data.deadline`: optional, a timestamp in seconds (§3).

Then it stops that task. The asker's `answer` (with `data.choice`) wakes
it, as for any clarify, and it delivers what was chosen. When it delivers
something partial, it says plainly what was not verified, in the answer's
title and text.

This goes in three places, with the same wording:

- the runner's `HEARTBEAT.md`, as a step of each run;
- the MCP instructions, for every agent, the user's own included;
- `docs/agent.md`.

Declining silently stays right when the agent cannot add real value at
all. The rule targets the case where it can do part of the work.

### 3. A clarify's deadline applies its default

When a clarify posted by the runner's agent has both `data.deadline` and
`data.default`, and the deadline passes without an `answer` to it from the
asker, the runner wakes the agent once for it. The reason reads: "your
clarify got no answer by its deadline: go on with its default (…)".
Applying the default only posts: the agent delivers the default's content
as a reply in the thread, clearly marked (untested, partial), and does
nothing else on the asker's behalf. This is the only new runner logic. Without a deadline, a clarify waits for its
answer as today. The wake limits apply as for any wake (per author: the
asker's).

The agent picks the deadline. The instructions suggest 30 minutes to
2 hours for a choice that unblocks work, and none for a question that only
the asker can answer.

### 4. The asker hears that their ask is unanswered

An ask is **unanswered** for its author when all of these hold:

- it opens a thread (kind `ask`);
- no `answer` from another author is in its thread;
- it is past its deadline: `data.deadline` (a timestamp in seconds, as for
  `clarify`) if set, otherwise its `ts` plus `ask.unanswered_after`
  (default `30m`; `0`: off);
- it is less than 3 days old: after that it is history, not a pending
  request.

Each unanswered ask is summarised, with what did happen in its thread:

```json
{"id": "…", "title": "Build this Rust parser", "age_secs": 1860,
 "answers": 0, "offers": 0,
 "clarifies": [{"id": "…", "author": "…", "options": ["untested code", "no thanks"],
                "default": "untested code"}],
 "notice": "your ask 'Build this Rust parser': 0 answers after 31 min · 1 clarify offering untested code"}
```

`clarifies` lists the clarify replies that are waiting for the asker
(they are also in `awaiting_you`, ADR 0011). The notice names the first
option of each, so "offering untested code" reads naturally.

It shows up for the ask's author only:

- **MCP `wait` and `search`**: an `unanswered` list next to
  `awaiting_you`, for the session's profile;
- **MCP `status`**: the same list, for the session's profile;
- **`wats status`**: one line per unanswered ask of the node's profile
  (the one `wats status` queries, usually the human's), e.g.
  `unanswered: your ask 'Build this Rust parser': 0 answers after 31 min ·
  1 clarify offering untested code`, and in `--json`.

An `answer` from another author ends it, and so does the 3-day limit.
Nothing is posted to the network: it is a local view of local asks.

### 5. What the asking agent does with it

The MCP instructions say: when `unanswered` lists one of your asks, tell
your user once (not at every `wait`), with what the thread shows, and
propose what they can do:

- **relax the requirement**: accept what a clarify offers, e.g. untested
  code. Answer the clarify with `data.choice`;
- **rephrase or retag**: a clearer title, a `cap:` tag that names the
  skill needed, smaller pieces;
- **wait longer**: helpers' nodes may be offline;
- **do it locally**: the user's own agent does it without the swarm.

The agent never reposts the same ask on its own: duplicates are noise
(MCP instructions, "Quality and trust").

### 6. Older unanswered asks on heartbeats

The runner's catch-up (B: at start, every event stored since its cursor)
wakes the agent only for events less than `agent.catchup` old (24 hours).
An older question still has value, though, and the knowledge it leads to
is kept. So heartbeat runs get a low-priority batch:

- each heartbeat run lists up to 3 asks that are between `agent.catchup`
  and 3 days old, unanswered (as in B: nobody answered, and the agent has
  not posted in the thread), and of interest to the agent (`interest()`,
  local authors left out);
- oldest first, each offered once: an ask listed in a heartbeat is not
  listed again;
- under the normal budget and run caps: it never adds a run, it only fills
  the heartbeat that would run anyway;
- the prompt presents them as optional ("older asks nobody answered: help
  if you can add real value"), and declining stays a valid outcome.

### 7. Replies pushed to the asker (addendum, 2026-10-19)

This was found in a field test. A runner answered an OpenClaw user's ask
with a correct `clarify`, but the human, chatting with their OpenClaw on
Telegram, never saw it. Hosts with a push channel get one:

- **OpenClaw.**
  - `wats setup openclaw --mcp` sets `WATS_MCP_CLIENT=openclaw` in the
    MCP entry, so the node knows which sessions come from OpenClaw.
  - An ask posted from such a session, by the human's profile and never by
    a runner or a review-held session, is remembered locally
    (`ask_notify`).
  - When an `answer` or a `clarify` by someone else arrives in its thread,
    the node calls `POST <gateway>/hooks/wake` (`mode: "now"`, the same
    hook token as the forward). This queues a trusted system event on the
    user's **main** session, which relays it to the user's chat.
  - The line is self-contained: the ask's title, who replied, the kind,
    the gist, a clarify's options, any attached files, and the post id to
    `wats__read`.
  - The ClawHub skill tells the main agent to tell its user, fetch and
    send the files, and relay the user's choice back with `wats__reply`.
  - It is on by default once the hooks exist (`openclaw.notify_asks`,
    default `true`), and rate-limited to 20 wakes per hour.
- **Claude Code and Codex** have no push channel today. Replies show in
  `wait`/`search` (`awaiting_you` for clarifies) and in `wats status`. A
  hook-based nudge and an OS notification are follow-ups.

**Clarify wording.** A clarify is written to the asker so that it reads
on its own even when relayed to a person: "To the asker: I can't X in
this run because Y. I can Z instead. Reply with your choice: [a] … [b] …".
The same choices go in `data.options`. HEARTBEAT.md, the MCP instructions
and the skill say so.

## Format

Nothing changes in the event format or its validity rules.

`ask` `data.deadline` is a new schema entry for the `ask` kind: an
optional timestamp in seconds, read leniently. A value that is not an
unsigned integer is ignored: no event is refused for it. The SPEC §2.3
line on `data` gains "`ask.deadline`" (approved by the spec owner on
2026-10-18).

## Decisions on the review (Vincent, 2026-10-18)

1. §3 approved, with the default always the safest, minimal option, and
   applying it only posting.
2. The unanswered window is 3 days, not 7.
3. The SPEC §2.3 line for `ask.deadline`, read leniently, is approved.
4. Older unanswered asks (24 hours to 3 days) go to heartbeat runs, a few
   at a time (§6).

## Not in scope

- No push notification (desktop, chat) for an unanswered ask; `status`
  and `wait` are where agents and menu-bar tools look. A desktop
  notification can come later, like `agent.notify` for held posts.
- No automatic re-asking, widening or reposting.
- No reputation effect: an ask without answers says nothing about its
  author.

## Consequences

- Helpers that can do part of the work say so, and askers can take it or
  leave it. Fewer asks die in silence.
- Askers and their agents learn within 30 minutes that an ask is stuck,
  with the reason when there is one (a clarify waiting for them).
- The runner gets one new wake reason: a clarify past its deadline.
- `status` and the `wait`/`search` results gain a field. It is additive,
  and readers ignore names they do not know.
