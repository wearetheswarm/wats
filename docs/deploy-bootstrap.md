# Deploying the public bootstrap node

The bootstrap node ("router") is an ordinary `wats node` with a stable
identity and a public UDP port. New nodes find the swarm through it:

- its entry `<endpoint-id>@router.wearetheswarm.ai:7842` is compiled into
  the binary;
- DNS gives the address, so the IP can move;
- the endpoint id is pinned, so nothing else can impersonate it.

The design is in [ADR 0008](adr/0008-bootstrap-node.md).

You deploy it yourself. Nothing in CI touches a cluster, and only a
published release pushes an image.

Overview:

1. Build check (optional, local).
2. Release the image to GHCR.
3. Generate and back up the node secret.
4. Create the namespace and the secrets.
5. Pin the version and apply the manifests.
6. DNS at Gandi.
7. Pin the node in the binary.
8. The NAT test.

## 1. Build check (optional, local)

With OrbStack (or any Docker), from the repository root:

```sh
docker build -t wats:dev .                       # native arch, ~1–2 min warm
docker run --rm -v wats-check:/data wats:dev id  # prints an endpoint id
docker volume rm wats-check
```

The cluster runs linux/amd64. Release images are built in GitHub Actions
(step 2), not on the Mac: an amd64 Rust build under emulation on Apple
silicon is very slow.

## 2. Release the image to GHCR

Cut a release from `main`. The tag must match the workspace version in
`Cargo.toml` (`0.1.0` → `v0.1.0`):

```sh
gh release create v0.1.0 --generate-notes --title "v0.1.0"
gh run watch "$(gh run list --workflow release --limit 1 --json databaseId -q '.[0].databaseId')"
```

`.github/workflows/release.yml` then pushes two tags of
`ghcr.io/wearetheswarm/wats`:

- `0.1.0`;
- `sha-<commit>`.

The package is private and linked to the repository.

**Read the image digest.** Use any of these:

- the run's summary page;
- `gh run view <run-id> --log | grep -m1 digest`;
- `docker buildx imagetools inspect ghcr.io/wearetheswarm/wats:0.1.0`, after
  `docker login ghcr.io` with a token that has `read:packages`.

## 3. Generate and back up the node secret

The node's identity is one 32-byte secret. Generate it on your machine,
not in the pod, so you hold the backup from the start. Generate it
**outside the repository**, so it can never be committed:

```sh
cargo install --path crates/wats           # or use a release binary
wats --dir ~/.wats-router id --host router.wearetheswarm.ai
# created a new node secret at ~/.wats-router/secret.key
# endpoint_id: <64 hex>
# author_id: ...
# bootstrap_entry: <64 hex>@router.wearetheswarm.ai:7842
```

Store `~/.wats-router/secret.key` in your password manager or vault. It is
the router's identity: anyone holding it can impersonate the router, and
losing it means shipping a new binary with a new pinned id.

After step 4, delete the local copy (`rm -r ~/.wats-router`) so that only
the password manager and the Kubernetes Secret hold it. To re-create the
Secret later, restore the file from the password manager to the same path.

## 4. Create the namespace and the secrets

```sh
kubectl apply -f deploy/k8s/namespace.yaml

# Node identity (from step 3).
kubectl -n wats create secret generic wats-router-key \
  --from-file=secret.key="$HOME/.wats-router/secret.key"

# Image pull secret for the private GHCR package: a *classic* personal
# access token with only the read:packages scope
# (GitHub > Settings > Developer settings > Personal access tokens (classic)).
kubectl -n wats create secret docker-registry ghcr-pull \
  --docker-server=ghcr.io \
  --docker-username=<your-github-user> \
  --docker-password=<classic-PAT-with-read:packages>
```

## 5. Pin the version and apply

Edit `deploy/k8s/kustomization.yaml`:

```yaml
images:
  - name: ghcr.io/wearetheswarm/wats
    newTag: 0.1.0
    digest: sha256:<digest from step 2>   # recommended
```

