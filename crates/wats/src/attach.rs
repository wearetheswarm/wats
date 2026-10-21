//! Several MCP sessions, one node (SPEC §2.16: the MCP server runs inside
//! the daemon process).
//!
//! The process that owns a data directory (`wats node`, or the first
//! `wats mcp`) listens on `<data dir>/mcp.sock`. Any later `wats mcp` for
//! the same directory attaches to it and relays its stdio, so every agent
//! session on a machine shares one node, one store and one network
//! presence. Each attached session sends a one-line JSON handshake (its
//! profile and the directories it may share files from), then speaks MCP.
//!
//! Unix only for now; on Windows each data directory serves one session.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};

use crate::mcp::WatsMcp;
use crate::node::{Node, is_valid_profile};

const HANDSHAKE_MAX_BYTES: u64 = 64 * 1024;

/// First line sent by an attaching session.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handshake {
    pub wats_attach: u32,
    pub profile: Option<String>,
    pub file_roots: Vec<PathBuf>,
    /// The token of an agent run (`WATS_RUNNER_TOKEN`), set by the runner
    /// in the MCP config it starts the agent with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_token: Option<String>,
    /// The MCP host (`WATS_MCP_CLIENT`), e.g. `openclaw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// Names the MCP host a `wats mcp` session runs under, e.g. `openclaw`
/// (set by `wats setup openclaw --mcp` in its MCP entry): the node then
/// pushes replies to the asks posted from there (ADR 0018 §7).
pub const CLIENT_ENV: &str = "WATS_MCP_CLIENT";

/// [`CLIENT_ENV`], when set and not empty.
pub fn client_from_env() -> Option<String> {
    std::env::var(CLIENT_ENV)
        .ok()
        .filter(|c| !c.trim().is_empty())
}

/// Longest Unix socket path we use (`sun_path` is 104 bytes on macOS,
/// 108 on Linux, including the terminating NUL).
const MAX_SOCKET_PATH: usize = 100;

/// Names the node's socket instead of `<data dir>/mcp.sock`: a socket on a
/// volume shared with another container, which then attaches without the
/// node's data directory (docs/docker.md). With it, `wats mcp` never
/// starts a node of its own.
pub const SOCKET_ENV: &str = "WATS_MCP_SOCKET";

/// [`SOCKET_ENV`], when set and not empty.
pub fn socket_override() -> Option<PathBuf> {
    std::env::var_os(SOCKET_ENV)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Where the node owning `data_dir` listens: [`SOCKET_ENV`] when set, else
/// `<data dir>/mcp.sock`, or, when that path is too long for a Unix socket,
/// `~/.wats/sock/<hash>.sock` with the hash derived from the data directory.
pub fn socket_path(data_dir: &Path) -> PathBuf {
    socket_override().unwrap_or_else(|| default_socket_path(data_dir))
}

fn default_socket_path(data_dir: &Path) -> PathBuf {
    let direct = data_dir.join("mcp.sock");
    if direct.as_os_str().len() <= MAX_SOCKET_PATH {
        return direct;
    }
    let absolute = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let hash = blake3::hash(absolute.as_os_str().as_encoded_bytes()).to_hex();
    let dir = std::env::home_dir()
        .map(|h| h.join(".wats").join("sock"))
        .unwrap_or_else(|| std::env::temp_dir().join("wats-sock"));
    dir.join(format!("{}.sock", &hash[..16]))
}

/// Number of attached sessions currently served.
#[derive(Debug, Clone, Default)]
pub struct Sessions(Arc<AtomicUsize>);

impl Sessions {
    pub fn active(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

struct SessionGuard(Sessions);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        (self.0).0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serve attached MCP sessions until the node shuts down.
#[cfg(unix)]
pub fn serve(node: Arc<Node>) -> Result<Sessions> {
    use std::os::unix::fs::PermissionsExt;

    let path = socket_path(&node.config().data_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).or_else(|e| {
            if dir == node.config().data_dir {
                Ok(())
            } else {
                Err(e)
            }
        })?;
    }
    // We hold the data directory lock, so any existing socket is stale.
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    // Only processes of the user who owns the socket may attach (security
    // review D), on top of the 0600 socket in a 0700 directory.
    let owner = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&path)?.uid()
    };
    let sessions = Sessions::default();
    let counter = sessions.clone();
    let listener = Arc::new(listener);
    let accept_loop = move || {
        let listener = Arc::clone(&listener);
        let node = Arc::clone(&node);
        let counter = counter.clone();
        async move { accept_sessions(&listener, owner, node, counter).await }
    };
    tokio::spawn(async move {
        crate::supervise::supervise("mcp socket accept loop", accept_loop).await;
    });
    Ok(sessions)
}

/// Accept attached MCP sessions on the node's socket, each in its own task,
/// from processes of `owner` only.
#[cfg(unix)]
async fn accept_sessions(
    listener: &tokio::net::UnixListener,
    owner: u32,
    node: Arc<Node>,
    counter: Sessions,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => match stream.peer_cred() {
                Ok(cred) if cred.uid() == owner => stream,
                Ok(cred) => {
                    warn!(
                        uid = cred.uid(),
                        "mcp socket: refused a connection from another user"
                    );
                    continue;
                }
                Err(e) => {
                    warn!("mcp socket: refused a connection without peer credentials: {e}");
                    continue;
                }
            },
            Err(e) => {
                warn!("mcp socket: {e}");
                break;
            }
        };
        let node = Arc::clone(&node);
        counter.0.fetch_add(1, Ordering::SeqCst);
        let guard = SessionGuard(counter.clone());
        let sessions = counter.clone();
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(e) = serve_session(node, stream, sessions).await {
                debug!("attached session ended: {e:#}");
            }
        });
    }
}

