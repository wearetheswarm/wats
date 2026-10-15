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
    let sessions = Sessions::default();
    let counter = sessions.clone();
    tokio::spawn(async move {
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
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
    });
    Ok(sessions)
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
    server.serve((reader, write)).await?.waiting().await?;
    Ok(())
}

#[cfg(not(unix))]
pub fn serve(_node: Arc<Node>) -> Result<Sessions> {
    Ok(Sessions::default())
}

/// Attach this process's stdio to the node owning `data_dir`, if one is
/// running. Returns `Ok(false)` when no node is listening.
#[cfg(unix)]
pub async fn attach(
    data_dir: &Path,
    profile: Option<String>,
    file_roots: Vec<PathBuf>,
) -> Result<bool> {
    if let Some(profile) = &profile
        && !is_valid_profile(profile)
    {
        bail!("profile name must be 1-64 chars of [a-z0-9_-]");
    }
    let path = socket_path(data_dir);
    let Ok(stream) = tokio::net::UnixStream::connect(&path).await else {
        return Ok(false);
    };
    let (mut from_node, mut to_node) = stream.into_split();
    let mut handshake = serde_json::to_vec(&Handshake {
        wats_attach: 1,
        profile,
        file_roots,
    })?;
    handshake.push(b'\n');
    to_node.write_all(&handshake).await?;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        r = tokio::io::copy(&mut stdin, &mut to_node) => { r?; }
        r = tokio::io::copy(&mut from_node, &mut stdout) => { r?; }
    }
    Ok(true)
}

#[cfg(not(unix))]
pub async fn attach(_: &Path, _: Option<String>, _: Vec<PathBuf>) -> Result<bool> {
    Ok(false)
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
