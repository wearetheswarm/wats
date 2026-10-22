# 0034. Daily agent budgets: dollars, minutes, LLM tokens

Status: Proposed (2026-10-22). Ships in 0.4.5.

Amends ADR 0011 ("Budget", 2026-10-16): the dollar budget stays, two
measures join it, each monthly limit is derived from its daily one, and 0
means no limit. Asked by the setup wizard of ADR 0027 (its 0.4.5 section).

Code: `crates/wats/src/budget.rs` (`Budget::of`, `Budget::for_runtime`,
`check`, `describe`, `status_json`, `reports_cost`, `reports_tokens`),
`crates/wats/src/runs.rs` (`read_result`: the LLM tokens of each run),
`crates/wats/src/agent.rs` (the check before each run, `AgentInfo::status`),
`crates/wats/src/firstrun.rs` (`ask_budget`, `preset`, `parse_daily`),
`crates/wats/tests/agent.rs` (`the_minutes_budget_stops_new_runs`,
`the_llm_token_budget_stops_new_runs`).

Source: product decisions by Vincent (2026-10-22): daily budgets, with the
monthly one derived; the budget always asked by the wizard, by how the
agent is paid; and the guide "set it to what you're willing to give the
swarm from your subscription, machine or API spend". That share is what
makes a node fair to the swarm without hurting its owner.

## Context

Until 0.4.4 the node held an agent to dollars only: `agent.max_usd_per_day`
(2) and `agent.max_usd_per_month` (20), from the cost the agent CLI
reports, and a number of runs per day for the runs that report none. On a
subscription (Claude Pro/Max, ChatGPT) the reported cost is an estimate of
API prices that nobody pays: the real limit is the provider's usage
window, so a dollar cap stops the agent for no reason, or not at all. Codex
reports no cost at all, so its only budget was its runs. A self-hosted
model costs the machine's time.

## Decision

### 1. Three measures, per UTC day and per UTC month

| Key | Default | Measure |
|---|---|---|
| `agent.max_usd_per_day` | 2 | dollars, from the cost the CLI reports |
| `agent.max_minutes_per_day` | 0 | the wall time of the runs, measured by the node |
| `agent.max_tokens_per_day` | 0 | LLM tokens (model input, cached input included, and output), as the CLI reports them |

- **Each month is 30 × its day**, unless `agent.max_usd_per_month`,
  `agent.max_minutes_per_month` or `agent.max_tokens_per_month` is set.
  `agent.max_usd_per_month` keeps working as an override, for
  compatibility.
- **0 is no limit**, for a day and for a month, the convention of
  `sync.initial_max`. A day of 0 makes its derived month 0 too; a month
  set by hand still applies.
- The windows, the ledger and its two months, the pause until the window
  resets, the log line, and the events kept for the next run are those of
  the dollar budget (ADR 0011). The ledger lines gain `secs` and `tokens`;
  an older line counts as 0 of each, and an older wats ignores the new
  fields.
- **Minutes cap each run and stop new ones** (§6). Claude Code still gets
  the dollars left as `--max-budget-usd`, and no such flag without a
  dollar limit.
- `agent.max_runs_per_day` (48) still counts the runs without a reported
  cost.

### 2. What each runtime is held to

Checked against what each CLI writes, and the fake CLIs of the tests that
mirror it:

