//! The blob collection (`storage.gc`) across crashes: a `wats node` killed
//! while it collects back to back loses no blob a kept post lists, and a
//! kept blob whose file went missing is reported, never a panic.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use iroh_blobs::Hash;
use iroh_blobs::api::proto::BlobStatus;
use iroh_blobs::store::fs::FsStore;
use wats::event::{Draft, Kind};
use wats::node::{Attachment, NetworkMode, Node, NodeConfig, PUBLIC_NET};

/// Above the 16 KiB the blob store inlines in its database: files.
const SIZE: usize = 40 * 1024;
const KEPT: u8 = 6;
const LOOSE: usize = 600;
const LOOSE_SIZE: usize = 17 * 1024;
const ROUNDS: usize = 4;

fn config(dir: &Path, gc_ms: u64) -> NodeConfig {
    let mut config = NodeConfig::new(dir);
    config.mode = NetworkMode::Loopback;
    config.infra = wats::infra::Infra::None;
    config.pow_bits = Some(8);
    config.min_pow_bits = Some(8);
    config.blob_gc_interval = Duration::from_millis(gc_ms);
    config
}

fn content(seed: u8) -> Vec<u8> {
    (0..SIZE).map(|i| (i % 251) as u8 ^ seed).collect()
}

/// Loose blobs (in no tag, listed by no post), as a dropped body or a
/// failed fetch leaves them.
async fn add_loose(data: &Path, round: usize) -> Vec<Hash> {
    let blobs = FsStore::load(data.join("blobs")).await.unwrap();
    let mut hashes = Vec::new();
    for i in 0..LOOSE {
        let mut bytes = vec![0u8; LOOSE_SIZE];
        bytes[..16].copy_from_slice(&((round * LOOSE + i) as u128).to_le_bytes());
        hashes.push(blobs.add_bytes(bytes).temp_tag().await.unwrap().hash());
    }
    blobs.shutdown().await.unwrap();
    hashes
}

fn data_file(data: &Path, hash: &Hash) -> std::path::PathBuf {
    data.join("blobs/data")
        .join(format!("{}.data", hash.to_hex()))
}