#[cfg(unix)]
async fn serve_session(
    node: Arc<Node>,
    stream: tokio::net::UnixStream,
    sessions: Sessions,
) -> Result<()> {
    let (read, write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    (&mut reader)
        .take(HANDSHAKE_MAX_BYTES)
        .read_line(&mut line)
        .await?;
    let first: serde_json::Value = serde_json::from_str(&line).context("bad attach handshake")?;
    if first.get("wats_control").is_some() {
        // This connection is no MCP session.
        let others = sessions.active().saturating_sub(1);
        return serve_control(&node, first, write, others).await;
    }
    let handshake: Handshake = serde_json::from_value(first).context("bad attach handshake")?;
    if handshake.wats_attach != 1 {
        bail!("unsupported attach version {}", handshake.wats_attach);
    }
    let mut server =
        WatsMcp::new(Arc::clone(&node), handshake.file_roots).with_client(handshake.client);
    if let Some(profile) = &handshake.profile {
        server = server.with_profile(profile)?;
    }
    if let Some(run) = handshake
        .runner_token
        .as_deref()
        .and_then(|t| node.runner_run(t))
    {
        server = server.runner_session(run);
    }
    // A node that stops on a guardian halt, or only serves halt events,
    // ends its sessions (ADR 0016 §3).
    let stopped = |o: &crate::node::HaltOutcome| {
        matches!(
            o,
            crate::node::HaltOutcome::Serving | crate::node::HaltOutcome::Exit(_)
        )
    };
    let mut outcome = node.halt_outcome();
    if stopped(&outcome.borrow()) {
        return Ok(());
    }
    let running = server.serve((reader, write)).await?;
    tokio::select! {
        result = running.waiting() => { result?; }
        _ = outcome.wait_for(stopped) => {}
    }
    Ok(())
}

/// A request of a CLI command run by the human at the terminal, sent as
/// the first line instead of a [`Handshake`]: one request, one JSON line
/// back (`{"ok": …}` or `{"error": …}`), no MCP. Agents get no MCP tool
/// for these: dropping a body is a human decision (ADR 0020 §2).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    /// `wats drop`: drop a body on this node, reason `own`.
    Drop { id: String, detail: Option<String> },
    /// A daemon starting on this data directory asks a background node
    /// (started by `wats mcp`) to stop so that it can take over (ADR 0021
    /// §4). Refused by any other node.
    HandOver,
    /// `wats retract`: retract posts of a profile of this node (`None`: the
    /// default one), ADR 0020 §5.1. For the human: no rate limit, no audit
    /// copy (the agents' `retract` tool has both).
    Retract {
        ids: Vec<String>,
        reason: Option<String>,
        profile: Option<String>,
    },
    /// `wats ps`, `wats --detach` and `wats stop` (ADR 0023): who this
    /// node is (pid, role, version, start, log), its MCP sessions, peers,
    /// connectivity and agents.
    Info,
    /// `wats stop` (ADR 0023): stop gracefully, whatever the role. Answers
    /// with [`Control::Info`]'s fields before stopping.
    Stop,
    /// `wats fleet check` (ADR 0026): this node's endpoint id, its gossip
    /// neighbors now, and which of `ids` (endpoint ids) it has heard from
    /// since it started.
    Peers { ids: Vec<String> },
}

