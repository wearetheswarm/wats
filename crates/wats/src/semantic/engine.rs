//! Semantic search on a node (ADR 0053 §4–§7): the model file and its
//! download, the worker that embeds new posts then the history, the
//! vectors in memory, and what `status` says.
//!
//! - **Never in the way.** One thread at background priority embeds in
//!   batches of [`BATCH`], new posts first, then the history newest first,
//!   working at most half the time. The backfill pauses on battery and
//!   under load. Search works from the first second: vectors where they
//!   exist, full-text search for everything.
//! - **Resumable.** `semantic_state` keeps where the scan of new posts
//!   (`top`) and the backfill (`below`) stand.
//! - **Loaded only when needed.** The model is loaded for a batch or a
//!   query, and unloaded after [`IDLE_UNLOAD`] unused.
//! - **The model file** is downloaded once from our release assets,
//!   checked against its pinned SHA-256 and its minisign signature, never
//!   from anywhere else. Without it, search is full-text, as before 0.5.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use super::rank::{self, VectorHit};
use super::{Index, MODEL_BYTES, MODEL_FILE, MODEL_SHA256, Model};
use crate::store::Store;

/// Posts embedded per batch.
pub const BATCH: usize = 16;
/// The model is unloaded after this long unused.
pub const IDLE_UNLOAD: Duration = Duration::from_secs(10 * 60);
/// How often an idle worker looks for new posts.
pub const POLL: Duration = Duration::from_secs(2);
/// The release the model file is published in.
pub const MODEL_URL: &str =
    "https://github.com/wearetheswarm/wats/releases/download/model-bge-small-en-v1.5-f16/";
/// After a failed download: 1 h, 6 h, then daily.
pub const DOWNLOAD_RETRY: [Duration; 3] = [
    Duration::from_secs(3600),
    Duration::from_secs(6 * 3600),
    Duration::from_secs(24 * 3600),
];

/// What computes embeddings: the model, or a stand-in in tests.
pub trait Embed: Send + Sync {
    fn passage(&self, title: &str, text: &str) -> Vec<f32>;
    fn query(&self, query: &str) -> Vec<f32>;
}

impl Embed for Model {
    fn passage(&self, title: &str, text: &str) -> Vec<f32> {
        Model::passage(self, title, text)
    }

    fn query(&self, query: &str) -> Vec<f32> {
        Model::query(self, query)
    }
}

/// Loads the model from its file.
pub type Loader = Box<dyn Fn(&std::path::Path) -> Result<Arc<dyn Embed>> + Send + Sync>;

/// The real loader: the pinned file.
pub fn model_loader() -> Loader {
    Box::new(|path| {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Ok(Arc::new(Model::load(&bytes)?) as Arc<dyn Embed>)
    })
}

/// How a node runs semantic search.
#[derive(Debug, Clone)]
pub struct Config {
    /// The model file.
    pub model_file: PathBuf,
    /// The model's identity in the store: its file's SHA-256.
    pub model_id: String,
    /// Where to download the file from (a directory URL), or `None`
    /// (hermetic, no infrastructure): it must be there already.
    pub download: Option<String>,
    /// Pause the backfill on battery and under load.
    pub laptop_manners: bool,
}

impl Config {
    /// The model file in `dir` (`<data dir>/models`, or
    /// `semantic.model_dir`).
    pub fn new(dir: PathBuf, download: bool) -> Self {
        Config {
            model_file: dir.join(MODEL_FILE),
            model_id: MODEL_SHA256.to_string(),
            download: download.then(|| MODEL_URL.to_string()),
            laptop_manners: true,
        }
    }
}

#[derive(Default)]
struct Vectors {
    index: Index,
    /// The table's count and highest `seq` when loaded.
    count: usize,
    max: i64,
}

/// Semantic search's state on a node.
pub struct Semantic {
    config: Config,
    loader: Loader,
    model: Mutex<Option<(Arc<dyn Embed>, Instant)>>,
    vectors: Mutex<Vectors>,
    /// Why there are no vectors (e.g. the model is not downloaded).
    off: Mutex<Option<String>>,
    stop: AtomicBool,
    /// When the backfill started, for its "ready" line.
    started: Mutex<Option<Instant>>,
}

/// What one batch did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub embedded: usize,
    /// The backfill finished with this batch.
    pub finished: bool,
}

