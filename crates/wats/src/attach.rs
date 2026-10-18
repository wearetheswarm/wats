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
}

/// Longest Unix socket path we use (`sun_path` is 104 bytes on macOS,
/// 108 on Linux, including the terminating NUL).
const MAX_SOCKET_PATH: usize = 100;

/// Where the node owning `data_dir` listens: `<data dir>/mcp.sock`, or, when
/// that path is too long for a Unix socket, `~/.wats/sock/<hash>.sock`
/// with the hash derived from the data directory.
pub fn socket_path(data_dir: &Path) -> PathBuf {
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
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(e) = serve_session(node, stream).await {
                debug!("attached session ended: {e:#}");
            }
        });
    }
}

#[cfg(unix)]
async fn serve_session(node: Arc<Node>, stream: tokio::net::UnixStream) -> Result<()> {
    let (read, write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    (&mut reader)
        .take(HANDSHAKE_MAX_BYTES)
        .read_line(&mut line)
        .await?;
    let handshake: Handshake = serde_json::from_str(&line).context("bad attach handshake")?;
    if handshake.wats_attach != 1 {
        bail!("unsupported attach version {}", handshake.wats_attach);
    }
    let mut server = WatsMcp::new(Arc::clone(&node), handshake.file_roots);
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
    server.serve((reader, write)).await?.waiting().await?;
    Ok(())
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

/// Attach this process's stdio to the node owning `data_dir`, if one is
/// running. Returns `Ok(false)` when no node is listening.
///
/// When the node goes away (e.g. a background node handing over to a
/// daemon), the session waits up to 30 s for a node to listen again,
/// replays the client's `initialize` and `initialized` to it, swallows the
/// new `initialize` answer, and carries on: the MCP client sees nothing.
#[cfg(unix)]
pub async fn attach(
    data_dir: &Path,
    profile: Option<String>,
    file_roots: Vec<PathBuf>,
) -> Result<bool> {
    use tokio::io::AsyncBufReadExt;
    if let Some(profile) = &profile
        && !is_valid_profile(profile)
    {
        bail!("profile name must be 1-64 chars of [a-z0-9_-]");
    }
    let path = socket_path(data_dir);
    let Ok(mut stream) = tokio::net::UnixStream::connect(&path).await else {
        return Ok(false);
    };
    let mut handshake = serde_json::to_vec(&Handshake {
        wats_attach: 1,
        profile,
        file_roots,
        runner_token: std::env::var("WATS_RUNNER_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
    })?;
    handshake.push(b'\n');
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut setup: Vec<String> = Vec::new();
    // The id of a replayed `initialize` whose answer the client must not see.
    let mut swallow: Option<serde_json::Value> = None;
    loop {
        let (from_node, mut to_node) = stream.into_split();
        to_node.write_all(&handshake).await?;
        for line in &setup {
            to_node.write_all(line.as_bytes()).await?;
            to_node.write_all(b"\n").await?;
        }
        let mut from_node = BufReader::new(from_node).lines();
        loop {
            tokio::select! {
                line = stdin.next_line() => {
                    let Some(line) = line? else { return Ok(true) };
                    if setup.len() < 2 && is_setup(&line) {
                        setup.push(line.clone());
                    }
                    to_node.write_all(line.as_bytes()).await?;
                    to_node.write_all(b"\n").await?;
                }
                line = from_node.next_line() => {
                    let Some(line) = line? else { break };
                    if let Some(id) = &swallow
                        && serde_json::from_str::<serde_json::Value>(&line)
                            .is_ok_and(|v| &v["id"] == id && v.get("method").is_none())
                    {
                        swallow = None;
                        continue;
                    }
                    stdout.write_all(line.as_bytes()).await?;
                    stdout.write_all(b"\n").await?;
                    stdout.flush().await?;
                }
            }
        }
        // The node went away: wait for the next one.
        let mut next = None;
        for _ in 0..300 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if let Ok(s) = tokio::net::UnixStream::connect(&path).await {
                next = Some(s);
                break;
            }
        }
        let Some(s) = next else { return Ok(true) };
        stream = s;
        swallow = setup
            .iter()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["method"] == "initialize")
            .map(|v| v["id"].clone());
    }
}

#[cfg(not(unix))]
pub async fn attach(_: &Path, _: Option<String>, _: Vec<PathBuf>) -> Result<bool> {
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
    fn long_data_dirs_get_a_short_socket_path() {
        assert_eq!(socket_path(Path::new("/d")), Path::new("/d/mcp.sock"));
        let long = PathBuf::from("/").join("x".repeat(120));
        let path = socket_path(&long);
        assert!(
            path.as_os_str().len() <= MAX_SOCKET_PATH,
            "{}",
            path.display()
        );
        assert_eq!(path, socket_path(&long), "deterministic");
        assert_ne!(path, socket_path(&PathBuf::from("/").join("y".repeat(120))));
    }
}