/// Every kept blob is complete, intact and on disk; returns how many of
/// `loose` are still in the store.
async fn check(data: &Path, kept: &[(Hash, Vec<u8>)], loose: &[Hash]) -> usize {
    let blobs = FsStore::load(data.join("blobs")).await.unwrap();
    for (hash, bytes) in kept {
        assert!(
            matches!(blobs.blobs().status(*hash).await.unwrap(), BlobStatus::Complete { size } if size == SIZE as u64),
            "kept blob {hash} lost"
        );
        assert!(
            data_file(data, hash).exists(),
            "file of kept blob {hash} lost"
        );
        assert_eq!(
            &blobs.blobs().get_bytes(*hash).await.unwrap()[..],
            &bytes[..]
        );
    }
    let mut left = 0;
    for hash in loose {
        if !matches!(
            blobs.blobs().status(*hash).await.unwrap(),
            BlobStatus::NotFound
        ) {
            left += 1;
        }
    }
    blobs.shutdown().await.unwrap();
    left
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_a_node_mid_collection_loses_nothing_kept() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("n");
    // Kept posts with a file each; half of their blobs untagged, as after
    // a crash between storing a fetched post and its tag.
    let mut kept = Vec::new();
    {
        let mut c = config(&data, 10);
        c.blob_gc = false;
        let node = Node::start(c).await.unwrap();
        for seed in 0..KEPT {
            let path = dir.path().join(format!("file-{seed}.bin"));
            std::fs::write(&path, content(seed)).unwrap();
            let event = node
                .publish(
                    Draft {
                        net: PUBLIC_NET.into(),
                        kind: Some(Kind::Note),
                        title: Some(format!("post {seed}")),
                        text: "kept".into(),
                        ..Draft::default()
                    },
                    &[Attachment { path, name: None }],
                )
                .await
                .unwrap();
            kept.push((event.blobs[0].hash.parse::<Hash>().unwrap(), content(seed)));
        }
        node.shutdown().await.unwrap();
    }
    {
        let blobs = FsStore::load(data.join("blobs")).await.unwrap();
        for (hash, _) in kept.iter().step_by(2) {
            blobs
                .tags()
                .delete(format!("wats/blob/{}", hash.to_hex()))
                .await
                .unwrap();
        }
        blobs.shutdown().await.unwrap();
    }

    // Kill a node collecting every millisecond once it has begun deleting
    // (a file of a loose blob gone), a little later each round.
    let mut midway = 0;
    let mut loose = Vec::new();
    for round in 0..ROUNDS {
        let fresh = add_loose(&data, round).await;
        let mut child = Command::new(env!("CARGO_BIN_EXE_wats"))
            .args([
                "--hermetic",
                "--infra",
                "none",
                "--no-default-bootstrap",
                "--dir",
                data.to_str().unwrap(),
                "node",
            ])
            .env("WATS_HERMETIC", "1")
            .env("WATS_TEST_BLOB_GC_MS", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while fresh.iter().all(|h| data_file(&data, h).exists()) {
            assert!(std::time::Instant::now() < deadline, "no collection began");
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_micros(500 * round as u64));
        child.kill().unwrap(); // SIGKILL: no shutdown, no flush
        child.wait().unwrap();
        let left = check(&data, &kept, &fresh).await;
        let files = fresh
            .iter()
            .filter(|h| data_file(&data, h).exists())
            .count();
        eprintln!("round {round}: {left} of {LOOSE} loose blobs left, {files} of their files");
        // Killed during the sweep: some blobs deleted and not others, or
        // all deleted in the database and some files left.
        if (0 < left && left < LOOSE) || (left == 0 && files > 0) {
            midway += 1;
        }
        loose.extend(fresh);
    }
    assert!(midway > 0, "no kill landed during a sweep");

    // A node that runs to the end deletes every loose blob, keeps the rest.
    let node = Node::start(config(&data, 10)).await.unwrap();
    let from = node.blob_collections();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while node.blob_collections() < from + 2 {
        assert!(tokio::time::Instant::now() < deadline, "no collection ran");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    node.shutdown().await.unwrap();
    drop(node);
    assert_eq!(check(&data, &kept, &loose).await, 0);
    // Nor their files, which a kill between the database commit and the
    // file removal leaves behind: the restart removed them.
    let files = loose
        .iter()
        .filter(|h| data_file(&data, h).exists())
        .count();
    assert_eq!(files, 0, "files of collected blobs left on disk");
    assert_eq!(
        std::fs::read_dir(data.join("blobs/temp")).unwrap().count(),
        0
    );
}

/// A kept blob whose file is gone (deleted by hand, a disk fault): the
/// node starts, says the blob is unavailable, and does not panic.
#[tokio::test(flavor = "multi_thread")]
async fn a_kept_blob_with_its_file_gone_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("n");
    let path = dir.path().join("file.bin");
    std::fs::write(&path, content(1)).unwrap();
    let node = Node::start(config(&data, 10)).await.unwrap();
    let event = node
        .publish(
            Draft {
                net: PUBLIC_NET.into(),
                kind: Some(Kind::Note),
                title: Some("post".into()),
                text: "kept".into(),
                ..Draft::default()
            },
            &[Attachment { path, name: None }],
        )
        .await
        .unwrap();
    node.shutdown().await.unwrap();
    drop(node);
    let blob = event.blobs[0].clone();
    let hash: Hash = blob.hash.parse().unwrap();
    std::fs::remove_file(data_file(&data, &hash)).unwrap();

    let node = Node::start(config(&data, 10)).await.unwrap();
    let read = node.blob_bytes(&blob.hash).await;
    assert!(!matches!(read, Ok(Some(_))), "{read:?}");
    let export = node.fetch_and_export(&blob).await;
    assert!(!matches!(export, Ok(Some(_))), "{export:?}");
    eprintln!("read: {read:?}; export: {export:?}");
    // Collections go on, and the post is still there.
    let from = node.blob_collections();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while node.blob_collections() < from + 2 {
        assert!(tokio::time::Instant::now() < deadline, "no collection ran");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(node.store().get(&event.id).unwrap().is_some());
    node.shutdown().await.unwrap();
}
