//! Author retractions (ADR 0020 §5.1) on a running node, and the
//! `wats retract`, `wats agent retracted` and `wats status` commands.

use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use serde_json::json;
use wats::event::{Draft, Event, Kind, rotate_signature};
use wats::node::{NetworkMode, Node, NodeConfig, PUBLIC_NET};
use wats::store::DropReason;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn note(seed: u8, text: &str) -> Event {
    Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Note),
        title: Some("a post".into()),
        text: text.into(),
        ..Draft::default()
    }
    .sign(&key(seed), now(), 0)
    .unwrap()
}

fn rotate(old: u8, new: u8) -> Event {
    Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Rotate),
        title: Some("rotate".into()),
        text: "handing over to my next key".into(),
        data: json!({
            "new_key": hex::encode(key(new).verifying_key().as_bytes()),
            "new_key_sig": rotate_signature(&key(new), key(old).verifying_key().as_bytes()),
        })
        .as_object()
        .unwrap()
        .clone(),
        ..Draft::default()
    }
    .sign(&key(old), now(), 0)
    .unwrap()
}

async fn start_node(dir: &std::path::Path) -> anyhow::Result<std::sync::Arc<Node>> {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.pow_bits = Some(0);
    config.min_pow_bits = Some(0);
    Node::start(config).await
}

async fn receive(node: &Node, event: &Event) -> anyhow::Result<()> {
    node.publish_signed(serde_json::to_value(event)?).await?;
    Ok(())
}

/// A retraction by a successor key (4 -> 5 -> 6): pending while the
/// rotation chain is missing, whether its target was held or arrived
/// afterwards; applied when the last `rotate` arrives.
#[tokio::test(flavor = "multi_thread")]
async fn a_successor_retraction_waits_for_the_rotation_chain() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let node = start_node(&dir.path().join("n")).await?;
    let held = note(4, "held before the retraction");
    let later = note(4, "arrives after the retraction");
    let other = note(7, "someone else's post");
    receive(&node, &held).await?;
    receive(&node, &other).await?;
    let retraction = wats::retract::draft(
        PUBLIC_NET,
        vec![held.id.clone(), later.id.clone(), other.id.clone()],
        None,
    )?
    .sign(&key(6), now(), 0)?;
    receive(&node, &retraction).await?;
    // No chain yet: nothing dropped, the absent post remembered.
    assert!(node.store().get(&held.id)?.is_some(), "pending");
    assert_eq!(
        node.store().drop_record(&later.id)?.map(|r| r.reason),
        Some(DropReason::Retraction)
    );
    // The absent post arrives: its author (4) does not match 6 with what
    // this node holds, so its body is stored and the record goes.
    receive(&node, &later).await?;
    assert!(node.store().get(&later.id)?.is_some());
    assert!(node.store().drop_record(&later.id)?.is_none());

    receive(&node, &rotate(4, 5)).await?;
    assert!(
        node.store().get(&held.id)?.is_some(),
        "still a link missing"
    );
    receive(&node, &rotate(5, 6)).await?;
    for id in [&held.id, &later.id] {
        assert!(node.store().get(id)?.is_none(), "dropped once chained");
        let record = node.store().drop_record(id)?.unwrap();
        assert_eq!(record.reason, DropReason::Retraction);
        assert_eq!(record.trigger.as_deref(), Some(retraction.id.as_str()));
    }
    // Someone else's post is never dropped.
    assert!(node.store().get(&other.id)?.is_some());
    assert!(node.store().drop_record(&other.id)?.is_none());

    // A retraction is a root post with refs: anything else is refused.
    let reply = Draft {
        net: PUBLIC_NET.into(),
        thread: Some(other.id.clone()),
        parent: Some(other.id.clone()),
        kind: Some(Kind::Ext(wats::retract::KIND.into())),
        text: "retracted".into(),
        ..Draft::default()
    }
    .sign(&key(7), now(), 0)?;
    assert!(receive(&node, &reply).await.is_err());
    let empty = Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Ext(wats::retract::KIND.into())),
        title: Some("Retraction".into()),
        text: "nothing".into(),
        ..Draft::default()
    }
    .sign(&key(7), now(), 0)?;
    assert!(receive(&node, &empty).await.is_err());
    node.shutdown().await?;
    Ok(())
}

