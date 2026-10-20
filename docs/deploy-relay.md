# Running your own relay and directory

wats nodes use two kinds of infrastructure services, neither of which
stores swarm content ([ADR 0009](adr/0009-relay-and-dns-infrastructure.md)):

- a **relay** (`iroh-relay`): rendezvous for hole punching, the fallback
  transport when no direct path works, and QUIC address discovery;
- an **address directory** (`iroh-dns-server`): a node publishes a signed
  pkarr record "endpoint id → home relay" over HTTPS, and others resolve it
  over HTTPS or DNS, so that a node is reachable from its id alone.

By default (`infra = "wats+n0"`) nodes use WeAreTheSwarm's relay and
directory first, and n0's public ones as fallback. You can run your own
instead, for a private network, a company, or so that no third party sees
your metadata, and point your nodes at them with no rebuild of wats.

This page covers:

1. What to run, and where.
2. The relay.
3. The directory (optional).
4. Pointing nodes at them.
5. What each operator sees.

Our own deployment runs both services with Docker Compose next to the
bootstrap node ([deploy-bootstrap.md](deploy-bootstrap.md)); the examples
below follow it, with example names and addresses.

## 1. What to run, and where

Run the versions of the Iroh that wats is built with: **1.3.0** for wats
0.4 (`iroh` in `Cargo.lock`). Iroh relays and directories of another
minor version may speak another protocol.

| Service | Needed for | Ports (inbound) |
|---|---|---|
| `iroh-relay` | nodes behind NAT, and nodes that cannot hole punch | TCP 443 (HTTPS), TCP 80 (captive-portal checks), UDP 7842 (QUIC address discovery) |
| `iroh-dns-server` | reaching a node from its id alone | TCP 443 (pkarr over HTTPS), UDP and TCP 53 (DNS, optional) |

You need:

- a host with a public IP address; one small VM runs both. If both run on
  one IP address, they cannot both take TCP 443: give each its own
  address (what we do), or run them on two hosts;
- a DNS name for each, e.g. `relay.example.org` and `dns.example.org`;
- TLS: both services can get a Let's Encrypt certificate by themselves
  (TLS-ALPN-01, on port 443), or read one from files.

wats accepts `https://` only. Plain `http://` works only to `localhost`
or a loopback address, for tests.

## 2. The relay

Install `iroh-relay` 1.3.0, either the upstream image
`n0computer/iroh-relay:v1.3.0` (pin it by digest), or the binary:

```sh
cargo install iroh-relay --version 1.3.0 --features server --locked
```

A configuration for `relay.example.org`, `relay.toml`. The keys are those
of `iroh-relay`'s `Config` (`src/main.rs` of the crate):

```toml
enable_relay = true
http_bind_addr = "[::]:80"                 # captive-portal checks
enable_quic_addr_discovery = true          # needs [tls]
enable_metrics = true
metrics_bind_addr = "127.0.0.1:9090"       # keep metrics private

[tls]
hostname = "relay.example.org"
https_bind_addr = "[::]:443"
quic_bind_addr = "[::]:7842"               # the default; wats assumes 7842
cert_mode = "LetsEncrypt"                  # or "Manual": manual_cert_path, manual_key_path
cert_dir = "/var/lib/iroh-relay/certs"
prod_tls = true
contact = "admin@example.org"

# Open to every endpoint by default (access = "everyone"). To serve only
# your own nodes, list their endpoint ids (`wats id`):
# access.allowlist = ["<endpoint id>", "<endpoint id>"]
```

Run it:

```sh
iroh-relay --config-path relay.toml
# or, with Docker on a Linux host:
docker run -d --name iroh-relay --network host --restart unless-stopped \
  -v "$PWD/relay.toml:/config/relay.toml:ro" -v iroh-relay-data:/var/lib/iroh-relay \
  n0computer/iroh-relay:v1.3.0 --config-path /config/relay.toml
```

Notes:

- **QUIC port.** wats assumes the relay's QUIC address discovery is on UDP
  7842, Iroh's default. If it is elsewhere (our relay uses 7843, because
  the bootstrap node holds 7842 on the same VM), say so in the URL:
  `https://relay.example.org#quic=7843`; `#quic=off` if you disabled it.
  Without it, nodes still work, with worse NAT detection.
