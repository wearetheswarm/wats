# 0008. Public bootstrap node and its packaging

Status: Provisional (2026-10-15).

Code: `crates/wats/src/bootstrap.rs`, `Dockerfile`, `deploy/k8s/`,
`.github/workflows/release.yml`. Runbook: `docs/deploy-bootstrap.md`.

## Context

New nodes need a first peer. Until now, peers exchanged tickets by hand.
The user operates a Kubernetes cluster (OVH) and the `wearetheswarm.ai`
zone (Gandi). Their bootstrap node must be reachable by name, keep a
stable identity, and must not be a trust anchor: a hijacked DNS record
must not let someone else pose as the router.

## Decision

### Bootstrap entries

- **Format.** `<endpoint-id>@<host>:<port>`.
- **Resolution.** The host is resolved with DNS at startup, so the IP can
  move. The endpoint id is pinned: Iroh authenticates every QUIC
  connection against the dialed key, so a host answering with another key
  is never connected to.
- **Built-in list.** `DEFAULT_BOOTSTRAP` is compiled in, and stays empty
  until the router's id is known (TODO in the code).
- **Overrides.**
  - `--bootstrap` / `WATS_BOOTSTRAP` add entries.
  - `--no-default-bootstrap` / `WATS_NO_DEFAULT_BOOTSTRAP` disable the
    built-in list.
  - `--loopback` never uses it.
- **Failure handling.** A node skips itself, and skips entries that fail to
  resolve.

### Router configuration

- **Port.** A fixed UDP port, **7842**: `--port` / `WATS_PORT` binds
  `0.0.0.0:<port>`, while IPv6 stays on a random port.
- **Public address.** `--external-addr` / `WATS_EXTERNAL_ADDR` advertises a
  load balancer address.

### Identity

- **Where it is read from.** `--secret-file` / `WATS_SECRET_FILE` reads the
  root secret from a file that must exist, instead of `<data dir>/secret.key`.
  For the router it is a Kubernetes Secret.
- **Where it is generated.** On the operator's machine, which is the
  backup. The image is distroless (no shell, no `tar`), so copying a secret
  out of a pod would be awkward. A missing secret file is an error, never a
  silently fresh identity.
- **Printing it.** `wats id` prints the endpoint id, the author id and the
  bootstrap entry. It does not need the node to be stopped.

### Shutdown

- **Signals.** The node shuts down cleanly on SIGTERM as well as Ctrl-C.
- **Registration.** Handlers are registered at startup, because a process
  running as PID 1 in a container ignores signals it has no handler for.

### Packaging

- **Image.** Multi-stage Dockerfile:
  - Rust 1.99 on Debian trixie for the build, distroless `cc-debian13:nonroot`
    at runtime, both pinned by digest;
  - user 65532, data on a `/data` volume, `wats node --quiet` in the
    foreground.
- **Release.** `release.yml` builds linux/amd64 only and pushes
  `ghcr.io/wearetheswarm/wats:<version>` and `:sha-<commit>` to a private
  GHCR package, with OCI labels.
  - It runs on `release: published` or by hand, and checks that the tag
    matches `Cargo.toml`.
  - There is no `latest` tag: deployments pin a version, ideally a digest.
  - No image is built on pull requests or pushes (CI budget).
- **Kubernetes.**
  - Namespace `wats` (restricted Pod Security Standard).
  - A one-replica StatefulSet with a PVC for data and the key from a
    Secret; non-root, read-only root filesystem, no capabilities, seccomp
    RuntimeDefault, resource requests and limits.
  - A LoadBalancer Service on UDP 7842 with `externalTrafficPolicy: Local`,
    a headless Service, and an image pull secret `ghcr-pull`.
  - A kustomization pins the image tag and digest.

## Consequences

- **Not a trust anchor.** Losing the router does not stop the swarm: nodes
  that know each other keep working, and the default list can hold several
  routers later. DNS hijacking can only cause a denial of service, never
  impersonation.
- **Key rotation.** Rotating the router key means a new release with a new
  pinned id. Old binaries then fail to bootstrap through it, though they
  keep any peers they already know.
- **Later options.** arm64 images on a native runner, probes once a health
  endpoint exists, and several routers in other regions.
