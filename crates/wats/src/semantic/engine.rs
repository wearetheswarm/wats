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
    "https://github.com/wearetheswarm/wats/releases/download/model-bge-small-en-v1.5-int8/";
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

    /// Several posts at once (a batch of the backfill).
    fn passages(&self, posts: &[(&str, &str)]) -> Vec<Vec<f32>> {
        posts.iter().map(|(t, x)| self.passage(t, x)).collect()
    }
}

impl Embed for Model {
    fn passage(&self, title: &str, text: &str) -> Vec<f32> {
        Model::passage(self, title, text)
    }

    fn passages(&self, posts: &[(&str, &str)]) -> Vec<Vec<f32>> {
        Model::passages(self, posts)
    }

    fn query(&self, query: &str) -> Vec<f32> {
        Model::query(self, query)
    }
}

/// Loads a model from its file, checked against an expected SHA-256.
pub type Loader = Box<dyn Fn(&std::path::Path, &str) -> Result<Arc<dyn Embed>> + Send + Sync>;

/// The real loader.
pub fn model_loader() -> Loader {
    Box::new(|path, sha| {
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        Ok(Arc::new(Model::load_verified(&bytes, sha)?) as Arc<dyn Embed>)
    })
}

/// A model of an earlier release, and where its file would be.
#[derive(Debug, Clone)]
pub struct Previous {
    pub id: String,
    pub file: PathBuf,
}

/// How a node runs semantic search.
#[derive(Debug, Clone)]
pub struct Config {
    /// The pinned model's file.
    pub model_file: PathBuf,
    /// The pinned model's identity in the store: its file's SHA-256.
    pub model_id: String,
    /// Earlier models that may serve while the pinned one builds its
    /// index ([`super::PREVIOUS_MODELS`], in the same directory).
    pub previous: Vec<Previous>,
    /// Where to download the file from (a directory URL), or `None`
    /// (hermetic, no infrastructure): it must be there already.
    pub download: Option<String>,
    /// Pause the backfill on battery and under load.
    pub laptop_manners: bool,
}

impl Config {
    /// The model files in `dir` (`<data dir>/models`, or
    /// `semantic.model_dir`).
    pub fn new(dir: PathBuf, download: bool) -> Self {
        Config {
            model_file: dir.join(MODEL_FILE),
            model_id: MODEL_SHA256.to_string(),
            previous: super::PREVIOUS_MODELS
                .iter()
                .map(|(id, file)| Previous {
                    id: (*id).to_string(),
                    file: dir.join(file),
                })
                .collect(),
            download: download.then(|| MODEL_URL.to_string()),
            laptop_manners: true,
        }
    }

    /// The file of model `id`: the pinned one's, or a previous one's.
    fn file_of(&self, id: &str) -> Option<&PathBuf> {
        if id == self.model_id {
            return Some(&self.model_file);
        }
        self.previous.iter().find(|p| p.id == id).map(|p| &p.file)
    }
}

#[derive(Default)]
struct Vectors {
    /// The model these vectors are of.
    model: String,
    index: Index,
    /// The table's count and highest `seq` when loaded.
    count: usize,
    max: i64,
}

/// The loaded models by id, each with when it was last used.
type Loaded = std::collections::HashMap<String, (Arc<dyn Embed>, Instant)>;

/// Free bytes under `storage.max` (`None`: no limit known).
pub type FreeBytes = Box<dyn Fn() -> Option<u64> + Send + Sync>;

/// Bytes a vector takes on disk, with its row.
const VECTOR_BYTES: u64 = (super::DIM as u64) * 2 + 64;

/// Semantic search's state on a node.
pub struct Semantic {
    config: Config,
    loader: Loader,
    /// Loaded models by id: the serving one and, while a new model builds
    /// its index, the pinned one too.
    models: Mutex<Loaded>,
    vectors: Mutex<Vectors>,
    /// Why there are no vectors (e.g. the model is not downloaded).
    off: Mutex<Option<String>>,
    stop: AtomicBool,
    /// When the backfill started, for its "ready" line.
    started: Mutex<Option<Instant>>,
    /// A download in progress: bytes so far, and the file's size.
    downloading: Mutex<Option<(u64, u64)>>,
    /// Free bytes under `storage.max`: whether two indexes fit while a new
    /// model builds its own.
    free_bytes: Mutex<FreeBytes>,
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
            models: Mutex::default(),
            vectors: Mutex::default(),
            off: Mutex::new(None),
            stop: AtomicBool::new(false),
            started: Mutex::new(None),
            downloading: Mutex::new(None),
            free_bytes: Mutex::new(Box::new(|| None)),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// How to read the free bytes under `storage.max` (the node, once it
    /// is up).
    pub fn set_free_bytes(&self, free: FreeBytes) {
        if let Ok(mut f) = self.free_bytes.lock() {
            *f = free;
        }
    }