/// The answer to [`Control::Info`].
#[cfg(unix)]
fn info(node: &Node, sessions: usize) -> serde_json::Value {
    let config = node.config();
    serde_json::json!({
        "pid": std::process::id(),
        "role": config.role.to_string(),
        "version": env!("CARGO_PKG_VERSION"),
        "profile": config.profile,
        "data_dir": config.data_dir,
        "started": node.started_at(),
        "log": config.log_file,
        "sessions": sessions,
        "peers": node.known_peers().len(),
        "health": node.health_json(),
        "history": node.history_status(),
        "agents": config.agents.iter().filter(|a| a.running).map(|a| a.name.clone()).collect::<Vec<_>>(),
    })
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlLine {
    wats_control: u32,
    request: Control,
}

#[cfg(unix)]
async fn serve_control(
    node: &Arc<Node>,
    first: serde_json::Value,
    mut write: tokio::net::unix::OwnedWriteHalf,
    sessions: usize,
) -> Result<()> {
    use serde_json::json;
    let reply = match serde_json::from_value::<ControlLine>(first) {
        Ok(line) if line.wats_control == 1 => match run_control(node, line.request, sessions).await
        {
            Ok(value) => json!({ "ok": value }),
            Err(e) => json!({ "error": format!("{e:#}") }),
        },
        Ok(line) => {
            json!({ "error": format!("unsupported control version {}", line.wats_control) })
        }
        Err(e) => json!({ "error": format!("bad control request: {e}") }),
    };
    write.write_all(&serde_json::to_vec(&reply)?).await?;
    write.write_all(b"\n").await?;
    write.flush().await?;
    Ok(())
}

#[cfg(unix)]
async fn run_control(
    node: &Arc<Node>,
    request: Control,
    sessions: usize,
) -> Result<serde_json::Value> {
    match request {
        Control::Info => Ok(info(node, sessions)),
        Control::Peers { ids } => {
            let known: std::collections::HashSet<String> =
                node.known_peers().iter().map(ToString::to_string).collect();
            let met: Vec<&String> = ids.iter().filter(|id| known.contains(*id)).collect();
            Ok(serde_json::json!({
                "endpoint_id": node.endpoint_id().to_string(),
                "neighbors": node.neighbors().iter().map(ToString::to_string).collect::<Vec<_>>(),
                "met": met,
            }))
        }
        Control::Stop => {
            tracing::info!("stopping: asked by `wats stop`");
            // Before any session sees its node go: they must not start it
            // again (`wats stop` records it first, naming itself).
            let config = node.config();
            crate::background::record_stop(
                &config.data_dir,
                crate::background::StopReason::Stop,
                "a stop request on its socket",
                &config.profile,
            );
            let answer = info(node, sessions);
            // Let this answer go out first.
            let node = Arc::clone(node);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                node.request_stop();
            });
            Ok(answer)
        }
        Control::HandOver => {
            if node.config().role != crate::node::NodeRole::Background {
                bail!("this node is not a background node: it does not hand over");
            }
            tracing::info!("a daemon is taking over this data directory: stopping");
            crate::background::record_stop(
                &node.config().data_dir,
                crate::background::StopReason::HandOver,
                "a daemon taking over",
                &node.config().profile,
            );
            node.request_stop();
            Ok(serde_json::json!("handing over"))
        }
        Control::Drop { id, detail } => {
            crate::event::parse_hex::<32>(&id, "id")?;
            let outcome = node
                .drop_body(&id, crate::store::DropReason::Own, None, detail.as_deref())
                .await?;
            Ok(serde_json::to_value(outcome)?)
        }
        Control::Retract {
            ids,
            reason,
            profile,
        } => {
            let key = match profile.as_deref() {
                Some(profile) => node.author_key(profile)?,
                None => node.default_author().clone(),
            };
            let events = node.retract_as(&key, &ids, reason.as_deref()).await?;
            Ok(retracted_json(&events))
        }
    }
}

/// The answer to a retraction: per network, the retraction id and the
/// posts it names.
pub fn retracted_json(events: &[crate::event::Event]) -> serde_json::Value {
    serde_json::Value::Array(
        events
            .iter()
            .map(|e| serde_json::json!({"id": e.id, "net": e.net, "refs": e.refs}))
            .collect(),
    )
}

/// Send one [`Control`] request to the node running in `data_dir`. `None`
/// when no node listens there. Returns the node's answer.
pub async fn control(data_dir: &Path, request: Control) -> Result<Option<serde_json::Value>> {
    control_within(data_dir, request, std::time::Duration::from_secs(30)).await
}

/// [`control`], waiting at most `timeout` for the answer.
#[cfg(unix)]
pub async fn control_within(
    data_dir: &Path,
    request: Control,
    timeout: std::time::Duration,
) -> Result<Option<serde_json::Value>> {
    let Ok(stream) = tokio::net::UnixStream::connect(socket_path(data_dir)).await else {
        return Ok(None);
    };
    let (from_node, mut to_node) = stream.into_split();
    let exchange = async {
        let mut line = serde_json::to_vec(&ControlLine {
            wats_control: 1,
            request,
        })?;
        line.push(b'\n');
        to_node.write_all(&line).await?;
        let reply = BufReader::new(from_node)
            .lines()
            .next_line()
            .await?
            .context(
                "the node closed the connection without answering: it may be older than \
                 this wats, restart it",
            )?;
        let reply: serde_json::Value = serde_json::from_str(&reply)?;
        if let Some(error) = reply.get("error") {
            bail!("{}", error.as_str().unwrap_or("the node refused"));
        }
        Ok(reply["ok"].clone())
    };
    tokio::time::timeout(timeout, exchange)
        .await
        .with_context(|| format!("the node did not answer within {} s", timeout.as_secs()))?
        .map(Some)
}

#[cfg(not(unix))]
pub async fn control_within(
    _: &Path,
    _: Control,
    _: std::time::Duration,
) -> Result<Option<serde_json::Value>> {
    Ok(None)
}

#[cfg(not(unix))]
pub fn serve(_node: Arc<Node>) -> Result<Sessions> {
    Ok(Sessions::default())
}

/// Whether an MCP message from the client is part of its session setup
/// (replayed to a new node after a reconnection).
fn is_setup(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| {
        matches!(
            v["method"].as_str(),
            Some("initialize") | Some("notifications/initialized")
        )
    })
}

/// MCP tools that only read: a call in flight when the node goes away is
/// sent again to the next node (ADR 0021 §4). A call that publishes is
/// not, since the first node may have published it already: the client
/// gets an error saying so instead.
const RETRIED_TOOLS: [&str; 7] = [
    "read", "search", "browse", "who", "wait", "status", "pending",
];

/// A request sent to the node and not answered yet.
#[derive(Debug, Clone)]
struct InFlight {
    id: serde_json::Value,
    /// Its method: the answer to `tools/list` is kept for when no node
    /// runs.
    method: String,
    line: String,
    /// It may be sent again to the next node.
    retry: bool,
}

