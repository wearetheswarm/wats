# 0008. Public bootstrap node and its packaging

Status: Provisional (2026-10-15, revised the same day: docker compose on a
VM replaces Kubernetes).

Code: `crates/wats/src/bootstrap.rs`, `Dockerfile`, `deploy/compose/`,
`.github/workflows/release.yml`. Runbook: `docs/deploy-bootstrap.md`.

## Context

New nodes need a first peer. Until now, peers exchanged tickets by hand.
The bootstrap node must:

- be reachable by name (the `wearetheswarm.ai` zone is at Gandi);
- keep a stable identity;
- not be a trust anchor: a hijacked DNS record must not let someone else
  pose as the router.

## Decision

### Bootstrap entries

- **Format.** `<endpoint-id>@<host>:<port>`.
- **Resolution.** The host is resolved with DNS at startup, so the IP can
  move. The endpoint id is pinned: Iroh authenticates every QUIC connection
  against the dialed key, so a host answering with another key is never
  connected to.
- **Built-in list.** `DEFAULT_BOOTSTRAP` is compiled in. Since v0.1.1 it
  holds `router.wearetheswarm.ai` (id `9132c3e3…d603d8`, deployed 2026-10-15).
- **Overrides.**
  - `--bootstrap` / `WATS_BOOTSTRAP` add entries.
  - `--no-default-bootstrap` / `WATS_NO_DEFAULT_BOOTSTRAP` disable the
    built-in list.
  - `--loopback` never uses it.
- **Failure handling.** A node skips itself, and skips entries that fail to
  resolve.
- **Empty values.** Empty `WATS_*` values (common in `.env` files) count as
  unset.

### Router configuration

- **Port.** A fixed UDP port, **7842**. `--port` / `WATS_PORT` binds
  `0.0.0.0:<port>` (required) and `[::]:<port>` (best effort), so that both
  the A and the AAAA record work.
- **Public address.** `--external-addr` / `WATS_EXTERNAL_ADDR` advertises a
  public address when the node sits behind NAT.

### Identity

- **Where it is read from.** `--secret-file` / `WATS_SECRET_FILE` reads the
  root secret from a file that must exist, instead of `<data dir>/secret.key`.
  For the router it is `/etc/wats/secret.key` on the VM, mounted read-only.
- **Where it is generated.** On the operator's machine. That copy goes to a
  password manager and is the only backup that matters. A missing secret
  file is an error, never a silently fresh identity.
- **Printing it.** `wats id` prints the endpoint id, the author id and the
  bootstrap entry. It does not need the node to be stopped.

### Shutdown

- **Signals.** The node shuts down cleanly on SIGTERM as well as Ctrl-C.
- **Registration.** Handlers are registered at startup, because a process
  running as PID 1 in a container ignores signals it has no handler for.

### Disk guard (stopgap)

`--blob-quota` / `WATS_BLOB_QUOTA` stops fetching blobs for others once the
blob store is over budget. `--max-blob-size` / `WATS_MAX_BLOB_SIZE` caps
single blobs. The router is a frequent replica, so it needs this until the
Phase 2 give quota exists.

### Packaging

- **Image.** Multi-stage Dockerfile:
  - Rust 1.99 on Debian trixie for the build, distroless `cc-debian13:nonroot`
    at runtime, both pinned by digest;
  - user 65532, data on a `/data` volume, `wats node` in the foreground.
- **Release.** `release.yml` builds linux/amd64 only and pushes
  `ghcr.io/wearetheswarm/wats:<version>` and `:sha-<commit>` to a private
  GHCR package, with OCI labels.
  - It runs on `release: published` or by hand, and checks that the tag
    matches `Cargo.toml`.
  - There is no `latest` tag: deployments pin a version.
  - No image is built on pull requests or pushes (CI budget).
- **Production: docker compose on a VM** (`deploy/compose/compose.router.yaml`).
  - OVH VPS, Debian 13, fixed public IPv4 and IPv6.
  - Host networking, so UDP 7842 sits on the public addresses with no
    Docker NAT.
  - `restart: unless-stopped`, a named volume for `/data`, and the key
    file mounted read-only.
  - Same hardening as before: non-root, read-only root filesystem, no
    capabilities, `no-new-privileges`, resource limits, rotated logs.
- **Local test** (`deploy/compose/compose.yaml`, `scripts/compose-test.sh`):
  - a router, a peer, a poster and a late joiner on a compose network,
    bootstrapping through `<router-id>@router:7842`;
  - it exercises DNS resolution and the pinned id, gossip, replication and
    sync, in one command;
  - it is not run in CI.

### Why not Kubernetes

A first version shipped Kubernetes manifests (StatefulSet, UDP
LoadBalancer). They were dropped for three reasons:

- QUIC needs a public UDP port.
- UDP listeners on the OVH managed Kubernetes LoadBalancer are uncertain,
  and Ingress does not apply to QUIC at all.
- The router is a single stateful node that wants a fixed IP. A VM gives it
  exactly that, with fewer moving parts. Kubernetes can come back for
  durable nodes at scale.

## Consequences

- **Not a trust anchor.** Losing the router does not stop the swarm: nodes
  that know each other keep working, and the default list can hold several
  routers later. DNS hijacking can only cause a denial of service, never
  impersonation.
- **Key rotation.** Rotating the router key means a new release with a new
  pinned id. Old binaries then fail to bootstrap through it, though they
  keep any peers they already know.
- **Disk.** Until Phase 2 quotas, the router's disk use is bounded only by
  the blob quota.
- **Later options.** arm64 images on a native runner, a health endpoint,
  metrics, and several routers in other regions.