    fn set_off(&self, why: Option<String>) {
        if let Ok(mut off) = self.off.lock() {
            *off = why;
        }
    }

    /// The pinned model, loaded if its file is there.
    pub fn model(&self) -> Option<Arc<dyn Embed>> {
        self.load(&self.config.model_id.clone())
    }

    /// Model `id` (pinned or previous), loaded if its file is there and
    /// is that model. A pinned file that does not load is removed, to be
    /// downloaded again.
    fn load(&self, id: &str) -> Option<Arc<dyn Embed>> {
        let mut models = self.models.lock().ok()?;
        if let Some((model, used)) = models.get_mut(id) {
            *used = Instant::now();
            return Some(Arc::clone(model));
        }
        let file = self.config.file_of(id)?.clone();
        if !file.exists() {
            return None;
        }
        let started = Instant::now();
        match (self.loader)(&file, id) {
            Ok(model) => {
                debug!(
                    "semantic: model {} loaded in {:?}",
                    short(id),
                    started.elapsed()
                );
                models.insert(id.to_string(), (Arc::clone(&model), Instant::now()));
                if id == self.config.model_id {
                    self.set_off(None);
                }
                Some(model)
            }
            Err(e) => {
                warn!(
                    "semantic: the model file {} does not load: {e:#}",
                    file.display()
                );
                if id == self.config.model_id {
                    let _ = std::fs::remove_file(&file);
                    self.set_off(Some(format!("model file refused ({e})")));
                }
                None
            }
        }
    }

    /// Unloads the models unused for [`IDLE_UNLOAD`]; the vectors in memory
    /// go with the one they are of.
    pub fn unload_if_idle(&self) {
        let Ok(mut models) = self.models.lock() else {
            return;
        };
        let idle: Vec<String> = models
            .iter()
            .filter(|(_, (_, used))| used.elapsed() >= IDLE_UNLOAD)
            .map(|(id, _)| id.clone())
            .collect();
        for id in idle {
            models.remove(&id);
            if let Ok(mut v) = self.vectors.lock()
                && v.model == id
            {
                *v = Vectors::default();
            }
            debug!("semantic: model {} unloaded (idle)", short(&id));
        }
    }

    /// The model whose vectors answer queries: the pinned one, or a
    /// previous one while the pinned one builds its index.
    pub fn serving(&self, store: &Store) -> String {
        self.config
            .previous
            .iter()
            .find(|p| {
                store
                    .semantic_state(&format!("serving:{}", p.id))
                    .ok()
                    .flatten()
                    .is_some()
            })
            .map_or_else(|| self.config.model_id.clone(), |p| p.id.clone())
    }

    /// Brings the vectors in memory up to the table for `model`: the new
    /// rows, or all of them again when some were deleted.
    fn refresh(&self, store: &Store, model: &str, vectors: &mut Vectors) -> Result<()> {
        if vectors.model != model {
            *vectors = Vectors {
                model: model.to_string(),
                ..Vectors::default()
            };
        }
        let (count, max) = store.embeddings_extent(model)?;
        if (count, max) == (vectors.count, vectors.max) {
            return Ok(());
        }
        let fresh = || Vectors {
            model: model.to_string(),
            ..Vectors::default()
        };
        if count < vectors.count || max < vectors.max {
            *vectors = fresh();
        }
        for (seq, blob) in store.embeddings_after(model, vectors.max)? {
            vectors.index.push(seq, &blob);
        }
        if vectors.index.len() != count {
            // Rows deleted and others added: load them all again.
            *vectors = fresh();
            for (seq, blob) in store.embeddings_after(model, 0)? {
                vectors.index.push(seq, &blob);
            }
        }
        (vectors.count, vectors.max) = (count, max);
        Ok(())
    }

