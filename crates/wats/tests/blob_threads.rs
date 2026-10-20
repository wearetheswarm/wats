//! Repairing blobs leaks no thread. A closed iroh-blobs store leaves one
//! thread of its runtime stuck for good (it drops its runtime from one of
//! its own threads, `docs/upstream/iroh-blobs-runtime-drop-issue.md`):
//! a repair must not open one. Alone in its binary, so that no other test
//! starts or stops threads meanwhile.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::Path;
use std::time::Duration;

use wats::event::{BlobRef, Draft, Kind};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig, PUBLIC_NET};

/// Above the 16 KiB the blob store inlines in its database: files.
const SIZE: usize = 40 * 1024;
const BLOBS: u8 = 8;

fn config(dir: &Path) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.infra = wats::infra::Infra::NONE;
    config.pow_bits = Some(8);
    config.min_pow_bits = Some(8);
    config
}

fn content(seed: u8) -> Vec<u8> {
    (0..SIZE).map(|i| (i % 251) as u8 ^ seed).collect()
}

/// The threads of the blob stores' runtimes on Linux (named
/// `iroh-blob-store-<n>`, truncated to 15 bytes); every thread of the
/// process on macOS, which lists no thread names.
fn threads() -> usize {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_dir("/proc/self/task")
            .unwrap()
            .filter_map(|task| std::fs::read_to_string(task.ok()?.path().join("comm")).ok())
            .filter(|name| name.starts_with("iroh-blob-store"))
            .count()
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        assert!(out.status.success(), "ps -M failed");
        // One line per thread, after the header.
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .count()
            .saturating_sub(1)
    }
}

async fn eventually<F, Fut, T>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(value) = f().await {
            return value;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Repair one blob of `node`: by a read (any size) for even seeds, by a
/// fetch (the size its post declares) for odd ones.
async fn repair(node: &Node, blob: &BlobRef, seed: u8) {
    if seed.is_multiple_of(2) {
        let bytes = node
            .blob_bytes(&blob.hash)
            .await
            .unwrap()
            .expect("restored");
        assert_eq!(&bytes[..], &content(seed)[..]);
    } else {
        let path = node
            .fetch_and_export(blob)
            .await
            .unwrap()
            .expect("restored");
        assert_eq!(std::fs::read(path).unwrap(), content(seed));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn repairing_blobs_leaks_no_thread() {
    tokio::time::timeout(Duration::from_secs(120), repairs())
        .await
        .expect("the test hung");
}

async fn repairs() {
    let dir = tempfile::tempdir().unwrap();
    let (data_a, data_b) = (dir.path().join("a"), dir.path().join("b"));
    let a = Node::start(config(&data_a)).await.unwrap();
    let mut b_config = config(&data_b);
    b_config.peers.push(a.endpoint().addr());
    let b = Node::start(b_config.clone()).await.unwrap();
    a.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await
        .unwrap();
    let mut blobs = Vec::new();
    for seed in 0..BLOBS {
        let path = dir.path().join(format!("file-{seed}.bin"));
        std::fs::write(&path, content(seed)).unwrap();
        let event = a
            .publish(
                Draft {
                    net: PUBLIC_NET.into(),
                    kind: Some(Kind::Note),
                    title: Some(format!("post {seed}")),
                    text: "kept on two nodes".into(),
                    ..Draft::default()
                },
                &[Attachment { path, name: None }],
            )
            .await
            .unwrap();
        blobs.push(event.blobs[0].clone());
    }
    for blob in &blobs {
        eventually("blob on b", || async {
            b.blob_bytes(&blob.hash).await.ok().flatten()
        })
        .await;
    }
    b.shutdown().await.unwrap();
    drop(b);
    // Every file gone on B: each read repairs one blob from A.
    for blob in &blobs {
        std::fs::remove_file(
            data_b
                .join("blobs/data")
                .join(format!("{}.data", blob.hash)),
        )
        .unwrap();
    }
    let b = Node::start(b_config).await.unwrap();
    b.wait_for_neighbor(PUBLIC_NET, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(b.unreadable_blobs(), usize::from(BLOBS), "found at start");

    // The first repair warms up what any fetch starts (a connection, I/O
    // threads); the others must start nothing that stays.
    repair(&b, &blobs[0], 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = threads();
    for (seed, blob) in (1..BLOBS).zip(&blobs[1..]) {
        repair(&b, blob, seed).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after = threads();
    assert_eq!(b.unreadable_blobs(), 0);
    // A leak is one thread per repair (7 here); allow some slack for the
    // runtime's I/O threads on macOS, where all threads are counted.
    assert!(
        after <= before + 2,
        "{} repairs left {} more threads ({before} before, {after} after)",
        BLOBS - 1,
        after.saturating_sub(before)
    );
    b.shutdown().await.unwrap();
    a.shutdown().await.unwrap();
}