/// The request a client line carries, if it expects an answer.
fn in_flight(line: &str) -> Option<InFlight> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let id = v.get("id").filter(|id| !id.is_null())?.clone();
    let method = v.get("method")?.as_str()?;
    let retry = match method {
        "tools/call" => v["params"]["name"]
            .as_str()
            .is_some_and(|name| RETRIED_TOOLS.contains(&name)),
        // Listing tools, resources or prompts, pings: harmless to repeat.
        "initialize" => false,
        _ => !method.starts_with("notifications/"),
    };
    Some(InFlight {
        id,
        method: method.to_string(),
        line: line.to_string(),
        retry,
    })
}

/// The JSON-RPC error a client gets for a call lost in a node change.
fn lost_call(id: &serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32000, "message": "the wats node restarted during this call, \
            which may or may not have taken effect: check (e.g. with search) before trying again"},
    })
    .to_string()
}

/// The answer to a call lost when the node went away and none serves the
/// session now: `message` says why, and a call that may have published
/// says that too.
#[cfg(unix)]
fn lost_without_node(
    call: &InFlight,
    message: &str,
    tools: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    let request: serde_json::Value = serde_json::from_str(&call.line).ok()?;
    if call.method == "tools/call" && !call.retry {
        let message = format!(
            "The wats node stopped during this call, which may or may not have taken effect: \
             check (e.g. with search) once it runs again. {message}"
        );
        return crate::halt::answer_without_node(&request, &message, tools);
    }
    crate::halt::answer_without_node(&request, message, tools)
}

/// At most this many nodes started again by one session within
/// [`RESPAWN_WINDOW`] (ADR 0023 §6), so that a node that crashes at start
/// does not loop forever; then the session says so on every call.
pub const RESPAWN_LIMIT: usize = 5;
/// See [`RESPAWN_LIMIT`].
pub const RESPAWN_WINDOW: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// The wait after a node started again failed, before the next start:
/// 1 s, doubling with each failure in a row, at most 30 s.
#[cfg(unix)]
const RESPAWN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(unix)]
const RESPAWN_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a node started again has to accept sessions.
#[cfg(unix)]
const RESPAWN_START_WAIT: std::time::Duration = std::time::Duration::from_secs(20);
/// How long a session waits for a node that holds the data directory but
/// accepts no session yet (starting, or a daemon taking over).
#[cfg(unix)]
const NODE_START_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// How long a hand-over keeps the sessions waiting for the daemon (whose
/// takeover waits up to 30 s), in seconds.
#[cfg(unix)]
const HAND_OVER_WAIT: u64 = 60;
/// How long a session waits for the node of a named socket
/// ([`SOCKET_ENV`]) to come back.
#[cfg(unix)]
const NAMED_SOCKET_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// Locked by the session that starts a node again: the other sessions of
/// the data directory wait for that node instead of starting their own.
pub const RESPAWN_LOCK: &str = "respawn.lock";

/// Starts a background node for the data directory, and logs the reason
/// it is given.
pub type Respawn<'a> = Box<dyn Fn(&str) -> Result<std::process::Child> + Send + Sync + 'a>;

/// What an MCP session does when its node goes away (ADR 0023 §6).
pub struct Recovery<'a> {
    /// Starts the node again. `None` on a named socket ([`SOCKET_ENV`]):
    /// that node is another container's, never started here.
    pub respawn: Option<Respawn<'a>>,
    /// `--dir <dir>` when the data directory is not the default, for the
    /// commands the session suggests.
    pub dir_flag: String,
}

/// Where the session stands with its data directory's node.
#[cfg(unix)]
enum State {
    /// A node accepts this session.
    Up(tokio::net::UnixStream),
    /// Stopped on purpose, or halted: not started again; the message says
    /// why.
    Stopped(String),
    /// A node holds the data directory without accepting sessions yet, or
    /// a daemon is taking over: wait for it.
    Starting,
    /// No node runs, and none is starting.
    Gone,
}

/// One MCP session relayed to the node of its data directory, across node
/// changes.
#[cfg(unix)]
struct Session<'a> {
    data_dir: &'a Path,
    socket: PathBuf,
    recovery: Recovery<'a>,
    /// The node served last, from its lock file.
    node: Option<crate::node::LockHolder>,
    /// When this session started a node again, within [`RESPAWN_WINDOW`].
    respawns: std::collections::VecDeque<std::time::Instant>,
    /// Failed starts in a row.
    failures: u32,
    last_failure: Option<String>,
}

