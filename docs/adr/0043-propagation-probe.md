# 0043. A propagation probe: how fast, and by which path, each node hears

Status: Proposed (2026-10-24), for discussion. No code yet.

Source: Vincent and the internal session, after the seed island of 0.4.10
(ADR 0025 §5). Partitions and silent links are classic failures of
distributed systems, so they can come back in another form. The
self-healing of 0.4.11 covers any cause. What is missing is seeing which
node is cut off, and since when.

## Context

0.4.12 adds per-node counters to `status`: events by gossip versus late by
anti-entropy, and link resets. They say that a node lags, not how far
behind the rest it is, nor whether the lag is on the way in or out.
devops's crude version compares each node's newest post time, and saw
seeds 1 and 3 thirteen minutes behind.

## Decision (proposed)

### 1. The probe event

- **Who sends it**: bootstrap nodes only (the router, and nodes with
  `probe.send = true`). Every `probe.every`, 5 minutes by default, each
  sends one small event.
- **Its kind**: an extension kind, `x.wats.probe` (ADR 0010: unknown
  extension kinds are stored and relayed by every node, so nodes before
  this one carry it too). It has an empty text, no title, no blobs, and
  `data` = `{"seq": n}`. It is signed like any event, with the sender's
  `ts`.
- **Kept out of sight**: `search`, `browse`, `wait`, runners and
  notifications skip the kind; `read` shows it if asked by id.
- **Short-lived**: a node drops probes older than 1 hour (`probe.ttl`),
  and keeps only the last 3 per sender. Rotation never counts them as
  history, and the fill and catch-up do not fetch them.

### 2. What each node records

For each probe it stores, a node notes when it arrived (its own clock)
and how: gossip (and from which neighbor), or a pull (anti-entropy,
catch-up). It keeps the last one per sender, in memory only.

`status` then shows, per sender:

- "router's probe: seen 40 s after it was sent (gossip, from 9f5d…)";
- or "8 min late (anti-entropy)";
- or "none for 15 min: no path from the router".

The delay is the receiver's arrival time minus the probe's `ts`. Clock
skew is bounded by `MAX_CLOCK_SKEW_SECS`, so the line says "about"; a
delay under the skew reads as "on time".

### 3. What it tells

- A node that sees probes late, through anti-entropy, has dead links in;
  its counters say whether gossip comes at all.
- A node whose own posts reach the router late has dead links out. The
  router's `status` shows the node's probe-like signal: its announces
  (or, later, a reply probe) and their delay.
- In `wats top` (ADR 0042), the network view colours each node by its
  probe delay: an island shows as one colour.

### 4. Cost

One event of about 300 bytes per bootstrap node every 5 minutes, which is
about 86 KB a day per sender, relayed once by every node. It is stored
only briefly.

## Questions for Vincent

- **Q1.** Probes from bootstrap nodes only, or also from every seed?
  Recommended: bootstrap nodes and seeds (`probe.send`), not end users.
- **Q2.** A reply probe, where each node answers the router's probe with
  its own, so the router sees the delay out from every node? It is more
  traffic (N events per round). Recommended: no for now. The router reads
  the delay from each node's announces and posts.
- **Q3.** Should the sender's `seq` let a node count lost probes (a gap in
  `seq`) as well as late ones? Recommended: yes, it is free.

## Alternatives

- **Timestamps of real posts** (devops's `make overview`): free, but quiet
  hours say nothing, and the posts' paths are unknown.
- **A dedicated ping protocol over wats's ALPN**: it measures links, not
  gossip propagation, which is what failed.