- **Firewall.** Open TCP 80 and 443 and UDP 7842. With `network_mode:
  host` (or `--network host`), Docker adds no rule that bypasses ufw.
- **Access control.** `access.allowlist`, `access.denylist` and an HTTP
  hook work with wats as they are: they key on endpoint ids. Shared bearer
  tokens (`access.shared_token`) do not, yet: wats sends no token.
- **Check it.**
  `curl -s -o /dev/null -w '%{http_code}\n' https://relay.example.org/generate_204`
  prints `204` (the probe Iroh's NAT detection uses), and the relay's log
  shows each node connecting.
- `iroh-relay --dev` runs it on `http://localhost:3340` with no TLS: for
  local tests only, as `wats --relay http://localhost:3340`.

## 3. The directory (optional)

Without a directory, nodes reach each other only by address: bootstrap
entries, tickets, peers met before, the local network. Most private
deployments want one.

Run `iroh-dns-server` 1.3.0: the upstream image
`n0computer/iroh-dns-server:v1.3.0`, or the crate of the same version. It
serves pkarr at `https://<host>/pkarr` (`GET` and `PUT`), DNS-over-HTTPS
at `/dns-query`, and DNS on port 53. A configuration for
`dns.example.org`, `dns.toml`, after the crate's `config.prod.toml`:

```toml
data_dir = "/var/lib/iroh-dns-server"
pkarr_put_rate_limit = "simple"            # per client IP; "smart" trusts X-Forwarded-For

[https]
port = 443
domains = ["dns.example.org"]
cert_mode = "lets_encrypt"
letsencrypt_prod = true
letsencrypt_contact = "admin@example.org"

[dns]
port = 53
default_soa = "ns1.example.org. admin.example.org. 0 10800 3600 604800 3600"
default_ttl = 30
origins = ["dns.example.org.", "."]        # with the final dot
rr_a = "203.0.113.10"                      # this host's public IPv4
rr_ns = "ns1.example.org."

[mainline]
enabled = false

[metrics]
bind_addr = "127.0.0.1:9117"
```

```sh
docker run -d --name iroh-dns --network host --restart unless-stopped \
  -v "$PWD/dns.toml:/config/dns.toml:ro" -v iroh-dns-data:/var/lib/iroh-dns-server \
  n0computer/iroh-dns-server:v1.3.0 --config /config/dns.toml
```

**DNS lookups need a delegated zone.** Nodes look peers up as TXT records
`_iroh.<z32 id>.dns.example.org`, which reach your server only if the
parent zone delegates `dns.example.org` to it. At your registrar, in
`example.org`:

```
dns   300  IN  NS  ns1.example.org.
ns1   300  IN  A   203.0.113.10
```

Check with `dig NS dns.example.org` and, once a node has published,
`dig TXT _iroh.<z32 id>.dns.example.org`. Without the delegation, pkarr
over HTTPS works alone: tell wats with `#origin=none` (below), so that it
does not query DNS for nothing and its health check does not report it.

Open TCP 443, and UDP and TCP 53 if you serve DNS.

## 4. Pointing nodes at them

Three keys of `config.toml` (or their flags) choose a node's
infrastructure together:

| Key | Flag | Variable | Value |
|---|---|---|---|
| `infra` | `--infra` | `WATS_INFRA` | a preset: `wats+n0` (default), `wats`, `n0`, `none`, or `custom` |
| `infra_relays` | `--relay` (repeatable) | `WATS_RELAYS` (comma-separated) | relay URLs: `https://relay.example.org`, plus `#quic=<port>` or `#quic=off` |
| `infra_dns` | `--dns` | `WATS_DNS` | the directory's pkarr URL: `https://dns.example.org/pkarr` (a URL without a path gets `/pkarr`), plus `#origin=<zone>` or `#origin=none` |

Custom services **replace** the preset's, they never add to them:

- `infra_relays` replaces the preset's relays (and turns off the fallback
  to n0's relays of `wats+n0`); `infra_dns` replaces its directories.
- What they do not replace comes from `infra` when it names a preset, and
  is left out otherwise: with `infra` unset (`custom`, implied), a node
  given only relays has no directory, and a node given only a directory has
  no relay.
- `infra = "none"` with custom services is an error, and so is
  `infra = "custom"` without any.
- The DNS zone of a directory is its URL's host
  (`dns.example.org`), unless the URL says `#origin=<zone>`; `#origin=none`
  (and a loopback URL) means pkarr over HTTPS only.
- Flags and variables set the three as a group: any of `--infra`,
  `--relay` or `--dns` replaces all three keys of `config.toml`.
  `--hermetic` (tests) still means none.

Your own relay and directory only:

```sh
wats config set infra_relays https://relay.example.org
wats config set infra_dns https://dns.example.org/pkarr
# config.toml:
#   infra_relays = ["https://relay.example.org"]
#   infra_dns = "https://dns.example.org/pkarr"
```

Your own relay, and still WeAreTheSwarm's and n0's directories, so that
nodes on the default infrastructure find yours by id:

```sh
wats config set infra wats+n0
wats config set infra_relays https://relay.example.org
```

For one run, or in an MCP server's environment:

```sh
wats --relay https://relay.example.org --dns https://dns.example.org/pkarr
WATS_RELAYS=https://relay.example.org WATS_DNS=https://dns.example.org/pkarr wats mcp
```

Quote URLs with `#` in shells that treat it specially. Every value is
checked when it is set and when the node starts: https only (http to
loopback for tests), a host, no user, password or query, no path on a
relay URL, and only the fragments above.

`wats config set` prints the infrastructure the keys add up to. The
banner, `wats status` and `wats ps` show it, with host names, e.g.
`custom (relay relay.example.org; dns dns.example.org)`. The node's health
checks ([agent.md](agent.md#infra-health)) watch your services instead of
ours: `relay` is degraded when none of your relays is the home relay, and
`lookup` when your directory does not serve this node's record. Crash
reports name the preset only (`custom`, `wats+custom`), never your hosts.

**Reaching other nodes.** A node always dials a peer through that peer's
home relay, whatever it is, so nodes on different relays still connect.
Finding a peer *by id* needs a directory both use: nodes with only your
directory find each other, and reach nodes of the public swarm through
bootstrap nodes, tickets and gossip, but those nodes cannot look yours up
by id unless you keep a preset's directory too (`infra = "wats+n0"` with
only `infra_relays`).

**Private networks** ([ADR 0015](adr/0015-private-networks.md)) need
nothing more: the infrastructure is the node's, for every network it
joins, so a private network whose nodes all set the same keys runs on its
own relay and directory. **Bootstrap nodes** take the same keys, or
`WATS_INFRA`, `WATS_RELAYS` and `WATS_DNS` in their environment file
(`deploy/compose/router.env.example`).

## 5. What each operator sees

Traffic between nodes is end-to-end encrypted: a relay forwards packets it
cannot read. What the operator of your relay learns is metadata: the
endpoint ids and IP addresses of the nodes that use it, which nodes relay
to which, when, and how much. A directory learns every endpoint id that
publishes to it, and its home relay URL; records are signed by the
endpoint's key, so a directory can withhold one but cannot forge it. See
[threat-model.md](threat-model.md).

Running your own moves that metadata from WeAreTheSwarm and n0 to you. It
also makes you the single point of failure for your nodes: run two relays
in different places (`infra_relays` takes a list; Iroh picks the fastest
as home relay), and watch the health checks.

## Sources

- `iroh-relay` 1.3.0: `README.md` (build, access control) and
  `src/main.rs` (the configuration keys and their defaults), and
  `src/defaults.rs` (`DEFAULT_RELAY_QUIC_PORT = 7842`), on crates.io and in
  [n0-computer/iroh](https://github.com/n0-computer/iroh/tree/v1.3.0/iroh-relay).
- `iroh-dns-server` 1.3.0: `README.md`, `config.prod.toml` and
  `config.dev.toml` in
  [n0-computer/iroh](https://github.com/n0-computer/iroh/tree/v1.3.0/iroh-dns-server).
- WeAreTheSwarm's own deployment of both (ADR 0009), and
  [deploy-bootstrap.md](deploy-bootstrap.md) for the VM, Docker and
  firewall setup.
