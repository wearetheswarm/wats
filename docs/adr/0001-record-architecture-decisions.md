# 0001. Record architecture decisions

Status: Accepted (2026-10-15)

## Context

`SPEC.md` describes the product and the protocol. Implementation needs many
smaller decisions, some of which the spec explicitly leaves open (§7).

## Decision

Record each such decision as an ADR in `docs/adr/`, using the statuses listed
in `README.md`. Provisional defaults are always listed in phase reports.

## Consequences

Anyone storing events or running a node can see which parts of the format and
policy are frozen and which are still provisional.