#[cfg(unix)]
impl Session<'_> {
    async fn connect(&self) -> Option<tokio::net::UnixStream> {
        tokio::net::UnixStream::connect(&self.socket).await.ok()
    }

    /// Note the node this session is attached to now.
    fn note_node(&mut self) {
        self.node = crate::node::lock_holder(self.data_dir)
            .filter(|_| crate::node::node_running(self.data_dir));
    }

    fn profile(&self) -> String {
        self.node
            .as_ref()
            .and_then(|h| h.profile.clone())
            .unwrap_or_else(|| "default".into())
    }

    fn start_command(&self) -> String {
        format!("wats{} --detach", self.recovery.dir_flag)
    }

    async fn assess(&self) -> State {
        use crate::background::StopReason;
        // A halted node may still accept sessions only to close them
        // (ADR 0016 §3): the marker first.
        if let Some(marker) = crate::halt::read_marker(self.data_dir) {
            return State::Stopped(crate::halt::mcp_message(&marker));
        }
        if let Some(stream) = self.connect().await {
            return State::Up(stream);
        }
        match crate::background::read_stop_record(self.data_dir) {
            Some(r) if r.reason == StopReason::Stop => {
                return State::Stopped(self.stopped_message(&r));
            }
            Some(r)
                if r.reason == StopReason::HandOver
                    && crate::node::now_secs().saturating_sub(r.at) < HAND_OVER_WAIT =>
            {
                return State::Starting;
            }
            _ => {}
        }
        if crate::node::node_running(self.data_dir) {
            State::Starting
        } else {
            State::Gone
        }
    }

    fn stopped_message(&self, record: &crate::background::StopRecord) -> String {
        format!(
            "The wats node for profile {} was stopped with {} at {}: this session does not \
             start it again. Tell the user. To use the swarm again, start it with `{}` (or \
             restart this MCP session); this session reconnects on its next call.",
            record.profile.as_deref().unwrap_or("default"),
            record.by,
            crate::runs::format_time(record.at),
            self.start_command(),
        )
    }

    fn gave_up_message(&self) -> String {
        format!(
            "The wats node for profile {} keeps stopping: this session started it {} times in \
             {} minutes{}, and does not try again on its own for now. Tell the user: see {}, \
             then start it with `{}` (or restart this MCP session); this session reconnects on \
             its next call.",
            self.profile(),
            self.respawns.len(),
            RESPAWN_WINDOW.as_secs() / 60,
            self.last_failure
                .as_deref()
                .map(|f| format!(" (last: {f})"))
                .unwrap_or_default(),
            crate::background::log_path(self.data_dir).display(),
            self.start_command(),
        )
    }

    fn unreachable_message(&self) -> String {
        if self.recovery.respawn.is_none() {
            return format!(
                "No wats node listens on {} ({SOCKET_ENV}). Tell the user to start that node; \
                 this session reconnects on its next call.",
                self.socket.display()
            );
        }
        let pid = crate::node::lock_holder(self.data_dir)
            .map(|h| format!(" (pid {})", h.pid))
            .unwrap_or_default();
        format!(
            "The wats node in {}{pid} holds its data directory but accepts no session. Tell the \
             user to restart it (`wats stop{}`, then `{}`); this session reconnects on its next \
             call.",
            self.data_dir.display(),
            self.recovery.dir_flag,
            self.start_command(),
        )
    }

    /// Why the node went away, for the logs: without a stop record, it
    /// was not asked to. Its process is gone once it lets go of the data
    /// directory lock (a zombie still answers `kill -0`).
    async fn why_lost(&self) -> String {
        let Some(h) = &self.node else {
            return "the node went away without `wats stop`".to_string();
        };
        for _ in 0..5 {
            if crate::node::lock_holder(self.data_dir).is_none_or(|now| now.pid != h.pid)
                || !crate::node::node_running(self.data_dir)
            {
                return format!(
                    "the node (pid {}) exited without `wats stop` (killed or crashed)",
                    h.pid
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        format!(
            "the node (pid {}) closed this session without `wats stop`",
            h.pid
        )
    }

    /// Whether this session may start a node now ([`RESPAWN_LIMIT`]).
    fn may_respawn(&mut self) -> bool {
        let now = std::time::Instant::now();
        while self
            .respawns
            .front()
            .is_some_and(|t| now.duration_since(*t) >= RESPAWN_WINDOW)
        {
            self.respawns.pop_front();
        }
        self.recovery.respawn.is_some() && self.respawns.len() < RESPAWN_LIMIT
    }

    /// The node went away (`why`): find the next one, starting it if
    /// nothing else will. `Err` with what to tell the client when no node
    /// serves this session for now.
    async fn recover(&mut self, why: &str) -> Result<Result<tokio::net::UnixStream, String>> {
        let since = std::time::Instant::now();
        loop {
            match self.assess().await {
                State::Up(stream) => return Ok(Ok(stream)),
                State::Stopped(message) => return Ok(Err(message)),
                State::Starting => {
                    if since.elapsed() >= NODE_START_WAIT {
                        return Ok(Err(self.unreachable_message()));
                    }
                }
                State::Gone if self.recovery.respawn.is_none() => {
                    if since.elapsed() >= NAMED_SOCKET_WAIT {
                        return Ok(Err(self.unreachable_message()));
                    }
                }
                State::Gone => {
                    if !self.may_respawn() {
                        return Ok(Err(self.gave_up_message()));
                    }
                    if let Some(stream) = self.respawn(why).await? {
                        return Ok(Ok(stream));
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Start a node again, unless another session does: `Some` once one
    /// accepts this session. Holds [`RESPAWN_LOCK`] meanwhile, and through
    /// the wait after a failed start, so that the sessions of one data
    /// directory start one node at a time; the data directory lock still
    /// lets only one run.
    async fn respawn(&mut self, why: &str) -> Result<Option<tokio::net::UnixStream>> {
        let Some(spawn) = &self.recovery.respawn else {
            return Ok(None);
        };
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.data_dir.join(RESPAWN_LOCK));
        // Without the file (a read-only directory?), start it anyway.
        let _lock = match lock {
            Ok(file) => match file.try_lock() {
                Ok(()) => Some(file),
                Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
                Err(std::fs::TryLockError::Error(_)) => None,
            },
            Err(_) => None,
        };
        // Another session's node may be up, or starting, by now.
        match self.assess().await {
            State::Gone => {}
            State::Up(stream) => return Ok(Some(stream)),
            State::Stopped(_) | State::Starting => return Ok(None),
        }
        let started = std::time::Instant::now();
        self.respawns.push_back(started);
        let reason = format!(
            "{why}; started again by the MCP session pid {} ({} of at most {RESPAWN_LIMIT} in {} \
             min)",
            std::process::id(),
            self.respawns.len(),
            RESPAWN_WINDOW.as_secs() / 60,
        );
        tracing::warn!("{reason}");
        let failure = match spawn(&reason) {
            Err(e) => format!("{e:#}"),
            Ok(mut child) => loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                if let Some(stream) = self.connect().await {
                    tracing::info!(pid = child.id(), "the new node accepts this session");
                    self.failures = 0;
                    crate::background::reap(child);
                    return Ok(Some(stream));
                }
                match child.try_wait() {
                    // It lost the data directory to another node.
                    Ok(Some(_)) if crate::node::node_running(self.data_dir) => return Ok(None),
                    Ok(Some(status)) => break format!("the new node exited at start ({status})"),
                    Ok(None) if started.elapsed() >= RESPAWN_START_WAIT => {
                        let pid = child.id();
                        crate::background::reap(child);
                        break format!(
                            "the new node (pid {pid}) accepted no session within {} s",
                            RESPAWN_START_WAIT.as_secs()
                        );
                    }
                    Ok(None) => {}
                    Err(e) => break e.to_string(),
                }
            },
        };
        self.failures = self.failures.saturating_add(1);
        tracing::warn!(
            "{failure}; see {}",
            crate::background::log_path(self.data_dir).display()
        );
        self.last_failure = Some(failure);
        if self.may_respawn() {
            let backoff = RESPAWN_BACKOFF
                .saturating_mul(2u32.saturating_pow(self.failures.saturating_sub(1)))
                .min(RESPAWN_BACKOFF_MAX);
            tokio::time::sleep(backoff).await;
        }
        Ok(None)
    }

    /// Answer the client while no node serves it, saying `message`, until
    /// a node accepts this session again (started by hand, by another
    /// session, or by this one when nothing says not to): then returns it
    /// with the request that found it. `None` when the client leaves.
    async fn serve_down<R, W>(
        &mut self,
        stdin: &mut tokio::io::Lines<R>,
        stdout: &mut W,
        setup: &mut Vec<String>,
        mut message: String,
        tools: Option<&serde_json::Value>,
    ) -> Result<Option<(tokio::net::UnixStream, String)>>
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        while let Some(line) = stdin.next_line().await? {
            let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            // The client's setup, answered here, is replayed to the next
            // node.
            if setup.len() < 2 && is_setup(&line) {
                setup.push(line.clone());
            } else if request.get("id").is_none_or(serde_json::Value::is_null) {
                continue; // a notification
            }
            match self.assess().await {
                _ if is_setup(&line) => {}
                State::Up(stream) => return Ok(Some((stream, line))),
                State::Stopped(said) => message = said,
                State::Starting => {}
                State::Gone if self.may_respawn() => {
                    let why = "no node runs (the last one went away without `wats stop`)";
                    match self.recover(why).await? {
                        Ok(stream) => return Ok(Some((stream, line))),
                        Err(said) => message = said,
                    }
                }
                State::Gone if self.recovery.respawn.is_some() => {
                    message = self.gave_up_message();
                }
                State::Gone => {}
            }
            if let Some(reply) = crate::halt::answer_without_node(&request, &message, tools) {
                stdout.write_all(format!("{reply}\n").as_bytes()).await?;
                stdout.flush().await?;
            }
        }
        Ok(None)
    }
}

/// Write one line to the node.
#[cfg(unix)]
async fn send_line(to_node: &mut tokio::net::unix::OwnedWriteHalf, line: &[u8]) -> Result<()> {
    to_node.write_all(line).await?;
    if !line.ends_with(b"\n") {
        to_node.write_all(b"\n").await?;
    }
    Ok(())
}

/// The id of the client's `initialize`, among the setup lines.
#[cfg(unix)]
fn initialize_id(setup: &[String]) -> Option<serde_json::Value> {
    setup
        .iter()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["method"] == "initialize")
        .map(|v| v["id"].clone())
}

/// Attach this process's stdio to the node owning `data_dir`, if one is
/// running. Returns `Ok(false)` when no node is listening.
///
/// When the node goes away, the session finds the next one and replays
/// the client's `initialize` and `initialized` to it, swallowing the new
/// `initialize` answer: the MCP client sees nothing (ADR 0023 §6).
///
/// - **A daemon takes over** (a hand-over record), or a node holds the
///   data directory: the session waits for it.
/// - **Crashed or killed** (no stop record): the session starts a
///   background node again through `recovery`, as `wats mcp` does when
///   none runs; the sessions of the data directory converge on one node
///   ([`RESPAWN_LOCK`], then the data directory lock), and each gives up
///   after [`RESPAWN_LIMIT`] starts in [`RESPAWN_WINDOW`].
/// - **Stopped with `wats stop`**, or halted (ADR 0016 §3): the session
///   does not start it again. It stays up and answers every call with
///   why, and reconnects on the first call once a node listens again.
///
/// A call in flight when the node went away is sent once more to the next
/// node if it only reads; otherwise the client gets an error for it (see
/// [`RETRIED_TOOLS`]).
#[cfg(unix)]
pub async fn attach(
    data_dir: &Path,
    profile: Option<String>,
    file_roots: Vec<PathBuf>,
    recovery: Recovery<'_>,
) -> Result<bool> {
    use tokio::io::AsyncBufReadExt;
    if let Some(profile) = &profile
        && !is_valid_profile(profile)
    {
        bail!("profile name must be 1-64 chars of [a-z0-9_-]");
    }
    let socket = socket_path(data_dir);
    let Ok(mut stream) = tokio::net::UnixStream::connect(&socket).await else {
        return Ok(false);
    };
    let mut handshake = serde_json::to_vec(&Handshake {
        wats_attach: 1,
        profile,
        file_roots,
        runner_token: std::env::var("WATS_RUNNER_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
        client: client_from_env(),
    })?;
    handshake.push(b'\n');
    let mut session = Session {
        data_dir,
        socket,
        recovery,
        node: None,
        respawns: std::collections::VecDeque::new(),
        failures: 0,
        last_failure: None,
    };
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut setup: Vec<String> = Vec::new();
    // The id of a replayed `initialize` whose answer the client must not see.
    let mut swallow: Option<serde_json::Value> = None;
    // Requests sent and not answered yet, and those to send again to the
    // next node after a change.
    let mut pending: Vec<InFlight> = Vec::new();
    let mut resend: Vec<InFlight> = Vec::new();
    // A request that came while no node served this session, for the
    // next one.
    let mut held: Option<String> = None;
    // The node's last answer to `tools/list`, for when none runs.
    let mut tools: Option<serde_json::Value> = None;
    // Nodes that closed this session without a word, in a row.
    let mut silent = 0u32;
    loop {
        session.note_node();
        let (from_node, mut to_node) = stream.into_split();
        let mut answered = false;
        // The calls to send again, once only (lost again, the client gets
        // an error), then the request that found this node.
        let mut again: Vec<String> = Vec::new();
        for call in std::mem::take(&mut resend) {
            again.push(call.line.clone());
            pending.push(InFlight {
                retry: false,
                ..call
            });
        }
        if let Some(line) = held.take() {
            pending.extend(in_flight(&line));
            again.push(line);
        }
        // A node that dies during the replay is lost like any other.
        let replayed = async {
            send_line(&mut to_node, &handshake).await?;
            for line in setup.iter().chain(&again) {
                send_line(&mut to_node, line.as_bytes()).await?;
            }
            anyhow::Ok(())
        }
        .await
        .is_ok();
        let mut from_node = BufReader::new(from_node).lines();
        loop {
            if !replayed {
                break;
            }
            tokio::select! {
                line = stdin.next_line() => {
                    let Some(line) = line? else { return Ok(true) };
                    if setup.len() < 2 && is_setup(&line) {
                        setup.push(line.clone());
                    }
                    if let Some(call) = in_flight(&line).filter(|c| {
                        // The client's own initialize is replayed as setup.
                        !is_setup(&c.line)
                    }) {
                        pending.push(call);
                    }
                    if send_line(&mut to_node, line.as_bytes()).await.is_err() {
                        break;
                    }
                }
                line = from_node.next_line() => {
                    // A killed node may reset the connection: lost too.
                    let Ok(Some(line)) = line else { break };
                    answered = true;
                    let value = serde_json::from_str::<serde_json::Value>(&line)
                        .ok()
                        .filter(|v| v.get("method").is_none());
                    let answer = value.as_ref().and_then(|v| v.get("id").cloned());
                    if let Some(id) = &swallow
                        && answer.as_ref() == Some(id)
                    {
                        swallow = None;
                        continue;
                    }
                    if let Some(id) = &answer {
                        if pending.iter().any(|c| &c.id == id && c.method == "tools/list")
                            && let Some(result) = value.as_ref().and_then(|v| v.get("result"))
                        {
                            tools = Some(result.clone());
                        }
                        pending.retain(|c| &c.id != id);
                    }
                    stdout.write_all(line.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
            }
        }
        // The node went away: the next one, or why there is none.
        silent = if answered {
            0
        } else {
            silent.saturating_add(1)
        };
        let why = session.why_lost().await;
        let next = if silent >= 10 {
            Err(session.unreachable_message())
        } else {
            session.recover(&why).await?
        };
        match next {
            Ok(next) => {
                stream = next;
                swallow = initialize_id(&setup);
                for call in std::mem::take(&mut pending) {
                    if call.retry {
                        resend.push(call);
                    } else {
                        stdout.write_all(lost_call(&call.id).as_bytes()).await?;
                        stdout.write_all(b"\n").await?;
                        stdout.flush().await?;
                    }
                }
            }
            Err(message) => {
                tracing::warn!("{message}");
                for call in std::mem::take(&mut pending) {
                    if let Some(reply) = lost_without_node(&call, &message, tools.as_ref()) {
                        stdout.write_all(format!("{reply}\n").as_bytes()).await?;
                        stdout.flush().await?;
                    }
                }
                let back = session
                    .serve_down(&mut stdin, &mut stdout, &mut setup, message, tools.as_ref())
                    .await?;
                let Some((next, line)) = back else {
                    return Ok(true);
                };
                stream = next;
                swallow = initialize_id(&setup);
                held = Some(line);
            }
        }
    }
}

#[cfg(not(unix))]
pub async fn attach(_: &Path, _: Option<String>, _: Vec<PathBuf>, _: Recovery<'_>) -> Result<bool> {
    Ok(false)
}

/// Call one MCP tool of the node running in `data_dir`, as a short MCP
/// session over its socket (e.g. `wats status`). `None` when no node runs
/// there. Returns the tool's structured result.
#[cfg(unix)]
pub async fn call_tool(
    data_dir: &Path,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<Option<serde_json::Value>> {
    use serde_json::json;
    let Ok(stream) = tokio::net::UnixStream::connect(socket_path(data_dir)).await else {
        return Ok(None);
    };
    let (from_node, mut to_node) = stream.into_split();
    let mut from_node = BufReader::new(from_node).lines();
    let handshake = Handshake {
        wats_attach: 1,
        profile: None,
        file_roots: Vec::new(),
        runner_token: None,
        client: None,
    };
    let initialize = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "wats-cli", "version": env!("CARGO_PKG_VERSION")}}});
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": tool, "arguments": arguments}});
    let exchange = async {
        for message in [serde_json::to_value(&handshake)?, initialize] {
            to_node.write_all(&serde_json::to_vec(&message)?).await?;
            to_node.write_all(b"\n").await?;
        }
        let response = |id: u64| {
            move |line: &str| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .filter(|v| v["id"] == id && v.get("method").is_none())
            }
        };
        loop {
            let line = from_node
                .next_line()
                .await?
                .context("the node closed the session")?;
            if response(1)(&line).is_some() {
                break;
            }
        }
        for message in [initialized, call] {
            to_node.write_all(&serde_json::to_vec(&message)?).await?;
            to_node.write_all(b"\n").await?;
        }
        loop {
            let line = from_node
                .next_line()
                .await?
                .context("the node closed the session")?;
            if let Some(reply) = response(2)(&line) {
                if let Some(error) = reply.get("error") {
                    bail!("{tool}: {}", error["message"]);
                }
                let result = &reply["result"];
                if result["isError"] == true {
                    bail!("{tool}: {}", result["content"][0]["text"]);
                }
                return Ok(result["structuredContent"].clone());
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), exchange)
        .await
        .context("the node did not answer within 10 s")?
        .map(Some)
}

#[cfg(not(unix))]
pub async fn call_tool(
    _: &Path,
    _: &str,
    _: serde_json::Value,
) -> Result<Option<serde_json::Value>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_reading_calls_are_sent_again_after_a_node_change() {
        let call = |tool: &str| {
            format!(
                r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{"name":"{tool}"}}}}"#
            )
        };
        assert!(in_flight(&call("search")).unwrap().retry);
        assert!(in_flight(&call("read")).unwrap().retry);
        assert!(
            !in_flight(&call("post")).unwrap().retry,
            "may have published"
        );
        assert!(!in_flight(&call("reply")).unwrap().retry);
        let list = r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#;
        assert!(in_flight(list).unwrap().retry);
        let note = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(in_flight(note).is_none(), "no answer expected");
        let lost: serde_json::Value =
            serde_json::from_str(&lost_call(&serde_json::json!(7))).unwrap();
        assert_eq!(lost["id"], 7);
        assert!(
            lost["error"]["message"]
                .as_str()
                .unwrap()
                .contains("restarted")
        );
        assert!(
            !lost["error"]["message"].as_str().unwrap().contains("  "),
            "one line: {lost}"
        );
    }

    #[test]
    fn calls_lost_with_no_node_back_say_why() {
        let call = |tool: &str| {
            in_flight(&format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"{tool}"}}}}"#
            ))
            .unwrap()
        };
        let stopped = "The wats node for profile default was stopped with `wats stop`";
        let read = lost_without_node(&call("search"), stopped, None).unwrap();
        assert_eq!(read["id"], 3);
        assert_eq!(read["result"]["isError"], true);
        assert_eq!(read["result"]["content"][0]["text"], stopped);
        // A write may have gone out: the client is told to check.
        let post = lost_without_node(&call("post"), stopped, None).unwrap();
        let text = post["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("may or may not have taken effect"), "{text}");
        assert!(text.ends_with(stopped), "{text}");
        // Listing tools: the node's last list.
        let tools = serde_json::json!({"tools": [{"name": "search"}]});
        let list = in_flight(r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#).unwrap();
        let listed = lost_without_node(&list, stopped, Some(&tools)).unwrap();
        assert_eq!(listed["result"], tools);
    }

    #[test]
    fn long_data_dirs_get_a_short_socket_path() {
        assert_eq!(
            default_socket_path(Path::new("/d")),
            Path::new("/d/mcp.sock")
        );
        let long = PathBuf::from("/").join("x".repeat(120));
        let path = default_socket_path(&long);
        assert!(
            path.as_os_str().len() <= MAX_SOCKET_PATH,
            "{}",
            path.display()
        );
        assert_eq!(path, default_socket_path(&long), "deterministic");
        assert_ne!(
            path,
            default_socket_path(&PathBuf::from("/").join("y".repeat(120)))
        );
    }
}