    /// The posts that match `query` by meaning, gated (ADR 0053 §2), at
    /// most `k`: `None` when semantic search cannot run (no model, no
    /// vector), and full-text search alone applies. With the serving
    /// model and its vectors: during a model change, the previous model's
    /// complete index, then the new one's.
    pub fn hits(&self, store: &Store, query: &str, k: usize) -> Option<Vec<VectorHit>> {
        let serving = self.serving(store);
        let (id, model) = match self.load(&serving) {
            Some(model) => (serving, model),
            // The previous model's file is gone: the new index, as far as
            // it goes.
            None => (self.config.model_id.clone(), self.model()?),
        };
        let embedding = model.query(query);
        let mut vectors = self.vectors.lock().ok()?;
        if let Err(e) = self.refresh(store, &id, &mut vectors) {
            warn!("semantic: reading the vectors: {e:#}");
            return None;
        }
        if vectors.index.is_empty() {
            return None;
        }
        // The background is the pinned model's: a previous model gates
        // against its own posts only.
        let background: Vec<f32> = if id == self.config.model_id {
            super::background()
                .scores(&embedding)
                .into_iter()
                .map(|(_, c)| c)
                .collect()
        } else {
            Vec::new()
        };
        Some(rank::gate(
            &vectors.index.scores(&embedding),
            &background,
            k,
        ))
    }

    /// A state key of the pinned model: `<name>:<model sha256>`. An older
    /// wats (0.5.0, which reads `top`) finds none of them after a rollback,
    /// and reindexes with its own model.
    fn key(&self, name: &str) -> String {
        format!("{name}:{}", self.config.model_id)
    }

    /// Whether a second index fits under `storage.max`, beside the one
    /// serving.
    fn room_for_two(&self, store: &Store) -> bool {
        let (_, total) = store
            .embedding_counts(&self.config.model_id)
            .unwrap_or((0, 0));
        let need = total.saturating_mul(VECTOR_BYTES);
        let free = self.free_bytes.lock().ok().and_then(|f| f());
        free.is_none_or(|free| free >= need)
    }

