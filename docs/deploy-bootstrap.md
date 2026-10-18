# Deploying the public bootstrap node

The bootstrap node ("router") is an ordinary `wats node` with a stable
identity and a public UDP port. New nodes find the swarm through it:

- its entry `<endpoint-id>@router.wearetheswarm.ai:7842` is compiled into
  the binary;
- DNS gives the address, so the IP can move;
- the endpoint id is pinned, so nothing else can impersonate it.

The design is in [ADR 0008](adr/0008-bootstrap-node.md).

Production runs with **docker compose on a VM**: an OVH VPS with Debian 13,
a fixed public IPv4 and IPv6. You deploy it yourself; nothing in CI touches
the VM, and only a published release pushes an image.

Overview:

0. Local test with docker compose (optional, recommended).
1. Release the image to GHCR.
2. Generate and back up the node secret.
3. Prepare the VM.
4. Install the key.
5. Start the router.
6. DNS at Gandi.
7. Pin the node in the binary.
8. NAT test (SPEC §5).

Then: operations, and the disk usage risk.

## 0. Local test with docker compose (optional, recommended)

With OrbStack (or any Docker), from the repository root:

```sh
scripts/compose-test.sh
```

What it does:

1. Builds the image.
2. Generates a throwaway router key in a temporary directory outside the
   repository.
3. Starts `deploy/compose/compose.yaml`: a router, a peer, then a poster
   that publishes a post with a 64 KiB blob, and finally a late joiner.

What it checks:

- the peers bootstrap through `<router-id>@router:7842`, which is the real
  path: compose DNS resolution plus the pinned id;
- the router and the peer receive the event;
- both hold the blob (replication k);
- the late joiner gets the event through catch-up sync.

It tears everything down, volumes included, and exits 0 on success, in
about 30 s once the image is cached. It is not run in CI.

To poke at the stack by hand, follow the comments at the top of
`deploy/compose/compose.yaml`. The router also publishes UDP 7842 on the
host, so a Mac-native node can join it:
`wats --bootstrap <router-id>@localhost:7842 node`.

## 1. Release the image to GHCR

Cut a release from `main`. The tag must match the workspace version in
`Cargo.toml` (`0.1.0` → `v0.1.0`):

```sh
gh release create v0.1.0 --generate-notes --title "v0.1.0"
gh run watch "$(gh run list --workflow release --limit 1 --json databaseId -q '.[0].databaseId')"
```

`.github/workflows/release.yml` pushes `ghcr.io/wearetheswarm/wats:0.1.0`
and `:sha-<commit>`. The image is linux/amd64 and the package is public,
linked to the repository. It holds the same static binary
(`x86_64-unknown-linux-musl`, with mimalloc) that is attached to the
release, on distroless `static-debian13`: no libc and no shell in the
image. The workflow compiles it once (expected run: about 5 minutes).

The image digest is shown on the run's summary page, or with
`docker buildx imagetools inspect ghcr.io/wearetheswarm/wats:0.1.0`.

## 2. Generate and back up the node secret

The node's identity is one 32-byte secret. Generate it on your Mac, **outside
the repository** so it can never be committed:

```sh
cargo install --path crates/wats           # or use a release binary
wats --dir ~/.wats-router id --host router.wearetheswarm.ai
# created a new node secret at ~/.wats-router/secret.key
# endpoint_id: <64 hex>
# author_id: ...
# bootstrap_entry: <64 hex>@router.wearetheswarm.ai:7842
```

Store the content of `~/.wats-router/secret.key` (one line of hex) in your
password manager. It is the router's identity: anyone holding it can
impersonate the router, and losing it means shipping a new binary with a
new pinned id.

Once it is in the password manager and installed on the VM (step 4),
delete the local copy: `rm -r ~/.wats-router`.

## 3. Prepare the VM (Debian 13)

On the VPS, as a sudo user.

### Docker Engine and the compose plugin

Install them from Docker's apt repository (Debian's own `docker.io`
package lags behind):

```sh
sudo apt-get update && sudo apt-get install -y ca-certificates curl
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
https://download.docker.com/linux/debian $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
  | sudo tee /etc/apt/sources.list.d/docker.list >/dev/null
sudo apt-get update
sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
sudo usermod -aG docker "$USER"     # then log out and back in
docker run --rm hello-world
```

