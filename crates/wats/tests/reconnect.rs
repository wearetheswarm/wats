//! Chaos tests of reconnection (src/reconnect.rs): a node that loses its
//! peers, or never resolves its bootstrap node, dials again until it
//! succeeds, then catches up on what was posted meanwhile. Hermetic:
//! loopback only (direct, or through a relay server on 127.0.0.1), DNS
//! failures injected.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use iroh::Watcher as _;
use wats::bootstrap::{BootstrapEntry, Resolver};
use wats::event::{Draft, Event, Kind};
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};
use wats::reconnect::Backoff;

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn config(dir: &std::path::Path) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    // Fast enough for a test, same shape as the default.
    config.reconnect = Backoff {
        first: Duration::from_millis(200),
        max: Duration::from_secs(1),
        start_grace: Duration::from_millis(500),
    };
    config
}

async fn eventually<F, T>(what: &str, f: F) -> T
where
    F: FnMut() -> Option<T>,
{
    eventually_within(what, Duration::from_secs(60), f).await
}

async fn eventually_within<F, T>(what: &str, limit: Duration, mut f: F) -> T
where
    F: FnMut() -> Option<T>,
{
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if let Some(value) = f() {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn connectivity(node: &Node) -> serde_json::Value {
    node.health_json()["connectivity"].clone()
}

async fn post(node: &Arc<Node>, title: &str) -> anyhow::Result<Event> {
    node.publish(
        Draft {
            net: PUBLIC_NET.into(),
            kind: Some(Kind::Ask),
            title: Some(title.into()),
            text: format!("{title}: posted while the other node was cut off."),
            ..Draft::default()
        },
        &[],
    )
    .await
}

/// A `wats node` process: the peer that dies.
struct Peer {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<serde_json::Value>,
}

impl Peer {
    fn spawn(dir: &std::path::Path, port: u16, extra: &[&str]) -> Peer {
        use std::io::BufRead;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
            .args(["--loopback", "--pow-bits=0", "--min-pow-bits=0", "--port"])
            .arg(port.to_string())
            .arg("--dir")
            .arg(dir)
            .arg("node")
            .args(extra)
            .env("WATS_HERMETIC", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("spawn wats");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if let Ok(value) = serde_json::from_str(&line)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        Peer { child, lines }
    }

    fn expect(&self, kind: &str) -> serde_json::Value {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("timed out waiting for {kind}"));
            if line["type"] == kind {
                return line;
            }
            assert_ne!(line["type"], "error", "{line}");
        }
    }

    /// kill -9: no goodbye, no peer cache saved.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The peer is killed; the node says it is offline and keeps dialing. The
/// peer comes back on the same address with a post made during the
/// outage: the node reconnects by itself and syncs it.
#[tokio::test(flavor = "multi_thread")]
async fn reconnects_to_a_killed_peer_and_catches_up() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let port = free_udp_port();
    let peer_dir = dir.path().join("peer");

    let peer = Peer::spawn(&peer_dir, port, &[]);
    let ticket: iroh_tickets::endpoint::EndpointTicket =
        peer.expect("ready")["ticket"].as_str().unwrap().parse()?;
    let peer_id = ticket.endpoint_addr().id;
    let mut node_config = config(&dir.path().join("node"));
    node_config.peers.push(iroh::EndpointAddr::from_parts(
        peer_id,
        [iroh::TransportAddr::Ip(
            (std::net::Ipv4Addr::LOCALHOST, port).into(),
        )],
    ));
    let node = Node::start(node_config).await?;
    node.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await?;
    eventually("online", || {
        (connectivity(&node)["state"] == "online").then_some(())
    })
    .await;

    peer.kill();
    eventually("offline and retrying", || {
        let c = connectivity(&node);
        (c["state"] == "offline" && c["attempt"].as_u64() >= Some(2)).then_some(())
    })
    .await;

    // While it is down, a post lands in its store (as if it got it from
    // someone else): the node can only get it by catching up.
    let missed = Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Ask),
        title: Some("missed during the outage".into()),
        text: "Posted while the node was cut off.".into(),
        ..Draft::default()
    }
    .sign(
        &ed25519_dalek::SigningKey::from_bytes(&[9; 32]),
        wats::node::now_secs(),
        0,
    )?;
    wats::store::Store::open(&peer_dir.join("events.sqlite"))?
        .insert(&missed, wats::node::now_secs())?;
    let missed = missed.id;

    // Back on its address.
    let peer = Peer::spawn(&peer_dir, port, &[]);
    peer.expect("ready");
    eventually("the post made during the outage", || {
        node.store().get(&missed).ok().flatten()
    })
    .await;
    eventually("online again", || {
        (connectivity(&node)["state"] == "online").then_some(())
    })
    .await;

    drop(peer);
    node.shutdown().await?;
    Ok(())
}