impl Semantic {
    pub fn new(config: Config, loader: Loader) -> Self {
        Semantic {
            config,
            loader,
            model: Mutex::new(None),
            vectors: Mutex::default(),
            off: Mutex::new(None),
            stop: AtomicBool::new(false),
            started: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    fn set_off(&self, why: Option<String>) {
        if let Ok(mut off) = self.off.lock() {
            *off = why;
        }
    }

    /// The model, loaded if its file is there.
    pub fn model(&self) -> Option<Arc<dyn Embed>> {
        let mut slot = self.model.lock().ok()?;
        if let Some((model, used)) = slot.as_mut() {
            *used = Instant::now();
            return Some(Arc::clone(model));
        }
        if !self.config.model_file.exists() {
            return None;
        }
        let started = Instant::now();
        match (self.loader)(&self.config.model_file) {
            Ok(model) => {
                debug!("semantic: model loaded in {:?}", started.elapsed());
                *slot = Some((Arc::clone(&model), Instant::now()));
                self.set_off(None);
                Some(model)
            }
            Err(e) => {
                warn!("semantic: the model file does not load: {e:#}");
                // A bad file is removed: it is downloaded again.
                let _ = std::fs::remove_file(&self.config.model_file);
                self.set_off(Some(format!("model file refused ({e})")));
                None
            }
        }
    }

    /// Unloads the model when unused for [`IDLE_UNLOAD`]; the vectors in
    /// memory go with it.
    pub fn unload_if_idle(&self) {
        let Ok(mut slot) = self.model.lock() else {
            return;
        };
        if slot
            .as_ref()
            .is_some_and(|(_, used)| used.elapsed() >= IDLE_UNLOAD)
        {
            *slot = None;
            if let Ok(mut v) = self.vectors.lock() {
                *v = Vectors::default();
            }
            debug!("semantic: model unloaded (idle)");
        }
    }

    /// Brings the vectors in memory up to the table: the new rows, or all
    /// of them again when some were deleted.
    fn refresh(&self, store: &Store, vectors: &mut Vectors) -> Result<()> {
        let model = &self.config.model_id;
        let (count, max) = store.embeddings_extent(model)?;
        if (count, max) == (vectors.count, vectors.max) {
            return Ok(());
        }
        if count < vectors.count || max < vectors.max {
            *vectors = Vectors::default();
        }
        for (seq, blob) in store.embeddings_after(model, vectors.max)? {
            vectors.index.push(seq, &blob);
        }
        if vectors.index.len() != count {
            // Rows deleted and others added: load them all again.
            *vectors = Vectors::default();
            for (seq, blob) in store.embeddings_after(model, 0)? {
                vectors.index.push(seq, &blob);
            }
        }
        (vectors.count, vectors.max) = (count, max);
        Ok(())
    }

    /// The posts that match `query` by meaning, gated (ADR 0053 §2), at
    /// most `k`: `None` when semantic search cannot run (no model, no
    /// vector), and full-text search alone applies.
    pub fn hits(&self, store: &Store, query: &str, k: usize) -> Option<Vec<VectorHit>> {
        let model = self.model()?;
        let embedding = model.query(query);
        let mut vectors = self.vectors.lock().ok()?;
        if let Err(e) = self.refresh(store, &mut vectors) {
            warn!("semantic: reading the vectors: {e:#}");
            return None;
        }
        if vectors.index.is_empty() {
            return None;
        }
        let background = super::background().scores(&embedding);
        let background: Vec<f32> = background.into_iter().map(|(_, c)| c).collect();
        Some(rank::gate(
            &vectors.index.scores(&embedding),
            &background,
            k,
        ))
    }

    /// One batch: new posts first, then the history (unless `pause`).
    pub fn step(&self, store: &Store, pause: bool) -> Result<Step> {
        let model_id = &self.config.model_id;
        let top = match store.semantic_state("top")? {
            Some(top) => top,
            None => {
                // First run: what is stored now is the history.
                let max = store.max_seq()?;
                store.set_semantic_state("top", max)?;
                store.set_semantic_state("below", max.saturating_add(1))?;
                store.drop_other_embeddings(model_id)?;
                max
            }
        };
        let mut todo = store.embedding_todo(model_id, Some(top), None, BATCH)?;
        let new = !todo.is_empty();
        let below = store.semantic_state("below")?.unwrap_or(0);
        let backfilling = store.semantic_state("done")?.is_none();
        if todo.is_empty() {
            if pause || !backfilling {
                // Rows above `top` that are not to embed: move past them.
                let max = store.max_seq()?;
                if max > top {
                    store.set_semantic_state("top", max)?;
                }
                return Ok(Step::default());
            }
            todo = store.embedding_todo(model_id, None, Some(below), BATCH)?;
            if todo.is_empty() {
                store.set_semantic_state("done", 1)?;
                return Ok(Step {
                    embedded: 0,
                    finished: true,
                });
            }
        }
        let Some(model) = self.model() else {
            return Ok(Step::default());
        };
        if backfilling && let Ok(mut started) = self.started.lock() {
            started.get_or_insert_with(Instant::now);
        }
        let mut embedded = 0;
        for (seq, title, text) in &todo {
            let vector = model.passage(title, text);
            if store.put_embedding(*seq, model_id, &super::to_blob(&vector))? {
                embedded += 1;
            }
        }
        let last = todo.last().map_or(0, |t| t.0);
        if new {
            store.set_semantic_state("top", last)?;
        } else {
            store.set_semantic_state("below", last)?;
        }
        Ok(Step {
            embedded,
            finished: false,
        })
    }

    /// The `semantic` value of `status` (stable; see `docs/agent.md`):
    /// `{"state": "ready" | "indexing" | "off", "embedded": N,
    /// "total": N, "detail": "…"}`.
    pub fn status(&self, store: &Store) -> Value {
        let (embedded, total) = store
            .embedding_counts(&self.config.model_id)
            .unwrap_or((0, 0));
        let off = self.off.lock().ok().and_then(|o| o.clone());
        let have_file = self.config.model_file.exists();
        if !have_file || off.is_some() {
            let detail = off.unwrap_or_else(|| "model not downloaded".into());
            return json!({"state": "off", "embedded": embedded, "total": total,
                          "detail": detail});
        }
        let done = store.semantic_state("done").ok().flatten().is_some();
        let state = if done && embedded >= total {
            "ready"
        } else {
            "indexing"
        };
        json!({"state": state, "embedded": embedded, "total": total})
    }

    /// The worker: on its own thread, at background priority, until the
    /// node is gone or [`Semantic::stop`].
    pub fn spawn_worker(self: &Arc<Self>, store: Weak<Store>) -> std::io::Result<()> {
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("semantic".into())
            .spawn(move || {
                if !wats_sys::background_priority() {
                    debug!("semantic: could not lower the worker's priority");
                }
                me.work(&store);
            })
            .map(|_| ())
    }

    fn work(&self, store: &Weak<Store>) {
        while !self.stop.load(Ordering::SeqCst) {
            let Some(store) = store.upgrade() else { return };
            if !self.config.model_file.exists() {
                drop(store);
                std::thread::sleep(POLL * 5);
                continue;
            }
            let pause = self.config.laptop_manners && busy_or_on_battery();
            let started = Instant::now();
            let step = self.step(&store, pause);
            let embedded = store
                .embedding_counts(&self.config.model_id)
                .map_or(0, |(done, _)| done);
            drop(store);
            match step {
                Ok(Step { finished: true, .. }) => {
                    // Only a backfill this process ran says so (not one a
                    // previous run finished).
                    let took = self.started.lock().ok().and_then(|s| *s);
                    if let Some(took) = took {
                        info!(
                            target: crate::brief::TARGET,
                            "semantic index ready: {embedded} posts ({})",
                            crate::awake::span(took.elapsed().as_secs())
                        );
                    }
                }
                Ok(Step { embedded: 0, .. }) => {
                    self.unload_if_idle();
                    std::thread::sleep(POLL);
                }
                // Half the time at most: rest as long as it worked.
                Ok(_) => std::thread::sleep(started.elapsed()),
                Err(e) => {
                    warn!("semantic: {e:#}");
                    std::thread::sleep(POLL * 15);
                }
            }
        }
    }

    /// Downloads the model file if it is not there (and may be), retrying
    /// at 1 h, 6 h, then daily, until it is there or the node is gone.
    pub async fn ensure_model(self: Arc<Self>) {
        let Some(base) = self.config.download.clone() else {
            if !self.config.model_file.exists() {
                self.set_off(Some(
                    "model not downloaded (no download without infrastructure)".into(),
                ));
            }
            return;
        };
        let mut failures = 0usize;
        while !self.config.model_file.exists() && !self.stop.load(Ordering::SeqCst) {
            match download(&base, &self.config.model_file).await {
                Ok(()) => {
                    info!("semantic: model downloaded ({MODEL_FILE})");
                    self.set_off(None);
                    return;
                }
                Err(e) => {
                    let wait = DOWNLOAD_RETRY[failures.min(DOWNLOAD_RETRY.len() - 1)];
                    failures += 1;
                    warn!(
                        "semantic: model download failed, search stays full-text; retry in {}: {e:#}",
                        crate::awake::span(wait.as_secs())
                    );
                    self.set_off(Some(format!("model not downloaded ({e})")));
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

/// One line for `wats status`: "index 12,400 of 15,000 posts", "ready
/// (15,000 posts)", "off: model not downloaded".
pub fn summary(semantic: &Value) -> String {
    let n = |key: &str| thousands(semantic[key].as_u64().unwrap_or(0));
    match semantic["state"].as_str() {
        Some("ready") => format!("ready ({} posts)", n("embedded")),
        Some("indexing") => format!("index {} of {} posts", n("embedded"), n("total")),
        Some("disabled") => "off (semantic.enabled = false)".into(),
        _ => format!(
            "off: {}",
            semantic["detail"]
                .as_str()
                .unwrap_or("model not downloaded")
        ),
    }
}

/// 15000 → "15,000".
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// On battery, or the load average above twice the number of cores (the
/// worker already runs at background priority, half the time: only a
/// machine that is really busy pauses it).
fn busy_or_on_battery() -> bool {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    wats_sys::load_average().is_some_and(|load| load > 2.0 * cores) || on_battery()
}

/// Whether the machine runs on battery: `pmset -g batt` on macOS,
/// `/sys/class/power_supply` on Linux. Unknown is not on battery.
fn on_battery() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("pmset")
            .args(["-g", "batt"])
            .output()
            .ok()
            .is_some_and(|out| String::from_utf8_lossy(&out.stdout).contains("'Battery Power'"))
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else {
            return false;
        };
        let mut mains = None;
        for entry in dir.flatten() {
            let path = entry.path();
            let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
            if kind.trim() == "Mains" {
                let online = std::fs::read_to_string(path.join("online")).unwrap_or_default();
                mains = Some(mains.unwrap_or(false) || online.trim() == "1");
            }
        }
        mains == Some(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// Downloads the model file from `base` (a directory URL) to `dest`,
/// checked against the pinned SHA-256 and its minisign signature by the
/// release key, then moved into place.
pub async fn download(base: &str, dest: &std::path::Path) -> Result<()> {
    let client = crate::update::https_client()?;
    let get = |url: String, limit: u64| {
        let client = client.clone();
        async move {
            let mut response = client
                .get(&url)
                .send()
                .await
                .map_err(|e| anyhow!("cannot reach {url}: {e}"))?;
            ensure!(
                response.status().is_success(),
                "{url} answered HTTP {}",
                response.status().as_u16()
            );
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| anyhow!("download {url}: {e}"))?
            {
                ensure!(
                    (body.len() + chunk.len()) as u64 <= limit,
                    "{url} is larger than expected"
                );
                body.extend_from_slice(&chunk);
            }
            Ok::<_, anyhow::Error>(body)
        }
    };
    let base = base.trim_end_matches('/');
    let data = get(format!("{base}/{MODEL_FILE}"), MODEL_BYTES).await?;
    let sha = hex::encode(Sha256::digest(&data));
    ensure!(
        sha == MODEL_SHA256,
        "{MODEL_FILE}: SHA-256 {sha}, not the pinned one"
    );
    let signature = get(format!("{base}/{MODEL_FILE}.minisig"), 4096).await?;
    crate::update::verify_signature(
        crate::update::RELEASE_PUBLIC_KEY,
        &data,
        std::str::from_utf8(&signature).context("signature is not text")?,
        MODEL_FILE,
    )?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = dest.with_extension("part");
    std::fs::write(&tmp, &data)?;
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Kind;

    /// Words to a vector: one dimension per word (hashed), normalised.
    struct Fake;

    fn bag(text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; super::super::DIM];
        for word in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
            if !word.is_empty() {
                let h = word
                    .bytes()
                    .fold(7usize, |h, b| h.wrapping_mul(31).wrapping_add(b as usize));
                v[h % super::super::DIM] += 1.0;
            }
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        v.iter_mut().for_each(|x| *x /= n);
        v
    }

    impl Embed for Fake {
        fn passage(&self, title: &str, text: &str) -> Vec<f32> {
            bag(&format!("{title} {text}"))
        }
        fn query(&self, query: &str) -> Vec<f32> {
            bag(query)
        }
    }

    fn semantic(dir: &std::path::Path) -> Semantic {
        std::fs::write(dir.join(MODEL_FILE), b"fake").unwrap();
        let mut config = Config::new(dir.to_path_buf(), false);
        config.laptop_manners = false;
        Semantic::new(config, Box::new(|_| Ok(Arc::new(Fake) as Arc<dyn Embed>)))
    }

    fn event(key: u8, ts: u64, text: &str) -> crate::event::Event {
        crate::event::Draft {
            net: "public".into(),
            kind: Some(Kind::Ask),
            title: Some(text.into()),
            text: text.into(),
            ..Default::default()
        }
        .sign(&ed25519_dalek::SigningKey::from_bytes(&[key; 32]), ts, 0)
        .unwrap()
    }

    fn ask(store: &Store, key: u8, ts: u64, text: &str) -> i64 {
        let e = event(key, ts, text);
        store.insert(&e, ts).unwrap();
        store.get(&e.id).unwrap().unwrap().seq
    }

    #[test]
    fn backfills_newest_first_then_new_posts_first() {
        let dir = tempfile::tempdir().unwrap();
        let s = semantic(dir.path());
        let store = Store::open_in_memory().unwrap();
        let old: Vec<i64> = (0..40)
            .map(|i| ask(&store, 1, 100 + i, &format!("history post number {i}")))
            .collect();
        assert_eq!(s.status(&store)["state"], "indexing");
        // The first batch: the newest of the history.
        let step = s.step(&store, false).unwrap();
        assert_eq!(step.embedded, BATCH);
        assert_eq!(store.embedding_counts(MODEL_SHA256).unwrap(), (16, 40));
        assert!(
            store
                .embedding_todo(MODEL_SHA256, None, Some(i64::MAX), 100)
                .unwrap()
                .iter()
                .all(|t| t.0 <= old[23])
        );
        // A new post goes ahead of the rest of the history.
        let new = ask(&store, 2, 500, "a brand new question about sorting");
        assert_eq!(s.step(&store, false).unwrap().embedded, 1);
        assert!(
            store
                .embedding_todo(MODEL_SHA256, None, Some(i64::MAX), 100)
                .unwrap()
                .iter()
                .all(|t| t.0 != new)
        );
        // Paused (battery): new posts only, the history waits.
        assert_eq!(s.step(&store, true).unwrap(), Step::default());
        while !s.step(&store, false).unwrap().finished {}
        assert_eq!(store.embedding_counts(MODEL_SHA256).unwrap(), (41, 41));
        assert_eq!(s.status(&store)["state"], "ready");
        // Search by meaning finds the post; the stand-in has no noise, so
        // only the exact bag stands out.
        let hits = s
            .hits(&store, "a brand new question about sorting", 10)
            .unwrap();
        assert_eq!(hits.first().map(|h| h.seq), Some(new));
        // A restart resumes: nothing left to do.
        let again = semantic(dir.path());
        assert_eq!(again.step(&store, false).unwrap(), Step::default());
    }

    #[test]
    fn status_lines() {
        assert_eq!(
            summary(&json!({"state": "indexing", "embedded": 12400, "total": 15000})),
            "index 12,400 of 15,000 posts"
        );
        assert_eq!(
            summary(&json!({"state": "ready", "embedded": 15, "total": 15})),
            "ready (15 posts)"
        );
        assert_eq!(
            summary(&json!({"state": "off", "detail": "model not downloaded"})),
            "off: model not downloaded"
        );
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn without_the_model_it_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let s = Semantic::new(Config::new(dir.path().to_path_buf(), false), model_loader());
        let store = Store::open_in_memory().unwrap();
        ask(&store, 1, 100, "a question");
        assert_eq!(s.status(&store)["state"], "off");
        assert_eq!(s.status(&store)["detail"], "model not downloaded");
        assert!(s.hits(&store, "question", 10).is_none());
        assert_eq!(s.step(&store, false).unwrap(), Step::default());
        // A file that is not the pinned model is refused and removed.
        std::fs::write(dir.path().join(MODEL_FILE), b"not the model").unwrap();
        assert!(s.model().is_none());
        assert!(!dir.path().join(MODEL_FILE).exists());
        assert_eq!(s.status(&store)["state"], "off");
    }

    #[test]
    fn deleted_vectors_leave_memory() {
        let dir = tempfile::tempdir().unwrap();
        let s = semantic(dir.path());
        let store = Store::open_in_memory().unwrap();
        let e = event(1, 100, "secret plans for the launch");
        store.insert(&e, 100).unwrap();
        for i in 0..30 {
            ask(
                &store,
                2,
                101 + i,
                &format!("unrelated weather question {i}"),
            );
        }
        while !s.step(&store, false).unwrap().finished {}
        let hits = s.hits(&store, "secret plans for the launch", 10).unwrap();
        assert_eq!(hits.len(), 1);
        store
            .drop_body(&e.id, crate::store::DropReason::Own, None, None, 200)
            .unwrap();
        let hits = s.hits(&store, "secret plans for the launch", 10).unwrap();
        assert!(hits.is_empty(), "{hits:?}");
    }
}