/// `retract_as` checks every post before publishing anything.
#[tokio::test(flavor = "multi_thread")]
async fn retract_as_checks_ownership_first() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let node = start_node(&dir.path().join("n")).await?;
    let mine = node
        .publish(
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                title: Some("mine".into()),
                text: "mine".into(),
                ..Draft::default()
            },
            &[],
        )
        .await?;
    let theirs = note(7, "theirs");
    receive(&node, &theirs).await?;
    let both = [mine.id.clone(), theirs.id.clone()];
    let err = node
        .retract_as(node.default_author(), &both, None)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("is not yours"), "{err:#}");
    assert!(node.store().get(&mine.id)?.is_some(), "nothing retracted");
    let missing = "ab".repeat(32);
    let err = node
        .retract_as(node.default_author(), &[missing], None)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("not on this node"), "{err:#}");
    node.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
fn wats(data: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_wats"))
        .env("WATS_HERMETIC", "1")
        .env_remove("WATS_INFRA")
        .env_remove("WATS_BOOTSTRAP")
        .env_remove("WATS_PEERS")
        .env_remove("WATS_PROFILE")
        .arg("--dir")
        .arg(data)
        .args(["--infra", "none", "--pow-bits", "4"])
        .args(args)
        .output()
        .unwrap()
}

/// `wats retract` with no node running (a short-lived one), `--profile`,
/// and what `wats status` and `wats agent retracted` show of agents'
/// retractions.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn the_retract_commands() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let data = dir.path().join("d");
    let node = start_node(&data).await?;
    let draft = |text: &str| Draft {
        net: PUBLIC_NET.into(),
        kind: Some(Kind::Note),
        title: Some("a post".into()),
        text: text.into(),
        ..Draft::default()
    };
    let mine = node.publish(draft("leaked"), &[]).await?;
    let helper = node.author_key("helper")?;
    let helpers = node.publish_as(&helper, draft("helper's"), &[]).await?;
    node.shutdown().await?;
    drop(node);

    // Another profile's post: refused, with the way out.
    let out = wats(&data, &["retract", &helpers.id]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("is not yours") && err.contains("--profile"),
        "{err}"
    );
    let out = wats(
        &data,
        &[
            "--profile",
            "helper",
            "retract",
            &helpers.id,
            "--reason",
            "oops",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let out = wats(&data, &["retract", &mine.id]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("retracted 1 post(s) on public"), "{text}");

    let node = start_node(&data).await?;
    for id in [&mine.id, &helpers.id] {
        assert!(node.store().get(id)?.is_none(), "{id} dropped");
        assert_eq!(
            node.store().drop_record(id)?.map(|r| r.reason),
            Some(DropReason::Retraction)
        );
    }
    node.shutdown().await?;
    drop(node);
    // The human's retractions are not agents': no log, no audit copy.
    assert!(wats::retract::agent_log(&data)?.is_empty());
    assert!(!data.join("agent").join("retracted").exists());

    // What an agent retracted shows in `wats status` and `wats agent
    // retracted`, with its audit copy.
    let id = "cd".repeat(32);
    wats::retract::append_agent_log(
        &data,
        &wats::retract::AgentRetraction {
            at: now(),
            profile: "alice-agent".into(),
            ids: vec![id.clone()],
            reason: "posted a secret by mistake".into(),
            events: vec![],
            run: None,
        },
    )?;
    wats::retract::save_audit_copy(&data, &id, &json!({"id": id, "text": "the body"}))?;
    let out = wats(&data, &["status", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(status["agent_retractions"]["count"], 1, "{status}");
    assert_eq!(status["agent_retractions"]["ids"], json!([id]));
    let out = wats(&data, &["status"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("agent retractions (last 7 days): 1 post(s) by alice-agent"),
        "{text}"
    );
    let out = wats(&data, &["agent", "retracted"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("alice-agent") && text.contains(&id) && text.contains("by mistake"),
        "{text}"
    );
    let out = wats(&data, &["agent", "retracted", &id]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("the body"));
    assert!(
        !wats(&data, &["agent", "retracted", &"ef".repeat(32)])
            .status
            .success()
    );
    Ok(())
}
