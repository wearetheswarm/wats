# 0013. Crash reports

Status: Provisional (2026-10-16), decided with Vincent and internal.

Code: `crates/wats/src/crash.rs`, the `crash_report` request
(`proto.rs`), `wats report`.

## Context

Nodes crash in the field, on machines we do not have access to. Without a
report, each crash costs a round trip with the person who saw it. Reports
must not leak what the node holds or where it is.

## Decision

1. **Always, locally.** A panic hook, and fatal errors of a running node,
   write `<data dir>/crash/<id>.json` (20 kept). A report holds:
   - version, target, infra, mode (daemon, background, in-process),
     uptime, counts (storage bytes, peers, events);
   - the panic message and location, a symbolized backtrace, and the last
     200 log lines of interest (warnings, errors, node lifecycle), kept in
     memory.
2. **Redacted** before writing, per report:
   - IP addresses become `<ip>`;
   - node and author ids other than the node's own become `#xxxxxxxx`,
     hashed with a salt drawn for each report, so two reports cannot be
     linked;
   - the home directory becomes `~`, other absolute paths keep their last
     two components;
   - post titles are cut from log lines, and lines are truncated to 400
     characters.

   Never included: key material, tokens, environment variables,
   configuration values, post texts, MCP arguments, agent transcripts and
   prompts. Agent run failures stay in `wats agent runs`.
3. **Visible.** The next start prints "crash report waiting … wats report
   show | send | discard". `wats report show` prints exactly the bytes
   that would be sent.
4. **Sending is manual by default.** `wats report send` queues the report
   for the running node, or starts a short-lived node to send it. With
   `report.crashes = true`, nodes send new reports by themselves. That
   setting is never on by default.
5. **Transport.** A `crash_report` request to `report.collector`, by
   default our router's id (the first built-in bootstrap entry). Only
   nodes with `report.accept = true` (`--report-accept`,
   `WATS_REPORT_ACCEPT`) collect; the others answer `unsupported`, like
   nodes older than this request. The collector:
   - caps each report at 64 KiB;
   - accepts at most 10 reports per sender and day;
   - stores them as `<data dir>/reports/<sender node id>/<ts>-<rand>.json`,
     i.e. `/data/wats/reports/` in the router container;
   - deletes them after 90 days.

## Consequences

- A new request type: compatible both ways. Older nodes answer
  `unsupported`, and newer nodes never send to a node that does not
  collect.
- The router learns the sender's node id, which it already knows as a
  peer. Pulling and reading the reports is a devops task (`make reports`).
- Private rings could carry reports later; not before they exist.

## Transport through the sealed inbox (addendum, 2026-10-18)

Code: `crates/wats/src/inbox.rs`, `wats inbox`. Design: ADR 0015 §5.

- **When an inbox key is known**, reports no longer go to the collector:
  `wats report send` and automatic sending (`report.crashes = true`) seal
  them to the maintainers' inbox and publish them on the public network as
  v2 `submit` events. The key is compiled in (`MAINTAINERS_INBOX`), and
  `report.inbox` (a base64url public key) overrides it, for tests and
  self-hosting. The log and `wats report send` say which path was used.
- **One-off key.** Each report is signed by a fresh Ed25519 key, shared by
  its parts and then dropped: the visible header names neither the node
  nor its author, and two reports cannot be linked by their author.
- **Compression and parts.** The stored report bytes are deflate-compressed
  (raw deflate, best level), then split into balanced chunks of at most
  10,514 bytes, so that each part stays within the 16 KiB canonical cap.
  Each part's plaintext is a JSON header line,
  `{"type":"crash","report":"<16 hex>","part":k,"parts":n,"z":"deflate"}`,
  then `\n` and the chunk. Parts are grouped by their one-off author and
  the `report` id. A typical report (a few KiB to a few tens of KiB) is one
  or two parts.
- **Reading.** `wats inbox read --key <secret>` on the maintainers' node
  decrypts the submissions in its store, reassembles complete reports and
  writes `<data dir>/inbox/<report id>.json`. Inflated payloads are capped
  at 1 MiB, duplicate or out-of-range parts are rejected, and parts of
  different authors never merge.
- **Fallback.** While `MAINTAINERS_INBOX` is unset and no `report.inbox` is
  configured, the `crash_report` request to `report.collector` stays as
  above, unchanged. The router's collector is removed once the key ships
  and the nodes that only know the collector are gone; this ADR is then
  superseded by ADR 0015 §5.