    /// Removes, from the model directory, every other `*.safetensors` file
    /// whose SHA-256 is not the pinned one's: once the pinned file is
    /// there and verified, the index is the pinned model's, and no
    /// download runs. A shared `semantic.model_dir` too (Vincent,
    /// 2026-10-26). Never the pinned file, never a `.part`.
    pub fn remove_stale_models(&self, store: &Store) {
        if self.serving(store) != self.config.model_id
            || self.downloading.lock().ok().and_then(|d| *d).is_some()
        {
            return;
        }
        let pinned = &self.config.model_file;
        let verified = std::fs::read(pinned)
            .ok()
            .is_some_and(|bytes| hex::encode(Sha256::digest(&bytes)) == self.config.model_id);
        let Some(dir) = pinned.parent().filter(|_| verified) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_model = path.extension().is_some_and(|e| e == "safetensors");
            if !is_model || &path == pinned || !path.is_file() {
                continue;
            }
            let other = std::fs::read(&path)
                .ok()
                .is_none_or(|bytes| hex::encode(Sha256::digest(&bytes)) != self.config.model_id);
            if other && std::fs::remove_file(&path).is_ok() {
                info!(
                    "semantic: removed {}, a model file this wats no longer uses",
                    path.display()
                );
            }
        }
    }

    /// One batch: new posts first, then the history (unless `pause`).
    pub fn step(&self, store: &Store, pause: bool) -> Result<Step> {
        let model_id = &self.config.model_id;
        let this_model = format!("model:{model_id}");
        // 0.5.1 kept `top`, `below` and `done` unprefixed, which 0.5.0
        // reads after a rollback as its own (and skips its reindex): they
        // move under this model's name. And it served its own model.
        if store.semantic_state(&this_model)?.is_some() {
            for name in ["top", "below", "done"] {
                if let Some(v) = store.semantic_state(name)? {
                    store.set_semantic_state(&self.key(name), v)?;
                    store.delete_semantic_state(name)?;
                }
            }
        }
        // An index 0.5.1 completed serves: recorded as such.
        if store.semantic_state(&self.key("done"))?.is_some() {
            store.set_semantic_state(&format!("serving:{model_id}"), 1)?;
        }
        // A model this index was not built with (ADR 0053 §6). A previous
        // model with a complete index keeps serving while this one builds
        // its own beside it, room allowing; otherwise its vectors go now
        // and this one starts from nothing.
        if store.semantic_state(&this_model)?.is_none() {
            let serving = self.serving(store);
            let keep = serving != *model_id
                && self.config.file_of(&serving).is_some_and(|f| f.exists())
                && self.room_for_two(store);
            if keep {
                info!("semantic: a new model; the previous one serves until its index is built");
            } else {
                let dropped = store.drop_other_embeddings(model_id)?;
                if dropped > 0 {
                    info!(
                        "semantic: a new model; {dropped} vectors of the previous one dropped, \
                         reindexing{}",
                        if serving != *model_id {
                            " (no room under storage.max for two indexes, or its file is gone)"
                        } else {
                            ""
                        }
                    );
                }
                store.clear_semantic_state()?;
            }
            let max = store.max_seq()?;
            store.set_semantic_state(&self.key("top"), max)?;
            store.set_semantic_state(&self.key("below"), max.saturating_add(1))?;
            store.delete_semantic_state(&self.key("done"))?;
            store.set_semantic_state(&this_model, 1)?;
            if let Ok(mut started) = self.started.lock() {
                *started = None;
            }
        }
        let top = store.semantic_state(&self.key("top"))?.unwrap_or(0);
        let mut todo = store.embedding_todo(model_id, Some(top), None, BATCH)?;
        let new = !todo.is_empty();
        let below = store.semantic_state(&self.key("below"))?.unwrap_or(0);
        let backfilling = store.semantic_state(&self.key("done"))?.is_none();
        if todo.is_empty() {
            if pause || !backfilling {
                // Rows above `top` that are not to embed: move past them.
                let max = store.max_seq()?;
                if max > top {
                    store.set_semantic_state(&self.key("top"), max)?;
                }
                return Ok(Step::default());
            }
            todo = store.embedding_todo(model_id, None, Some(below), BATCH)?;
            if todo.is_empty() {
                store.set_semantic_state(&self.key("done"), 1)?;
                self.switch(store)?;
                // Recorded, so that a release with another model lets this
                // one serve while it builds its own index.
                store.set_semantic_state(&format!("serving:{model_id}"), 1)?;
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
        let posts: Vec<(&str, &str)> = todo
            .iter()
            .map(|(_, title, text)| (title.as_str(), text.as_str()))
            .collect();
        for ((seq, _, _), vector) in todo.iter().zip(model.passages(&posts)) {
            if store.put_embedding(*seq, model_id, &super::to_blob(&vector))? {
                embedded += 1;
            }
        }
        let last = todo.last().map_or(0, |t| t.0);
        if new {
            store.set_semantic_state(&self.key("top"), last)?;
        } else {
            store.set_semantic_state(&self.key("below"), last)?;
        }
        Ok(Step {
            embedded,
            finished: false,
        })
    }

    /// The pinned model's index is complete: it serves from now on, and
    /// the previous model's vectors and file go.
    fn switch(&self, store: &Store) -> Result<()> {
        let serving = self.serving(store);
        if serving == self.config.model_id {
            return Ok(());
        }
        for key in [
            format!("serving:{serving}"),
            format!("model:{serving}"),
            format!("top:{serving}"),
            format!("below:{serving}"),
            format!("done:{serving}"),
        ] {
            store.delete_semantic_state(&key)?;
        }
        let dropped = store.drop_other_embeddings(&self.config.model_id)?;
        if let Ok(mut models) = self.models.lock() {
            models.remove(&serving);
        }
        info!(
            "semantic: switched to {} ({dropped} vectors of the previous model dropped)",
            MODEL_FILE
        );
        self.remove_stale_models(store);
        Ok(())
    }

    /// The `semantic` value of `status` (stable; see `docs/agent.md`):
    /// `{"state": "ready" | "indexing" | "off" | "downloading",
    /// "embedded": N, "total": N, "detail": "…"}`, and during a model
    /// change `"next": {"embedded": N, "total": N}`, the new model's index.
    pub fn status(&self, store: &Store) -> Value {
        let (embedded, total) = store
            .embedding_counts(&self.config.model_id)
            .unwrap_or((0, 0));
        let serving = self.serving(store);
        if serving != self.config.model_id
            && self.config.file_of(&serving).is_some_and(|f| f.exists())
        {
            let (old, _) = store.embedding_counts(&serving).unwrap_or((0, 0));
            return json!({"state": "ready", "embedded": old, "total": total,
                          "model": "previous",
                          "next": {"embedded": embedded, "total": total}});
        }
        let off = self.off.lock().ok().and_then(|o| o.clone());
        let have_file = self.config.model_file.exists();
        let downloading = self.downloading.lock().ok().and_then(|d| *d);
        if let (false, Some((got, size))) = (have_file, downloading) {
            return json!({"state": "downloading", "embedded": embedded, "total": total,
                          "downloaded": got, "size": size,
                          "detail": format!("downloading model ({} MB of {})", got / 1_000_000,
                                            size.div_ceil(1_000_000))});
        }
        if !have_file || off.is_some() {
            let detail = off.unwrap_or_else(|| "model not downloaded".into());
            return json!({"state": "off", "embedded": embedded, "total": total,
                          "detail": detail});
        }
        let done = store
            .semantic_state(&self.key("done"))
            .ok()
            .flatten()
            .is_some();
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
        let mut cleaned = false;
        while !self.stop.load(Ordering::SeqCst) {
            let Some(store) = store.upgrade() else { return };
            if !self.config.model_file.exists() {
                drop(store);
                std::thread::sleep(POLL * 5);
                continue;
            }
            // Once per start, the pinned file there: other models' files go
            // (after a switch, `switch` does it).
            if !cleaned {
                self.remove_stale_models(&store);
                cleaned = true;
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
            let progress = |got: u64| {
                if let Ok(mut d) = self.downloading.lock() {
                    *d = Some((got, MODEL_BYTES));
                }
            };
            progress(0);
            let outcome = download(&base, &self.config.model_file, &progress).await;
            if let Ok(mut d) = self.downloading.lock() {
                *d = None;
            }
            match outcome {
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

/// The start of an id, for logs.
fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// One line for `wats status`: "index 12,400 of 15,000 posts", "ready
/// (15,000 posts)", "off: model not downloaded".
pub fn summary(semantic: &Value) -> String {
    let n = |key: &str| thousands(semantic[key].as_u64().unwrap_or(0));
    match semantic["state"].as_str() {
        Some("ready") if semantic.get("next").is_some() => format!(
            "ready (previous model) · new model: index {} of {}",
            thousands(semantic["next"]["embedded"].as_u64().unwrap_or(0)),
            thousands(semantic["next"]["total"].as_u64().unwrap_or(0))
        ),
        Some("ready") => format!("ready ({} posts)", n("embedded")),
        Some("indexing") => format!("index {} of {} posts", n("embedded"), n("total")),
        Some("downloading") => semantic["detail"]
            .as_str()
            .unwrap_or("downloading model")
            .to_string(),
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
pub async fn download(
    base: &str,
    dest: &std::path::Path,
    progress: &(dyn Fn(u64) + Sync),
) -> Result<()> {
    let client = crate::update::https_client()?;
    let get = |url: String, limit: u64, report: bool| {
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
                if report {
                    progress(body.len() as u64);
                }
            }
            Ok::<_, anyhow::Error>(body)
        }
    };
    let base = base.trim_end_matches('/');
    let data = get(format!("{base}/{MODEL_FILE}"), MODEL_BYTES, true).await?;
    let sha = hex::encode(Sha256::digest(&data));
    ensure!(
        sha == MODEL_SHA256,
        "{MODEL_FILE}: SHA-256 {sha}, not the pinned one"
    );
    let signature = get(format!("{base}/{MODEL_FILE}.minisig"), 4096, false).await?;
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
        Semantic::new(
            config,
            Box::new(|_, _| Ok(Arc::new(Fake) as Arc<dyn Embed>)),
        )
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

    /// The published release (network): downloaded, checked against the
    /// pinned SHA-256 and the release key's signature, and it loads.
    #[test]
    #[ignore = "downloads 34 MB from the model release"]
    fn downloads_the_published_model() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(MODEL_FILE);
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(download(MODEL_URL, &dest, &|_| {}))
            .unwrap();
        let model = model_loader()(&dest, MODEL_SHA256).unwrap();
        assert_eq!(model.query("hello").len(), super::super::DIM);
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

    /// A finished index, then another model: its vectors go, and the
    /// history is embedded again, newest first.
    #[test]
    fn a_new_model_reindexes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let seqs: Vec<i64> = (0..20)
            .map(|i| ask(&store, 1, 100 + i, &format!("post number {i}")))
            .collect();
        let old = semantic(dir.path());
        while !old.step(&store, false).unwrap().finished {}
        assert_eq!(store.embedding_counts(MODEL_SHA256).unwrap(), (20, 20));
        let mut config = old.config().clone();
        config.model_id = "another-model".into();
        let new = Semantic::new(
            config,
            Box::new(|_, _| Ok(Arc::new(Fake) as Arc<dyn Embed>)),
        );
        assert_eq!(new.status(&store)["state"], "indexing");
        let step = new.step(&store, false).unwrap();
        assert_eq!(step.embedded, BATCH);
        // The previous model's vectors are gone.
        assert_eq!(store.embedding_counts(MODEL_SHA256).unwrap().0, 0);
        // Newest first.
        let left = store
            .embedding_todo("another-model", None, Some(i64::MAX), 100)
            .unwrap();
        assert!(left.iter().all(|t| t.0 <= seqs[3]), "{left:?}");
        while !new.step(&store, false).unwrap().finished {}
        assert_eq!(store.embedding_counts("another-model").unwrap(), (20, 20));
        assert_eq!(new.status(&store)["state"], "ready");
        // Back on the same model: nothing to do again.
        let again = Semantic::new(
            new.config().clone(),
            Box::new(|_, _| Ok(Arc::new(Fake) as Arc<dyn Embed>)),
        );
        assert_eq!(again.step(&store, false).unwrap(), Step::default());
    }

    /// 0.5.1's unprefixed keys move under the model's name, with no
    /// reindex; afterwards no `top` is left for a 0.5.0 to misread.
    #[test]
    fn the_state_moves_under_the_model_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        for i in 0..5 {
            ask(&store, 1, 100 + i, &format!("post {i}"));
        }
        let s = semantic(dir.path());
        while !s.step(&store, false).unwrap().finished {}
        // As 0.5.1 left it: the same model, unprefixed keys.
        let top = store.semantic_state(&s.key("top")).unwrap().unwrap();
        for name in ["top", "below", "done"] {
            let v = store.semantic_state(&s.key(name)).unwrap().unwrap();
            store.set_semantic_state(name, v).unwrap();
            store.delete_semantic_state(&s.key(name)).unwrap();
        }
        let again = semantic(dir.path());
        assert_eq!(again.step(&store, false).unwrap(), Step::default());
        assert_eq!(store.embedding_counts(MODEL_SHA256).unwrap(), (5, 5));
        assert_eq!(store.semantic_state("top").unwrap(), None);
        assert_eq!(store.semantic_state(&again.key("top")).unwrap(), Some(top));
        assert_eq!(again.status(&store)["state"], "ready");
    }

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// A model change with room for two indexes: the previous model keeps
    /// answering with its complete index while the new one builds its
    /// own; then the switch, and the previous vectors and file go.
    #[test]
    fn a_new_model_takes_over_smoothly() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        for i in 0..30 {
            ask(&store, 1, 100 + i, &format!("history post number {i}"));
        }
        let target = ask(&store, 2, 500, "a brand new question about sorting");
        // The previous release: its model, a complete index.
        let old_file = dir.path().join("old-model.safetensors");
        std::fs::write(&old_file, b"old").unwrap();
        let old_id = sha(b"old");
        let mut old_config = Config::new(dir.path().to_path_buf(), false);
        old_config.model_file = old_file.clone();
        old_config.model_id = old_id.clone();
        old_config.laptop_manners = false;
        let fake = || -> Loader { Box::new(|_, _| Ok(Arc::new(Fake) as Arc<dyn Embed>)) };
        let old = Semantic::new(old_config, fake());
        while !old.step(&store, false).unwrap().finished {}
        assert_eq!(store.embedding_counts(&old_id).unwrap(), (31, 31));
        // This release: a new model, the old one among the previous ones.
        let new_file = dir.path().join(MODEL_FILE);
        std::fs::write(&new_file, b"new").unwrap();
        let mut config = Config::new(dir.path().to_path_buf(), false);
        config.model_id = sha(b"new");
        config.previous = vec![Previous {
            id: old_id.clone(),
            file: old_file.clone(),
        }];
        config.laptop_manners = false;
        let new = Semantic::new(config, fake());
        assert_eq!(new.step(&store, false).unwrap().embedded, BATCH);
        // The previous index still answers, whole.
        assert_eq!(store.embedding_counts(&old_id).unwrap(), (31, 31));
        assert_eq!(new.serving(&store), old_id);
        let hits = new
            .hits(&store, "a brand new question about sorting", 5)
            .unwrap();
        assert_eq!(hits.first().map(|h| h.seq), Some(target));
        let status = new.status(&store);
        assert_eq!(status["state"], "ready", "{status}");
        assert_eq!(status["next"]["embedded"], BATCH as u64);
        assert_eq!(
            summary(&status),
            format!("ready (previous model) · new model: index {BATCH} of 31")
        );
        // Interrupted: a restart goes on with the transition.
        let new = Semantic::new(new.config().clone(), fake());
        while !new.step(&store, false).unwrap().finished {}
        // The switch: the new model serves, the old vectors and file go.
        assert_eq!(new.serving(&store), sha(b"new"));
        assert_eq!(store.embedding_counts(&old_id).unwrap().0, 0);
        assert_eq!(store.embedding_counts(&sha(b"new")).unwrap(), (31, 31));
        assert!(!old_file.exists() && new_file.exists());
        assert_eq!(new.status(&store)["state"], "ready");
        assert!(new.status(&store).get("next").is_none());
    }

    /// No room under storage.max for two indexes: the old vectors go at
    /// once, as in 0.5.1, and the new index builds from nothing.
    #[test]
    fn no_room_for_two_indexes_drops_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        for i in 0..10 {
            ask(&store, 1, 100 + i, &format!("post {i}"));
        }
        let old_file = dir.path().join("old-model.safetensors");
        std::fs::write(&old_file, b"old").unwrap();
        let fake = || -> Loader { Box::new(|_, _| Ok(Arc::new(Fake) as Arc<dyn Embed>)) };
        let mut old_config = Config::new(dir.path().to_path_buf(), false);
        old_config.model_file = old_file.clone();
        old_config.model_id = sha(b"old");
        let old = Semantic::new(old_config, fake());
        while !old.step(&store, false).unwrap().finished {}
        let mut config = Config::new(dir.path().to_path_buf(), false);
        config.model_id = "new-model".into();
        config.previous = vec![Previous {
            id: sha(b"old"),
            file: old_file,
        }];
        let new = Semantic::new(config, fake());
        new.set_free_bytes(Box::new(|| Some(100)));
        new.step(&store, false).unwrap();
        assert_eq!(store.embedding_counts(&sha(b"old")).unwrap().0, 0);
        assert_eq!(new.serving(&store), "new-model");
    }

    /// Other `*.safetensors` files go once the pinned file is verified
    /// (a shared model_dir too); never the pinned file, never a `.part`.
    #[test]
    fn stale_model_files_go_once_the_pinned_one_is_verified() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let old = dir.path().join("bge-small-en-v1.5-f16.safetensors");
        let part = dir.path().join("bge-small-en-v1.5-int8.part");
        let other = dir.path().join("notes.txt");
        let pinned = dir.path().join(MODEL_FILE);
        for f in [&old, &part, &other] {
            std::fs::write(f, b"x").unwrap();
        }
        let mut config = Config::new(dir.path().to_path_buf(), false);
        config.model_id = sha(b"pinned");
        let s = Semantic::new(config, model_loader());
        // Not there yet (downloading): nothing goes.
        s.remove_stale_models(&store);
        assert!(old.exists());
        // There, but not the pinned content: nothing goes.
        std::fs::write(&pinned, b"tampered").unwrap();
        s.remove_stale_models(&store);
        assert!(old.exists());
        std::fs::write(&pinned, b"pinned").unwrap();
        s.remove_stale_models(&store);
        assert!(!old.exists() && pinned.exists() && part.exists() && other.exists());
    }

    #[test]
    fn a_download_in_progress_shows() {
        let dir = tempfile::tempdir().unwrap();
        let s = Semantic::new(Config::new(dir.path().to_path_buf(), false), model_loader());
        let store = Store::open_in_memory().unwrap();
        *s.downloading.lock().unwrap() = Some((12_400_000, MODEL_BYTES));
        let status = s.status(&store);
        assert_eq!(status["state"], "downloading");
        assert_eq!(
            summary(&status),
            format!(
                "downloading model (12 MB of {})",
                MODEL_BYTES.div_ceil(1_000_000)
            )
        );
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
