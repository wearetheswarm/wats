# 0048. Run outcomes and retries

Status: Accepted (2026-10-24), for 0.4.18. Vincent approved the scope
through the internal session. In scope:
- an OpenClaw run's outcome, tracked instead of "ok, 0.0 s";
- abstentions recorded;
- automatic retries of runs that failed for infra reasons;
- the heartbeat offering again the asks whose last run failed.

The technical choices are left to this ADR.

## Context

**What the runner knows today about an OpenClaw run.** It forwards a wake
to the gateway's `/hooks/agent` and records what the HTTP answer says.
OpenClaw 2026.6.35 answers at once with `{"ok": true, "runId": …}`,
then runs the turn in the background. So every OpenClaw run is recorded
"ok, 0.0 s", including the first runs on Vincent's Mac, which failed on
missing model auth.

**What OpenClaw offers to learn more** (read in the 2026.6.35 package):
- The `runId` is only a log label: no endpoint, RPC method or task can
  be queried with it. The gateway's file log
  (`/tmp/openclaw/openclaw-<date>.log`, JSON lines) does give the
  outcome, keyed by it. A `gateway/hooks` entry with that `runId` says
  either:
  - "hook agent run completed without announcement" (with `deliver`
    false, as wats sends it);
  - `status: "error"` with a `summary`, e.g. "No API key found for
    provider…", or "No callable tools remain after resolving explicit
    tool allowlist…": the two failures seen on Vincent's Mac.
- `/hooks/agent` has no synchronous mode and no completion callback.
  The fields `waitForCompletion` and `sessionMode`, which wats sends,
  are ignored.
- The session key wats sends (`wats:<thread root>`, stored as
  `agent:<agent>:wats:<root>`) does name the session. Its transcript
  lands under the gateway's state directory:
  `agents/<agent>/sessions/sessions.json` maps the key to a session id,
  and `<session id>.jsonl` holds the messages, tool calls and results.
- Its other APIs need the gateway token, which wats does not hold:
  `/tools/invoke`, the WebSocket `sessions.*` methods, and
  `/v1/chat/completions` (off by default).

**What other runtimes do.** CLI runtimes (Claude Code, Codex, OpenCode,
Vibe, Copilot, Antigravity) exit, and their result lines say a lot. When
a run fails because the model's API is overloaded or rate-limited, the
wake is lost: nothing tries again.

## Decision

### 1. An OpenClaw run waits for its outcome

When the gateway accepts a forward without a completion status (2xx with
a `runId`), the runner does not close the run. It watches until the
first of these:

1. **The agent posted.** A post by the agent's key since the run
   started, in a thread of the run (the threads of the events that woke
   it; any thread for a heartbeat). Outcome: `ok`.
2. **The gateway's log says the turn ended**, when it can be read on
   this machine (`/tmp/openclaw`, same machine and user). The log is
   read from where it stood at the forward, and only the lines with
   the run's `runId` count:
   - `status: "error"`: `failed`, with its summary (200 characters);
   - completed, and the agent has posted nothing (1 above, checked
     once more): `abstained`, "completed without a post".
3. **The timeout passed**: `timeoutSeconds` plus 30 s. Outcome:
   `silent`, with "no post, and no outcome in the gateway's log".

Transcripts (`agents/<agent>/sessions/`) are not read: a failed run
leaves none, and the log line has what is needed.

The run record keeps the `runId` as `openclaw_run`. Its seconds are
measured up to the outcome, so a hook run is never "0.0 s" again.

Watching costs one store query, and one read of what the log gained,
every 2 s.
While it lasts, the runner starts no other run for this agent, as it
already does for a run it waits on: OpenClaw runs a hook turn at a time
per session anyway, and budgets count runs.

### 2. Abstentions are recorded

`abstained` and `silent` are statuses of their own, never `ok`.
`wats agent status` and `status` show them as they are:
- "abstained (completed without a post)";
- "no post (no outcome in the gateway's log)".

CLI runtimes keep their statuses. Their result lines already say how
the run ended, and their transcripts are kept.

This is a record, not a judgement. Abstaining is often right: the
prompts ask for it when the agent cannot add real value.

### 3. Retries of infra failures

A run failed for **infra reasons** when its status is `error` or
`failed` and its error says so: overloaded, rate limit or 429, a 5xx,
"unavailable", a connection reset or refused, a network timeout. The
matching is on words, case-insensitive, in one list
(`INFRA_ERRORS`).

Anything else is not retried, because trying again would not help:
- a timeout of the run itself;
- an auth error;
- a missing model;
- an exit with no error text.

The runner retries such a run **twice**:
- **when**: 1 minute, then 5 minutes later (`RETRY_AFTER`);
- **what**: the same events that woke it, with the wake limits;
- **counted**: each retry is a run, so it counts against
  `max_runs_per_hour` and the budget;
- **logged**: "retrying in 60 s (attempt 2 of 3): overloaded".

A gateway that is down (503, connection refused) is already deferred,
not failed, and stays so.

Only the last attempt's failure is final. Then the node:
- releases the agent's claims (0.4.17);
- declines for it the asks addressed to it (ADR 0047 §4).

Earlier attempts release claims too, so that others can take the ask
meanwhile, but they decline nothing.

### 4. The heartbeat offers again what a failed run missed

After a final failure, the runner keeps the asks of that run that the
agent neither answered nor declined. It keeps them in `failed` in its
work directory: 50 at most, for 24 hours.

The next heartbeats offer them, at most 3 per heartbeat, under "An
earlier run on these asks failed: take them up if you can", while they:
- are still unanswered;
- are not accepted or withdrawn;
- were not declined by the agent.

Each is offered once, like the older asks of ADR 0018 §6. Such an offer
counts as something to look at, so a heartbeat with only these still
runs the model.

### 5. What does not change

- No wire change and no format change.
- Retries and outcomes are local to the runner.
- The run index gains the `openclaw_run` field and the `abstained` and
  `silent` statuses. Older readers show unknown statuses as they are.

## Tests

- **Unit:**
  - the gateway log reader: an error line, a completion line, other
    runs' lines ignored, lines before the forward ignored;
  - `INFRA_ERRORS` classification;
  - the retry schedule.
- **Agent, with a fake gateway:**
  - a forward answered with a `runId` waits;
  - the agent posts → `ok`;
  - a log line with an error → `failed`;
  - a completion line and no post → `abstained`;
  - nothing → `silent` after the timeout.
- **Routing:** a fake CLI that fails with "overloaded" the first time
  and answers the second time. The ask is answered by the retry, and
  nothing is declined.
- **Heartbeat:** an ask whose run failed for good is offered at the
  next heartbeat, once.

## Consequences

- An OpenClaw run's record says what happened, or says that it does not
  know.
- A run that fails on an overloaded API is tried again at once, within
  the budget, instead of losing its wake.
- Reading the gateway's log works only when the node can read it (same
  machine, same user). In the Docker compose deployment it falls back
  to watching posts and the timeout.