| Runtime | Dollars | Minutes | LLM tokens |
|---|---|---|---|
| Claude Code | `total_cost_usd` of its stream-json `result` | yes | the `usage` of its `result`: `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, `output_tokens` |
| Codex (`exec --json`) | none: runs counted instead | yes | the `usage` of each `turn.completed`: `input_tokens` (its `cached_input_tokens` are a part of it) and `output_tokens`, summed over the turns |
| OpenClaw (the gateway's hook) | none | yes: the time to the gateway's answer | none: the gateway returns no model output, token count or cost |
| OpenCode (`run --format json`, ADR 0031) | the `cost` of each `step_finish`: OpenCode's estimate from catalog prices, while `opencode::ENFORCE_USD_BUDGET` is on (a cost of 0 is no cost) | yes | the `tokens` of each `step_finish`: `input`, `output`, `reasoning`, `cache.read`, `cache.write` (OpenCode keeps each apart), summed over the steps |
| Mistral Vibe (`-p`, ADR 0033) | its export's `cost_usd` (its own and its subagents'), Vibe's estimate, while `vibe::ENFORCE_USD_BUDGET` is on; a run killed before its export is charged the dollars it was allowed (its `--max-price`, ADR 0033) | yes | its export's `usage`: `total_tokens`, else `input_tokens` (`cached_input_tokens` a part of it) + `output_tokens`; none for a run killed before its export |

`Budget::for_runtime` drops the limits a runtime cannot be held to (the
dollars of Codex and OpenClaw, and of OpenCode or Vibe when their
`ENFORCE_USD_BUDGET` is off; the LLM tokens of OpenClaw), so that they are
neither enforced nor shown as if they were. The cost of OpenCode and Vibe
is shown as their estimate, not the bill.

Cached input is counted. It is most of what an agent run reads, it is what
the provider meters (at a lower price), and leaving it out would make the
count depend on the provider's caching. The wizard's token default, used
only for Codex on an API key, is sized with it.

### 3. Asked by the wizard, by how the agent is paid

In step 1, in quick and detailed mode alike (ADR 0027, 0.4.5): "How is
this agent paid? 1) subscription (Claude Pro/Max, ChatGPT…) 2) API key,
pay per use 3) self-hosted or free model". The answer picks the measure
and its daily defaults:

| Answer | Dollars | Minutes | LLM tokens |
|---|---|---|---|
| subscription | 0 (a cost estimate is meaningless there) | 120 | 0 |
| API key | 5 | 0 | 0 |
| self-hosted | 0 | 240 (the machine's time) | 0 |

Adjusted for the runtimes, by what each reports rather than by name: an
API key is $5 where the runtime reports a cost, else **5M LLM tokens** where
it reports them (Codex), else 120 minutes. Cached input counts toward the
tokens (§2), so 5M is a few dozen Codex runs with most of their input
cached; it is a starting point, kept rather than lowered. OpenClaw is not
asked how it is paid (its spend caps are its own): its line has minutes
only, 0 by default.

**How it is paid has no Enter default** (review, 2026-10-22): the person
types 1, 2 or 3, unless config.toml already says a budget (its answer is
the default) or the runtime's API key is in the environment
(`ANTHROPIC_API_KEY` for Claude Code, `OPENAI_API_KEY` for Codex,
`MISTRAL_API_KEY` for Vibe; not for OpenCode, which reaches many
providers): then 2, said in one line. The end of the input with no
default is no answer: nothing changes. **Without a terminal it is not
asked at all**: the budget keys and the old-default keys (ADR 0027 §7)
stay as they are, and the setup revision stays (§5), as for
`update.security_auto` (ADR 0030).

Then one line shows the result, for the measures the runtime is held to:
"Daily budget: 120 min, no USD limit, no LLM token limit (monthly: 30×).
Enter to accept, or change it (e.g. min=90 usd=3 tokens=2M; 0: no
limit)". The syntax: `name=value` items in any order, separated by spaces
or commas, the others kept; `min` (whole minutes, or hours with `h`),
`usd` (an optional `$`), `tokens` (`k` and `M` suffixes); 0 or `none` for
no limit. A wrong line is asked again. A month set in config.toml is
named on the line. When config.toml already sets a budget, the answer it
suggests is the default, and its own values are shown.

The values are written only where they differ from the defaults (ADR
0027, 0.4.5), so a subscription writes `max_usd_per_day = 0.0` and
`max_minutes_per_day = 120`. That also unsets a value the user had pinned
when it equals today's default (`max_usd_per_day = 2` set by hand): the
key then follows the default if a release changes it. When setup switches
the node to another runtime, the new agent's own table (or the defaults)
is the baseline, not the previous agent's values.

### 4. Where it shows

The banner, `wats agent runs` and the per-agent line of `wats status`
describe each measure: `$0.23 of $2.00 today, $5.10 of $60.00 this month ·
12 of 120 min today, 340 of 3600 this month · 34.5k LLM tokens today (no
limit)`, with the runs instead of dollars for a runtime that reports no
cost, and no tokens for one that reports none. `wats status --json` and
the MCP `status` tool add each agent's `usage`: `today` and `month`
(`usd`, `minutes`, `tokens`, `runs`; `null` for what the runtime does not
report) and `limits` (`null`: no limit).

### 5. A USD budget of 0 from before 0.4.5 stays a pause

(Vincent, 2026-10-22, on the review.) Until 0.4.5, `max_usd_per_day = 0`
or `max_usd_per_month = 0` paused the agent; since, it means no limit. An
upgrade must not turn a pause into free spending by itself. So when an
agent's table (`[agent]` or `[agents.<name>]`) sets one of them to 0 and
the data directory's setup revision is below 2 (`ZERO_CONFIRMED_REVISION`:
the revision whose budget question settles it) and the agent's budget is
not confirmed (no `<agent dir>/budget-confirmed`), the agent **stays
paused**: `check` returns a pause "until you run `wats setup`", whatever
was spent, and no run starts. The confirmation is **per agent** (review,
2026-10-22): one agent's answer never lifts another's pause.

- One loud line says it, in `wats status` and the MCP `status`
  (`budget_paused`, and the agent's `notice`), and at the node's start:
  "agent X: max_usd_per_day = 0 meant paused before 0.4.5 and now means
  no limit; it stays paused until you run `wats setup` (or set
  agents.X.enabled = false to keep it off)". The agent's budget line
  starts with "paused until `wats setup`".
- The budget question settles it for its agent, in `wats setup` and in
  the catch-up of a bare `wats` on a terminal: it says what the 0 meant,
  offers the preset of the answer (never the 0 as it is), writes the
  budget and writes the agent's `budget-confirmed`. Every other agent
  with such a 0, the old `[agent]` table included (which `wats --agent
  <runtime>` runs from `<data dir>/agent`), then gets its own budget
  question, written to its own table and confirmed; one left unanswered
  (the end of the input) stays paused, with a warning.
- **Revision 2 is recorded only when a person answered the budget
  question (with an agent) and no agent is left with an unconfirmed 0.**
  Otherwise the revision stays, so that the next bare `wats` on a
  terminal asks again (a catch-up does not ask an agent already
  confirmed). Without a terminal nothing is asked, nothing is confirmed
  and the revision stays.
- Codex is held to it too (a 0 paused it then, though it reports no
  cost); OpenClaw is not (its USD budgets never applied).

### 6. Each run within the budget, and runs that never end

(Review, 2026-10-22.)

- **The minutes left cap the run's timeout.** A run gets `agent.timeout`,
  or less: the minutes left today and this month, never under one
  minute. With less than a minute left, no run starts: the budget is
  paused until the window resets. The run time a run actually used is
  what the ledger counts.
- **LLM tokens cannot be capped per run** for most runtimes: Claude Code,
  Codex and OpenCode have no flag for it, so a run may go past the tokens
  left, and the next one does not start. Mistral Vibe has one: it gets
  `--max-tokens <left>` (unless its extra arguments set it), as it gets
  `--max-price <dollars left>`.
- **A run that never ends is charged.** When a run starts, the ledger gets
  a provisional entry with its id and its caps: its timeout, and the
  dollars it may spend (Claude Code's `--max-budget-usd` and, for
  OpenCode, which has no per-run cap, the same dollars left; Vibe's
  `--max-price`; none for Codex and OpenClaw). The run's own entry
  replaces it when it ends, also when the node stops it gracefully; a run
  that ran no turn (refused by OpenClaw's gateway, deferred) removes it. A
  provisional entry still there when a runner starts is from a run whose
  node crashed or was killed (SIGKILL, also the 10 s-then-kill of `wats
  stop` and of setup's restart offer when the node does not stop in
  time): it is charged its caps and marked `unfinished`, and the runner
  logs it once. **For Claude Code and OpenCode that is everything left of
  the day's (or month's) dollars when the run started, which can pause the
  agent until the next UTC day.** Provisional entries are not counted while
  their run runs.
- **The ledger keeps what it cannot read.** A line that is not an entry
  (a torn write, a newer format) is kept as it is, also when old entries
  are pruned; each entry is written in one append with its newline, after
  a newline first when the file does not end with one; a rewrite goes
  through a temporary file, synced to disk before it is renamed, and the
  directory synced after the rename (unix): a power loss leaves the old
  ledger or the new one, never an empty one.

## Alternatives

- **Keep dollars only, with a "no limit" for subscriptions.** Rejected:
  the agent then has no budget at all on a subscription, and the
  provider's window is shared with its owner's own sessions.
- **A monthly budget asked apart.** Rejected: one number a day is what
  people can size; 30× is a plain default, and the monthly keys stay for
  those who want another.
- **LLM tokens without cached input.** Rejected (§2): the count would
  depend on the provider's caching.
- **A token budget for every runtime, 0 where unknown.** Rejected: a limit
  that is never reached would read as enforced.
- **Cut a run when the minutes run out, whatever its timeout.** Rejected
  as such: the run's timeout is set to the minutes left instead (§6), so
  the cut is the timeout's, known at the start, and never under a
  minute.

## Consequences

- **Rolling back to 0.4.4 with a run in progress, or an unsettled one:**
  0.4.4 reads a provisional line as a run without a reported cost, and its
  prune of old entries rewrites the line without the `provisional` flag.
  After a rollback and a new upgrade, that run can count twice (once as
  0.4.4 kept it, once settled), in the runs per day. Accepted: it needs a
  rollback during or right after a crash, and errs on the side of the
  budget.
- **Hand edits can bypass the per-agent confirmation (§5)**: renaming an
  agent's table onto the name of a confirmed agent, or setting
  `legacy_dir = true` onto `<data dir>/agent` when that directory is
  confirmed, carries the confirmation over. Accepted: the marker guards
  upgrades, not deliberate edits of config.toml.

- **0 now means no limit for `agent.max_usd_per_day` and
  `agent.max_usd_per_month`.** Before 0.4.5 a 0 paused the agent; such a
  0 stays a pause until `wats setup` confirms the budget (§5), and the
  CHANGELOG says so.
- **A config.toml that sets `max_usd_per_day` and not the month gets 30 ×
  the day**: the default month goes from $20 to $60. `wats config set
  agent.max_usd_per_month 20` keeps the old one.
- The new keys are refused by wats 0.4.4 and older: `wats upgrade
  --rollback` names them, with their agent's table, before it swaps
  anything (`KEYS_SINCE`).
- They are safety keys (ADR 0011, addendum): a change shows until `wats
  config ack`.

## Later

- A per-run LLM token cap for the runtimes that get one (Claude Code,
  Codex and OpenCode have none today).
- A per-node budget across agents, if several agents share one
  subscription.
