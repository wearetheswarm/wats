# Draft issue for n0-computer/iroh-gossip (not posted)

Drafted while chasing an ask that a seed fleet never received (wats
0.4.10, 2026-10-23). Not posted: it waits for the maintainer's go.

---

## Title

A failed write leaves the peer as an active neighbor with a dead send loop: no `NeighborDown`, every message to it dropped until the connection closes

## Versions

- iroh-gossip 0.101.0, iroh 1.3.0, tokio 1.53.3
- macOS 26 (Darwin 25.6) and Linux (Docker); nothing in the code path is
  platform specific

## Summary

When `SendLoop::run` returns an error while the QUIC connection stays
open, the peer is never marked as disconnected:

- `connection_loop` runs the send and recv loops with `tokio::join!`.
  It waits for `RecvLoop::run`, and that loop ends only when the
  connection closes.
- The connection stays up: the other side keeps it alive and still
  holds it as its own connection to us. So the task never finishes, and
  `handle_connection_task_finished` never sends `PeerDisconnected`.
- The peer stays `PeerState::Active` with an `active_send_tx` whose
  receiver is gone.
- From then on, every `SendMessage` to that peer logs `failed to send:
  connection task send loop terminated` and is dropped. The comment there
  expects `PeerDisconnected` to come from the finished task, but it never
  comes. HyParView keeps the peer in the active view, and no
  `NeighborDown` is emitted, so the application cannot tell either.

The link stays dead for as long as the connection lives: hours, in our
case. Broadcasts reach that neighbor neither eagerly nor lazily. A
cluster whose only links out are such neighbors is cut off from the
swarm, while every node still reports its neighbors as up.

## What made a write fail

A write fails when the remote stops reading the stream, for any reason.
The repro below uses the one we could trigger on demand:

1. A sends a frame larger than B's `max_message_size`.
2. On B, `RecvStreamState::next` fails with "message too large". The
   recv loop logs `recv stream closed with error` and drops the stream,
   which sends `STOP_SENDING`.
3. A's next write on that stream fails, and A's send loop returns the
   error.

In production, the trigger came after a peer restarted. We did not find
which write error it was, but the log line is the same:

```
WARN gossip{me=5962b85866}: iroh_gossip::net: failed to send: connection task send loop terminated peer=9f5d9e24df
```

It repeated every few minutes until the node restarted.

## Repro

`Cargo.toml`:

```toml
[dependencies]
iroh = "=1.3.0"
iroh-gossip = "=0.101.0"
n0-future = "0.3"
tokio = { version = "1", features = ["full"] }
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

```rust
use std::time::Duration;

use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::Endpoint;
use iroh_gossip::api::Event;
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use n0_future::StreamExt;

async fn node(max: usize, lookup: &MemoryLookup) -> (Endpoint, Gossip, Router) {
    let endpoint = Endpoint::builder(presets::Minimal)
        .clear_ip_transports()
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .address_lookup(lookup.clone())
        .bind()
        .await
        .unwrap();
    let gossip = Gossip::builder().max_message_size(max).spawn(endpoint.clone());
    let router = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();
    (endpoint, gossip, router)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("iroh_gossip=debug").init();
    let lookup = MemoryLookup::new();
    let (ea, ga, _ra) = node(64 * 1024, &lookup).await;
    let (eb, gb, _rb) = node(1024, &lookup).await;
    lookup.add_endpoint_info(ea.addr());
    lookup.add_endpoint_info(eb.addr());
    let topic = TopicId::from_bytes([7; 32]);
    let (sa, mut ra) = ga.subscribe(topic, vec![]).await.unwrap().split();
    let (_sb, mut rb) = gb.subscribe(topic, vec![ea.id()]).await.unwrap().split();
    while !matches!(ra.next().await, Some(Ok(Event::NeighborUp(_)))) {}
    while !matches!(rb.next().await, Some(Ok(Event::NeighborUp(_)))) {}

    sa.broadcast(b"small before".to_vec().into()).await.unwrap();
    // B receives it.
    loop {
        if let Some(Ok(Event::Received(_))) = rb.next().await {
            break;
        }
    }

    // Larger than B's max_message_size.
    sa.broadcast(vec![0u8; 2048].into()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (mut received, mut down) = (0, false);
    for i in 0..20 {
        sa.broadcast(format!("small after {i}").into_bytes().into()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        while let Ok(Some(Ok(ev))) = tokio::time::timeout(Duration::from_millis(10), rb.next()).await {
            received += matches!(ev, Event::Received(_)) as u32;
        }
        while let Ok(Some(Ok(ev))) = tokio::time::timeout(Duration::from_millis(10), ra.next()).await {
            down |= matches!(ev, Event::NeighborDown(_));
        }
    }
    println!("B received {received}/20 small messages; A saw NeighborDown: {down}");
}
```

Output:

```
DEBUG gossip{me=2c49095819}:conn{peer=3ddad52a01}:recv: iroh_gossip::net::util: recv stream closed with error: message too large
WARN gossip{me=3ddad52a01}: iroh_gossip::net: failed to send: connection task send loop terminated peer=2c49095819
WARN gossip{me=2c49095819}: iroh_gossip::net: failed to send: connection task send loop terminated peer=3ddad52a01
B received 0/20 small messages; A saw NeighborDown: false
```

The oversized message is expected to fail. What follows is not: the two
nodes stay neighbors on both sides, with no traffic in either direction
and no event saying so.

## Expected

A send loop that ends with an error should end the peer's connection
task, or at least mark the peer as disconnected. HyParView can then
replace it, emit `NeighborDown`, and dial again if it wants the peer
back.

## Possible fixes

- In `connection_loop`, close the connection when the send loop returns
  an error, e.g. `conn.close(..., b"send failed")`. The recv loop then
  ends, the task finishes, and `handle_connection_task_finished` sends
  `PeerDisconnected` as it does today.
- Or in the actor: when `active_send_tx.send` fails, handle it as
  `PeerDisconnected` right away, instead of only logging and waiting for
  the task.
- For the trigger in the repro, on the receiving side: treat a frame over
  the limit as an error for that peer (close the connection), rather
  than dropping the stream silently while the connection lives on.

## Workaround on our side

wats now reads its neighbors' newest events every few minutes over its
own protocol (anti-entropy), so a dead gossip link delays events instead
of losing them. Gossip still stays dead toward that neighbor until the
connection closes.