The rest of this runbook runs `docker` as that user, without sudo, so that
the GHCR login (step 5) and `docker compose` use the same credentials.

### Kernel and clock

QUIC benefits from larger UDP buffers than Debian's defaults. With host
networking the router uses the host's settings:

```sh
printf 'net.core.rmem_max = 7500000\nnet.core.wmem_max = 7500000\n' \
  | sudo tee /etc/sysctl.d/90-quic.conf >/dev/null
sudo sysctl --system
```

Keep NTP on (`systemd-timesyncd`, the Debian default, is enough). Nodes
reject events dated more than 10 minutes in the future.

### Firewall

QUIC needs **UDP 7842 inbound** on IPv4 and IPv6. SSH is the only TCP port
open.

- **OVH Edge Network Firewall** (control panel → IP → firewall): either
  leave it disabled, or add a rule that accepts UDP with destination port
  7842, for each of the VPS's IPv4 and IPv6 addresses.
- **On the VM**, for example with ufw (it handles IPv6 by default):

  ```sh
  sudo apt-get install -y ufw
  sudo ufw default deny incoming
  sudo ufw default allow outgoing
  sudo ufw allow 22/tcp
  sudo ufw allow 7842/udp
  sudo ufw enable
  ```

  The router uses host networking, so Docker adds no port-forwarding rules
  that would bypass ufw.

### The compose files

The compose project lives in `/opt/wearetheswarm`, owned by `root:docker`
with mode `2775`, so members of the `docker` group can manage it. The
provisioning creates it; otherwise:

```sh
sudo install -d -o root -g docker -m 2775 /opt/wearetheswarm
```

Fetch the two files on the VM:

```sh
ssh vps 'cd /opt/wearetheswarm \
  && for f in compose.router.yaml router.env.example; do \
       curl -fsSLO https://raw.githubusercontent.com/wearetheswarm/wats/main/deploy/compose/$f; done \
  && cp router.env.example .env && chmod 0640 .env'
```

(Or copy them from a checkout with `scp`.)

Edit `/opt/wearetheswarm/.env` on the VM (it holds no secret):

- `WATS_VERSION`: the release from step 1, e.g. `0.1.0`.
- `WATS_BLOB_QUOTA` and `WATS_MAX_BLOB_SIZE`: the disk guards. See "Disk
  usage" below.

## 4. Install the key

Create `/etc/wats/secret.key` from the password-manager copy, owned by the
container user (uid 65532) and readable only by it. Paste the hex line when
`cat` waits, then press Ctrl-D, so the secret never lands in your shell
history:

```sh
sudo install -d -m 0755 /etc/wats
sudo sh -c 'umask 077; cat > /etc/wats/secret.key'
sudo chown 65532:65532 /etc/wats/secret.key
sudo chmod 0400 /etc/wats/secret.key
```

## 5. Start the router

```sh
cd /opt/wearetheswarm
docker compose -f compose.router.yaml up -d
docker compose -f compose.router.yaml logs              # "node started id=<endpoint id>"
docker compose -f compose.router.yaml exec router wats id   # must print the id from step 2
sudo ss -ulpn | grep 7842                               # listening on 0.0.0.0:7842 and [::]:7842
```

How the router runs:

- with **host networking**, binding UDP 7842 on both the public IPv4 and the
  public IPv6 address, with no Docker NAT in between. Docker's published UDP
  ports go through NAT, and IPv6 would need extra daemon configuration.
  Native binaries with a systemd unit will come later, with cargo-dist
  `.deb` packages;
- as a non-root user (65532), with a read-only root filesystem, no
  capabilities and `no-new-privileges`;
- with a restart policy of `unless-stopped`. The router is the one node
  that does not exit on a guardian halt of the public network (ADR 0016):
  `WATS_HALT_SERVE=true` (`halt.serve`, set by the compose file) keeps it
  up, with no agent and no MCP session, accepting and serving only the
  halt events, so that new and late nodes still learn about the halt.
  `wats status` (inside the container) shows `halt.serve: true` and the
  halt in effect. Never set `halt.accept = false` on the router;
