//! A relay of your own (ADR 0009, `infra_relays` / `--relay`): two
//! endpoints built from the same custom infrastructure as a node in
//! Internet mode reach each other through a relay server on this machine.
//! Hermetic: the relay listens on 127.0.0.1, the endpoints have no IP
//! transport (only the relay can carry the traffic) and no directory.

use std::time::Duration;

use wats::infra::Infra;

const ALPN: &[u8] = b"wats-test/custom-relay/0";

/// A relay server on 127.0.0.1, plain HTTP, no QUIC address discovery.
async fn local_relay() -> anyhow::Result<(iroh_relay::server::Server, u16)> {
    let mut config = iroh_relay::server::ServerConfig::default();
    config.relay = Some(iroh_relay::server::RelayConfig::new((
        std::net::Ipv4Addr::LOCALHOST,
        0,
    )));
    let server = iroh_relay::server::Server::spawn(config).await?;
    let port = server
        .http_addr()
        .map(|a| a.port())
        .ok_or_else(|| anyhow::anyhow!("the relay has no HTTP address"))?;
    Ok((server, port))
}

#[tokio::test(flavor = "multi_thread")]
async fn two_endpoints_talk_through_a_relay_of_your_own() -> anyhow::Result<()> {
    let (relay, port) = local_relay().await?;
    let relay_url = format!("http://127.0.0.1:{port}");
    let infra = Infra::resolve(None, std::slice::from_ref(&relay_url), None)?;
    assert_eq!(
        infra.to_string(),
        format!("custom (relay 127.0.0.1:{port}; no lookup)")
    );

    let bind = || async {
        infra
            .builder()?
            .clear_ip_transports()
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .map_err(anyhow::Error::from)
    };
    let (a, b) = (bind().await?, bind().await?);
    for endpoint in [&a, &b] {
        tokio::time::timeout(Duration::from_secs(30), endpoint.online()).await?;
        assert!(endpoint.bound_sockets().is_empty(), "no IP transport");
        let home: Vec<String> = endpoint
            .addr()
            .relay_urls()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(home, vec![format!("{relay_url}/")]);
    }

    let server = tokio::spawn({
        let b = b.clone();
        async move {
            let incoming = b.accept().await.ok_or_else(|| anyhow::anyhow!("closed"))?;
            let conn = incoming.await?;
            let (mut send, mut recv) = conn.accept_bi().await?;
            let got = recv.read_to_end(1024).await?;
            send.write_all(&got).await?;
            send.finish()?;
            conn.closed().await;
            anyhow::Ok(())
        }
    });
    // B's address: its id and home relay, nothing else.
    let conn = tokio::time::timeout(Duration::from_secs(30), a.connect(b.addr(), ALPN)).await??;
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(b"through my own relay").await?;
    send.finish()?;
    let echo = tokio::time::timeout(Duration::from_secs(30), recv.read_to_end(1024)).await??;
    assert_eq!(echo, b"through my own relay");
    conn.close(0u32.into(), b"done");
    tokio::time::timeout(Duration::from_secs(10), server).await???;

    for endpoint in [a, b] {
        endpoint.close().await;
    }
    relay.shutdown().await?;
    Ok(())
}
