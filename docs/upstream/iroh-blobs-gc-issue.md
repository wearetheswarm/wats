# Draft issue for n0-computer/iroh-blobs (not posted)

Drafted while adding blob collection to wats (PR #129). Not posted: it
waits for the maintainer's go.

---

## Title

FsStore GC deletes blobs under an in-progress download, and a re-add during a sweep can leave a complete entry with no file

## Versions

- iroh-blobs 0.103.1 (`fs` store, `GcConfig` with a 1 ms interval to make
  the races visible; they happen at any interval)
- iroh 1.3.0, redb 4.4.0, tokio 1.53.3
- macOS 26 (Darwin 25.6), APFS; nothing in the code paths below is
  platform specific

## Summary

1. **A download is not protected from GC.** `Downloader::download` holds
   no temp tag, and once a partial blob is in file mode its writes do not
   touch the metadata db (`BaoFileStorage::write_batch` returns no
   `EntryState` update for a plain batch, `store/fs/bao_file.rs`). So the
   hash is neither a root nor in the GC's "written since the last run"
   set, and the sweep deletes the entry and unlinks the files while the
   download writes to them. The download then completes, writes a
   `Complete` entry, and the data file is gone: `status` says `Complete`,
   reads fail with "poisoned storage".
2. **Re-adding a blob that a sweep is deleting can leave a complete entry
   with no file**, although the add holds a temp tag. The import moves its
   file into place and calls `protect()` (`finish_import_impl`), but when
   the sweep's delete transaction commits around it, `DeleteSet::commit`
   unlinks the freshly renamed file and the import's `Set` then records it
   as `Complete`.
3. Once an entry is `Complete` with its file gone there is no public way
   to reset or delete it: `Blobs::delete` is `pub(crate)` and the GC keeps
   it while it is tagged. (Importing the same bytes again does repair it,
   see below.)

## Repro

A test file against iroh-blobs alone (dev-dependencies: `tokio` with
`full`, `tempfile`, `iroh`). Both tests print what they find; on our
machines both show the problem on every run.

```rust
use std::time::Duration;

use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::Endpoint;
use iroh_blobs::store::fs::options::Options;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::store::GcConfig;
use iroh_blobs::BlobsProtocol;

async fn endpoint(lookup: &MemoryLookup) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .clear_ip_transports()
        .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .address_lookup(lookup.clone())
        .bind()
        .await
        .unwrap()
}

fn gc_options(root: &std::path::Path) -> Options {
    let mut options = Options::new(root);
    options.gc = Some(GcConfig {
        interval: Duration::from_millis(1),
        add_protected: None,
    });
    options
}

/// 1. A download into an FsStore whose GC runs.
#[tokio::test(flavor = "multi_thread")]
async fn download_is_not_protected_from_gc() {
    let lookup = MemoryLookup::new();
    let provider_store = MemStore::new();
    let data: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 253) as u8).collect();
    let tag = provider_store.add_bytes(data.clone()).temp_tag().await.unwrap();
    let hash = tag.hash();
    let provider = endpoint(&lookup).await;
    let router = Router::builder(provider.clone())
        .accept(iroh_blobs::ALPN, BlobsProtocol::new(&provider_store, None))
        .spawn();
    lookup.add_endpoint_info(provider.addr());

    let mut lost = 0;
    for round in 0..5 {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        let store = FsStore::load_with_opts(root.join("blobs.db"), gc_options(&root))
            .await
            .unwrap();
        let client = endpoint(&lookup).await;
        let result = store.downloader(&client).download(hash, vec![provider.id()]).await;
        // Pinned at once, as an application would.
        store.tags().set("mine", hash).await.unwrap();
        let status = store.blobs().status(hash).await.unwrap();
        let file = root.join("data").join(format!("{}.data", hash.to_hex()));
        let read = store.blobs().get_bytes(hash).await;
        eprintln!(
            "round {round}: download ok {}, status {status:?}, file exists {}, read ok {}",
            result.is_ok(),
            file.exists(),
            read.is_ok()
        );
        if !read.is_ok_and(|b| b[..] == data[..]) {
            lost += 1;
        }
        client.close().await;
        store.shutdown().await.unwrap();
    }
    router.shutdown().await.unwrap();
    eprintln!("downloads lost to the GC: {lost} of 5");
}

/// 2. Re-adding, under a temp tag, a blob the GC is deleting.
#[tokio::test(flavor = "multi_thread")]
async fn readd_during_gc_can_lose_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    let store = FsStore::load_with_opts(root.join("blobs.db"), gc_options(&root))
        .await
        .unwrap();
    let (mut broken, rounds) = (0, 3000);
    for i in 0..rounds {
        // Each content is added 20 times in a row: garbage between adds.
        let mut data = vec![0u8; 40 * 1024];
        data[..8].copy_from_slice(&((i / 20) as u64).to_le_bytes());
        let tt = store.add_bytes(data).temp_tag().await.unwrap();
        let hash = tt.hash();
        let file = root.join("data").join(format!("{}.data", hash.to_hex()));
        if !file.exists() {
            let status = store.blobs().status(hash).await.unwrap();
            eprintln!("round {i}: held blob {status:?} with no file");
            broken += 1;
        }
        drop(tt);
    }
    store.shutdown().await.unwrap();
    eprintln!("held blobs without their file: {broken} of {rounds}");
}
```

## Expected

1. A blob being downloaded is not deleted by GC, or at least the
   download fails and leaves no `Complete` entry without data.
2. A blob held by a temp tag right after `add_bytes` returns is readable.

## Actual

1. Five rounds out of five: `download ok true, status Complete { size:
   8388608 }, file exists false, read ok false` (the read error is
   `poisoned storage`).
2. About one add in fifteen (208 of 3000 on our run): `held blob
   Complete { size: 40960 } with no file`.

## Suggested fixes

- **The downloader holds a temp tag** (scope of the request) on the root,
  and on hashseq children as they are learnt, from before the first write
  until the download ends; or `execute_get` takes one. Callers then pin
  under their own tag before the temp tag goes.
- **Partial writes mark the hash protected** (insert it into the meta
  actor's `protected` set) even when they produce no `EntryState` update.
  That would cover downloads written through `import_bao` by other code.
- **Close the import/sweep race**: e.g. have `delete` skip hashes with a
  live handle or a temp tag, check `protected` again when committing the
  file deletions, or remove the files inside the same critical section as
  the `protect()` call.
- **A public repair or delete API**: `Blobs::delete(hash, force)` for a
  hash the caller knows is broken, or a `verify`/`repair` call that drops
  an entry whose files are missing. Today the only repair we found is to
  fetch the bytes elsewhere and import them again, which works because
  `finish_import_impl` writes the file and the entry anew over a
  `Complete` entry (`BaoFileHandle::complete` leaves a poisoned handle
  alone, but the next load reads the new file).

## Two more observations

- A crash between the GC's db commit and `DeleteSet::commit` leaves files
  with no entry, forever: no later run lists them. A startup pass that
  removes files of hashes absent from the db (and stale `temp/` files)
  would fix that. wats does this itself before it opens the store with GC.
- Inside a write batch, read commands (`status`, `has`) see the batch's
  uncommitted deletes, so `status` can say `NotFound` for a blob whose
  deletion is then never committed (e.g. the process dies). Code that
  removes files based on `status` must not run concurrently with a sweep.
  Worth a line in the docs of `status`.

## How wats works around it (for context)

- Every fetch holds its hash in a set our `add_protected` callback reads
  and under a temp tag, from before the first byte until it is pinned.
- A blob recorded `Complete` whose content cannot be read is noted (at
  start, by a file check, or when a read fails), fetched again into a
  scratch `FsStore`, exported to a file and imported again into the main
  store, which restores it.
- Files a crash left behind are removed at start, before GC runs.