/// DNS fails at startup ("Try again"), then comes back: the node resolves
/// the bootstrap host again at each attempt, connects without a restart,
/// and syncs what was posted while it could not.
#[tokio::test(flavor = "multi_thread")]
async fn resolves_the_bootstrap_host_again_until_dns_answers() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let port = free_udp_port();
    let mut router_config = config(&dir.path().join("router"));
    router_config.port = Some(port);
    let router = Node::start(router_config).await?;

    let dns_up = Arc::new(AtomicBool::new(false));
    let lookups = Arc::new(AtomicUsize::new(0));
    let mut node_config = config(&dir.path().join("node"));
    node_config.bootstrap = vec![BootstrapEntry {
        id: router.endpoint_id(),
        host: "router.invalid".into(),
        port,
    }];
    node_config.resolver = Resolver::custom({
        let (dns_up, lookups) = (Arc::clone(&dns_up), Arc::clone(&lookups));
        move |entry: BootstrapEntry| {
            let up = dns_up.load(Ordering::SeqCst);
            lookups.fetch_add(1, Ordering::SeqCst);
            async move {
                anyhow::ensure!(up, "resolve {}: Try again", entry.host);
                Ok(iroh::EndpointAddr::from_parts(
                    entry.id,
                    [iroh::TransportAddr::Ip(
                        (std::net::Ipv4Addr::LOCALHOST, entry.port).into(),
                    )],
                ))
            }
        }
    });
    let node = Node::start(node_config).await?;

    eventually("offline and retrying", || {
        let c = connectivity(&node);
        (c["state"] == "offline" && c["attempt"].as_u64() >= Some(2)).then_some(())
    })
    .await;
    assert!(
        lookups.load(Ordering::SeqCst) >= 3,
        "DNS resolved at startup and again at every attempt"
    );
    assert_eq!(node.neighbor_count(PUBLIC_NET), 0);
    let missed = post(&router, "posted while DNS was down").await?;

    dns_up.store(true, Ordering::SeqCst);
    eventually("the post made while DNS was down", || {
        node.store().get(&missed.id).ok().flatten()
    })
    .await;
    eventually("online", || {
        (connectivity(&node)["state"] == "online").then_some(())
    })
    .await;

    for n in [router, node] {
        n.shutdown().await?;
    }
    Ok(())
}