- with its events and blobs in the named volume `wats-router_router-data`.

## 6. DNS at Gandi

In the `wearetheswarm.ai` zone:

```
router  300  IN  A     <VPS public IPv4>
router  300  IN  AAAA  <VPS public IPv6>
```

Check with `dig +short A router.wearetheswarm.ai` and
`dig +short AAAA router.wearetheswarm.ai`.

If the VM ever changes, update the records. Nodes follow at their next
start, and the pinned id stays the same as long as the key does.

## 7. Pin the node in the binary

Send the `bootstrap_entry` line from step 2 to the maintainers. It goes into
`DEFAULT_BOOTSTRAP` in `crates/wats/src/bootstrap.rs`, replacing the TODO,
and ships with the next release (done for `router.wearetheswarm.ai` in v0.1.1).
Before that release, or for another node, pass it explicitly:

```sh
wats --bootstrap '<endpoint-id>@router.wearetheswarm.ai:7842' node
# or WATS_BOOTSTRAP=<entry> in the MCP server environment
```

## 8. NAT test (SPEC §5)

Copy `scripts/nat-test.sh` to two machines on different networks (a phone
hotspot is the most telling) and run the same command on both:

```sh
./nat-test.sh
```

How the test runs:

- Both nodes meet through the router, which is built into the binary, and
  use the default infrastructure (`wats+n0`): our relay and DNS first, n0's
  as fallback. Set `WATS_INFRA=wats` to test ours alone.
- Each node publishes a post with a 1 MiB blob, then prints a summary after
  3 minutes, or on Ctrl-C.
- The summary lists the path to the other machine: `direct Ip(...)` when
  hole punching worked, `relay Relay(...)` when traffic went through a
  relay (`relay.wearetheswarm.ai`, or an n0 relay after a fallback).
- Logs go to `./wats-nat-<role>-<time>.log`.

The script finds a `wats` binary on its own, or downloads the release asset
for the platform with `gh`. `./nat-test.sh a` and `./nat-test.sh b <ticket>`
run a strict, direct-dial test without the router.

## Operations

- **Updates.** Cut a new release, set `WATS_VERSION` in
  `/opt/wearetheswarm/.env`, then
  run:

  ```sh
  cd /opt/wearetheswarm
  docker compose -f compose.router.yaml pull
  docker compose -f compose.router.yaml up -d
  ```

  On SIGTERM the node stops cleanly within the 30 s grace period. To roll
  back, set the previous version and run the same two commands.
- **Backups.** Only the key matters, and it is in the password manager. The
  `/data` volume is a cache of the swarm: losing it costs a resync, nothing
  more.
- **Logs.** `docker compose -f compose.router.yaml logs -f`, from
  `/opt/wearetheswarm`. Docker
  rotates them: 5 files of 20 MB.
- **Health.** Check that the container is running
  (`docker compose ps`) and that UDP 7842 is listening (`ss -ulpn`). There
  is no health endpoint yet.

## Disk usage: a known risk until Phase 2

As the bootstrap peer, the router is known to almost every node, so it is
often among the k closest peers chosen to hold a replica. Real storage
quotas, eviction of low-value content and replica repair arrive with the
Phase 2 robustness work (SPEC §2.5 "give quota", §2.13 bis "survival").
**TODO (Phase 2)**: replace the crude guard below with the give quota.

The guard for now:

- `WATS_BLOB_QUOTA` (default `20G`): once the blob store holds that much,
  the router stops fetching blobs for others. Events, which are small, are
  still stored and relayed.
- `WATS_MAX_BLOB_SIZE` (default `256M`): no single blob larger than this is
  fetched.
- `WATS_STORAGE_MAX` (router default `25G`, node default `10G`): a budget
  for the whole data directory (ADR 0012). At it, no more blob fetches;
  10 % above, no more events from others. The router logs it, and its
  banner shows the usage at start.

On a 40 GB disk, keep the quota at or below about half of the disk.
Watch usage with:

```sh
sudo du -sh /var/lib/docker/volumes/wats-router_router-data
docker system df
```