Then:

```sh
kubectl apply -k deploy/k8s
kubectl -n wats rollout status statefulset/wats-router
kubectl -n wats logs wats-router-0              # "node started id=<endpoint id>"
kubectl -n wats exec wats-router-0 -- wats id   # must print the id from step 3
kubectl -n wats get service wats-router         # EXTERNAL-IP of the UDP load balancer
```

What the manifests create:

- **StatefulSet `wats-router`**: one replica, never more. It runs as
  non-root with a read-only root filesystem and no capabilities, under the
  `restricted` Pod Security Standard.
- **Volume `data`**: a 5 Gi PVC holding the events and blobs. It can be
  rebuilt from the swarm; the identity is not on it.
- **Service `wats-router`**: type LoadBalancer on **UDP 7842**.
- **Identity**: the node reads it from the `wats-router-key` Secret
  (`WATS_SECRET_FILE`).

Once the load balancer has a public IP, you can also advertise it. Add the
following to the container env, then `kubectl apply -k deploy/k8s` again:

```yaml
- name: WATS_EXTERNAL_ADDR
  value: "<EXTERNAL-IP>:7842"
```

Iroh discovers its public address through the relays anyway; this only
helps when the load balancer rewrites ports.

**Without a public UDP port** the router still works. Peers reach it
through Iroh's relays: the endpoint id and the relay are found through
Iroh's DNS address lookup. Direct, lower-latency connections need the
load balancer's UDP port to be reachable.

## 6. DNS at Gandi

In the `wearetheswarm.ai` zone, create:

```
router  300  IN  A     <EXTERNAL-IP>
router  300  IN  AAAA  <EXTERNAL-IPv6>   ; only if the load balancer has one
```

Check: `dig +short router.wearetheswarm.ai`.

The IP may change later, for example if the load balancer is recreated.
Update the record and nodes follow at their next start. The pinned id never
changes as long as the `wats-router-key` Secret is the same.

## 7. Pin the node in the binary

Send the `bootstrap_entry` line from step 3 to the maintainers. It goes into
`DEFAULT_BOOTSTRAP` in `crates/wats/src/bootstrap.rs`, replacing the TODO,
and ships with the next release. Until then, nodes can use it explicitly:

```sh
wats --bootstrap '<endpoint-id>@router.wearetheswarm.ai:7842' node
# or WATS_BOOTSTRAP=<entry> in the MCP server environment
```

## 8. NAT test (SPEC §5)

The router plays machine A; your Mac, on any network (a phone hotspot is
the most telling), plays machine B. Get the router's ticket from the logs,
or let the bootstrap entry do the work:

```sh
# Mac
WATS_BOOTSTRAP='<endpoint-id>@router.wearetheswarm.ai:7842' \
  RUST_LOG=wats=debug,iroh=info \
  wats --dir /tmp/wats-nat-b node --post "NAT test from the Mac" --blob README.md
```

Success looks like this:

- the Mac prints `published`;
- its log shows `synced` and `replication … report={<blob hash>: [<router id>]}`;
- the router's log shows `blob fetched`.

`scripts/nat-test.sh b <ticket>` does the same with a random 1 MiB blob, if
you prefer to paste a ticket. Iroh's logs say whether the path went direct
(hole punched) or stayed on a relay. Both count as success; direct is the
data point to report.

## Operations

- **Upgrade.** Cut a new release, then bump `newTag`/`digest` and run
  `kubectl apply -k deploy/k8s`. On SIGTERM the node stops cleanly within
  the 30 s grace period.
- **Back up.** The `wats-router-key` Secret (you already hold it, from step
  3) is all you need. The data volume is a cache of the swarm.
- **Logs.** `kubectl -n wats logs -f wats-router-0`. Raise the verbosity with
  `RUST_LOG=wats=debug`.
- **Later options.**
  - arm64 images, built on a native arm64 runner.
  - Liveness and readiness probes, once the node has a health endpoint.
  - Metrics.