/// A relay server on 127.0.0.1:`port`, plain HTTP, no QUIC address
/// discovery. Binding may briefly fail right after the previous server on
/// the same port stopped: retried.
async fn relay_on(port: u16) -> iroh_relay::server::Server {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let mut config = iroh_relay::server::ServerConfig::default();
        config.relay = Some(iroh_relay::server::RelayConfig::new((
            std::net::Ipv4Addr::LOCALHOST,
            port,
        )));
        match iroh_relay::server::Server::spawn(config).await {
            Ok(server) => return server,
            Err(e) if tokio::time::Instant::now() < deadline => {
                eprintln!("relay on port {port}: {e:#}; retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("relay on port {port}: {e:#}"),
        }
    }
}

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Two nodes that reach each other only through a relay on this machine
/// (no IP transport, no address lookup). The relay stops: both see they
/// are offline. A post lands on one side meanwhile. The relay stays down
/// for more than a minute, then comes back on the same URL: the other
/// node reconnects by itself and catches up, soon after both home relays
/// are back.
#[tokio::test(flavor = "multi_thread")]
async fn reconnects_when_the_relay_comes_back() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let port = free_tcp_port();
    let relay = relay_on(port).await;
    let relay_url: iroh::RelayUrl = format!("http://127.0.0.1:{port}").parse()?;
    let mode = NetworkMode::LocalRelay { port };

    let mut b_config = config(&dir.path().join("b"));
    b_config.mode = mode;
    let b = Node::start(b_config).await?;
    let mut a_config = config(&dir.path().join("a"));
    a_config.mode = mode;
    a_config.peers.push(iroh::EndpointAddr::from_parts(
        b.endpoint_id(),
        [iroh::TransportAddr::Relay(relay_url.clone())],
    ));
    let a = Node::start(a_config).await?;
    for node in [&a, &b] {
        // No direct path is possible: the relay carries everything.
        assert!(node.endpoint().bound_sockets().is_empty());
    }
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(30))
        .await?;
    for node in [&a, &b] {
        eventually("online through the relay", || {
            (connectivity(node)["state"] == "online").then_some(())
        })
        .await;
    }

    // Iroh notices only when the relay path goes idle: 30 s, plus up to
    // one 5 s heartbeat.
    relay.shutdown().await?;
    let down = tokio::time::Instant::now();
    for node in [&a, &b] {
        eventually("offline once the relay is gone", || {
            let c = connectivity(node);
            (node.neighbor_count(PUBLIC_NET) == 0 && c["state"] == "offline").then_some(())
        })
        .await;
    }
    eprintln!("both offline {:?} after the relay stopped", down.elapsed());

    // While the relay is down, a post lands in B's store (as if it got it
    // from someone else): A can only get it by catching up.
    let missed = Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Ask),
        title: Some("missed while the relay was down".into()),
        text: "Posted while the relay was down.".into(),
        ..Draft::default()
    }
    .sign(
        &ed25519_dalek::SigningKey::from_bytes(&[9; 32]),
        wats::node::now_secs(),
        0,
    )?;
    b.store().insert(&missed, wats::node::now_secs())?;
    let missed = missed.id;
    assert!(a.store().get(&missed)?.is_none());

    // The relay stays down longer than a QUIC handshake waits (30 s): the
    // dials A made while offline have failed by now. Gossip never dials a
    // peer again after its dial failed (see `Node::redial`); A must still
    // reconnect.
    tokio::time::sleep(Duration::from_secs(31)).await;

    // Back on the same port: the relay URL is unchanged. Each endpoint
    // dials the relay again on Iroh's own backoff (up to 16 s, doubled by
    // jitter): that part is not ours.
    let relay = relay_on(port).await;
    let up = tokio::time::Instant::now();
    eventually_within("both home relays back", Duration::from_secs(45), || {
        [&a, &b]
            .iter()
            .all(|node| {
                node.endpoint()
                    .home_relay_status()
                    .get()
                    .iter()
                    .any(|r| r.is_connected())
            })
            .then_some(())
    })
    .await;
    let relays_back = tokio::time::Instant::now();
    eprintln!(
        "both home relays back {:?} after the relay came back",
        up.elapsed()
    );
    // Ours: from there, A's dial in flight (10 s at most) gets through, or
    // the next attempt of its reconnect loop (at most 1 s apart here)
    // dials B afresh, and A catches up. It used to wait on the dial made
    // while the relay was down: a handshake retransmit up to 10 s later,
    // or never once that dial had failed.
    eventually_within(
        "the post made while the relay was down",
        Duration::from_secs(15),
        || a.store().get(&missed).ok().flatten(),
    )
    .await;
    eprintln!(
        "caught up {:?} after the relay came back, {:?} after both home relays",
        up.elapsed(),
        relays_back.elapsed()
    );
    for node in [&a, &b] {
        eventually("online again", || {
            (connectivity(node)["state"] == "online").then_some(())
        })
        .await;
    }

    for node in [a, b] {
        node.shutdown().await?;
    }
    relay.shutdown().await?;
    Ok(())
}
