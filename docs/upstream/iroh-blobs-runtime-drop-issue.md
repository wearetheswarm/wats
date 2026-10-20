# Draft issue for n0-computer/iroh-blobs (not posted)

Drafted while fixing a thread leak in wats (0.4.2). Not posted: it waits
for the maintainer's go.

---

## Title

Every closed `FsStore` leaks one thread: its runtime is dropped from one of its own threads and waits for itself

## Versions

- iroh-blobs 0.103.1 (`fs` store), tokio 1.53.3
- macOS 26 (Darwin 25.6) and Linux; nothing in the code path is platform
  specific

## Summary

`FsStore::load_with_opts` builds a private multi-thread tokio runtime and
moves it into the store's `Actor` (`_rt: RtWrapper`), which then runs as
a task on that same runtime. When the last handle goes (with or without
`Store::shutdown` first), `Actor::run` returns and drops the actor, on a
worker thread of that runtime. `RtWrapper::drop` then does

```rust
tokio::task::block_in_place(|| {
    drop(rt);
});
```

`block_in_place` hands the worker's core to another thread, but the
current thread stays a thread of the runtime's blocking pool. Dropping
the runtime shuts the pool down and waits, with no timeout, for every
thread of the pool to exit (`BlockingPool::shutdown(None)`), this one
included. It never does: the thread is parked for good, and the store's
other threads exit. So each `FsStore` that is closed leaves exactly one
thread behind, named `iroh-blob-store-<n>`.

Where the last handle is dropped makes no difference (a task, a plain
`std::thread`, after `shutdown()` or not): the actor is always dropped
on its own runtime.

Nothing waits on the stuck thread, so a process exits normally. But an
application that opens and closes stores for as long as it runs (wats
used a scratch store per blob repair) accumulates them.

## Repro

```rust
fn threads() -> usize {
    // macOS: one line per thread after the header. Linux:
    // std::fs::read_dir("/proc/self/task").unwrap().count()
    let out = std::process::Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().count() - 1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_store_leaks_a_thread() {
    let dir = tempfile::tempdir().unwrap();
    println!("start: {}", threads());
    for i in 0..5 {
        let root = dir.path().join(format!("s{i}"));
        let options = iroh_blobs::store::fs::options::Options::new(&root);
        let store = iroh_blobs::store::fs::FsStore::load_with_opts(root.join("blobs.db"), options)
            .await
            .unwrap();
        store.shutdown().await.unwrap();
        drop(store);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        println!("after closing store {i}: {}", threads());
    }
}
```

On our machines: `start: 4`, then 5, 6, 7, 8, 9, one more thread per
closed store. Dropping the store from a plain `std::thread` instead gives
the same (+1 each). A `MemStore` closed the same way leaks nothing.

## Suggested fix

Shut the runtime down without waiting for its threads, which is what
`Runtime::shutdown_background` is for ("drop a runtime from within
another runtime"); the threads then exit on their own once the actor's
task returns:

```rust
impl Drop for RtWrapper {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}
```

Checked with tokio alone: a multi-thread runtime that drops itself from
one of its tasks with `block_in_place(|| drop(rt))` leaves one thread per
runtime (2, 3, 4, 5 threads after three runs); with
`rt.shutdown_background()` it leaves none (5, 5, 5).

## What wats does meanwhile

A blob repair fetches the blob with the low-level `get::fsm` into a file
and imports it into the main store, with no scratch store
(`Node::repair_into`, `crates/wats/tests/blob_threads.rs`). The store
the node opens at start to check its files, and its main store at
shutdown, each still leave one thread: two per node start, once per
process in production.
