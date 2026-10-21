//! Possession proofs (ADR 0029 §4): a peer that says it holds a blob is
//! not believed on its word before this node releases its own copy, nor
//! counted as a replica by repair. This node fetches from that peer alone
//! a random chunk and the last chunk of the blob, BLAKE3/BAO-verified
//! against the hash, into memory: a few KiB, and the last chunk proves the
//! size.
//!
//! While any proof of a blob runs, this node refuses that blob to every
//! requester, unknown connections included (an intercepting event handler
//! on the blobs protocol): neither the peer under proof nor a second
//! identity of it can fetch the chunks from this node's own copy and relay
//! them. Proofs are rare and last seconds; a refused reader tries another
//! provider, or again. The peer can still fetch the chunks from a third
//! node that holds the blob: then a copy exists there, which is what a
//! release needs, but it may be counted twice. A snapshot, too: a peer may
//! delete after proving.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::EndpointId;
use iroh_blobs::Hash;
use iroh_blobs::protocol::{ChunkRanges, ChunkRangesExt, GetRequest};
use iroh_blobs::provider::events::{
    AbortReason, ConnectMode, EventMask, EventSender, ObserveMode, ProviderMessage, RequestMode,
    ThrottleMode,
};
use tracing::debug;

use super::{Node, REPAIR_CONNECT_LIMIT};
use crate::supervise::supervise;

/// The longest one proof may take, connection included.
const PROOF_TIMEOUT: Duration = Duration::from_secs(20);
/// Events of the blobs protocol waiting for the handler.
const EVENT_QUEUE: usize = 256;

/// The blobs under proof, with how many proofs of each run.
#[derive(Debug, Default)]
pub struct Guard {
    proving: Mutex<HashMap<Hash, usize>>,
}

/// A proof in progress: its blob refused to every requester until
/// dropped.
pub struct Proving {
    guard: Arc<Guard>,
    hash: Hash,
}

impl Drop for Proving {
    fn drop(&mut self) {
        if let Ok(mut proving) = self.guard.proving.lock()
            && let Some(count) = proving.get_mut(&self.hash)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                proving.remove(&self.hash);
            }
        }
    }
}

impl Guard {
    /// The event sender for the blobs protocol: `get` and `get_many`
    /// intercepted (refused while a proof of one of their blobs runs), the
    /// rest as without one. Its handler runs supervised: a request is
    /// answered as long as the node runs.
    pub fn event_sender(self: &Arc<Self>) -> EventSender {
        let mask = EventMask {
            connected: ConnectMode::None,
            get: RequestMode::Intercept,
            get_many: RequestMode::Intercept,
            push: RequestMode::Disabled,
            observe: ObserveMode::None,
            throttle: ThrottleMode::None,
        };
        let (sender, receiver) = EventSender::channel(EVENT_QUEUE, mask);
        let receiver = Arc::new(tokio::sync::Mutex::new(receiver));
        let guard = Arc::downgrade(self);
        tokio::spawn(supervise("blob request guard", move || {
            let (receiver, guard) = (Arc::clone(&receiver), guard.clone());
            async move {
                let mut receiver = receiver.lock().await;
                while let Some(message) = receiver.recv().await {
                    let Some(guard) = guard.upgrade() else { break };
                    guard.handle(message).await;
                }
            }
        }));
        sender
    }

    async fn handle(&self, message: ProviderMessage) {
        match message {
            ProviderMessage::GetRequestReceived(msg) => {
                let verdict = self.verdict(&[msg.inner.request.hash]);
                msg.tx.send(verdict).await.ok();
            }
            ProviderMessage::GetManyRequestReceived(msg) => {
                let verdict = self.verdict(&msg.inner.request.hashes);
                msg.tx.send(verdict).await.ok();
            }
            // Not asked for in the mask; answered anyway, as without one.
            ProviderMessage::ClientConnected(msg) => {
                msg.tx.send(Ok(())).await.ok();
            }
            ProviderMessage::PushRequestReceived(msg) => {
                msg.tx.send(Err(AbortReason::Permission)).await.ok();
            }
            ProviderMessage::ObserveRequestReceived(msg) => {
                msg.tx.send(Ok(())).await.ok();
            }
            ProviderMessage::Throttle(msg) => {
                msg.tx.send(Ok(())).await.ok();
            }
            _ => {}
        }
    }

    /// Refuse a request of `hashes` while a proof of one of them runs,
    /// whoever asks.
    fn verdict(&self, hashes: &[Hash]) -> Result<(), AbortReason> {
        let blocked = self
            .proving
            .lock()
            .is_ok_and(|p| hashes.iter().any(|h| p.contains_key(h)));
        if blocked {
            debug!("blob request refused: a possession proof of this blob runs");
            Err(AbortReason::Permission)
        } else {
            Ok(())
        }
    }

    pub(super) fn block(self: &Arc<Self>, hash: Hash) -> Proving {
        if let Ok(mut proving) = self.proving.lock() {
            let count = proving.entry(hash).or_insert(0);
            *count = count.saturating_add(1);
        }
        Proving {
            guard: Arc::clone(self),
            hash,
        }
    }
}

impl Node {
    /// Test only: refuse blob `hash` to every requester, as while a
    /// possession proof of it runs, until the returned value is dropped.
    #[doc(hidden)]
    pub fn block_blob_requests_for_tests(&self, hash: Hash) -> Proving {
        self.proof_guard.block(hash)
    }
}

/// The byte ranges `[start, end)` of the chunks a proof asks for: one
/// chosen at random, and the last one.
fn proof_chunks(size: u64, random: u64) -> Vec<(u64, u64)> {
    let chunks = size.div_ceil(1024).max(1);
    let last = chunks.saturating_sub(1);
    let pick = random.checked_rem(chunks).unwrap_or(0);
    let mut out = vec![pick];
    if last != pick {
        out.push(last);
    }
    out.into_iter()
        .map(|c| {
            let start = c.saturating_mul(1024);
            (start, start.saturating_add(1024).min(size))
        })
        .collect()
}

/// Whether the `[start, end)` intervals `have` cover every one of `want`.
fn covered(want: &[(u64, u64)], have: &mut [(u64, u64)]) -> bool {
    have.sort_unstable();
    want.iter().all(|&(start, end)| {
        let mut at = start;
        for &(s, e) in have.iter() {
            if s <= at && e > at {
                at = e;
            }
            if at >= end {
                return true;
            }
        }
        at >= end
    })
}

impl Node {
    /// Whether `peer` holds blob `hash` of `size` bytes: proved by fetching
    /// from it alone a random chunk and the last one (see the module doc).
    /// False on any failure or timeout.
    pub(super) async fn prove_holder(&self, peer: EndpointId, hash: Hash, size: u64) -> bool {
        let _blocked = self.proof_guard.block(hash);
        let want = proof_chunks(size, getrandom::u64().unwrap_or(0));
        let proved = tokio::time::timeout(PROOF_TIMEOUT, self.fetch_proof(peer, hash, size, &want))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out")));
        match proved {
            Ok(true) => true,
            Ok(false) => {
                debug!(peer = %peer.fmt_short(), %hash, "possession proof failed: chunks missing or another size");
                false
            }
            Err(e) => {
                debug!(peer = %peer.fmt_short(), %hash, "possession proof failed: {e:#}");
                false
            }
        }
    }

    async fn fetch_proof(
        &self,
        peer: EndpointId,
        hash: Hash,
        size: u64,
        want: &[(u64, u64)],
    ) -> anyhow::Result<bool> {
        use bao_tree::io::BaoContentItem;
        use iroh_blobs::get::fsm;

        let mut ranges = ChunkRanges::empty();
        for &(start, _) in want {
            ranges |= ChunkRanges::chunk(start / 1024);
        }
        let request = GetRequest::builder().root(ranges).build(hash);
        let connection = tokio::time::timeout(
            REPAIR_CONNECT_LIMIT,
            self.endpoint().connect(peer, iroh_blobs::ALPN),
        )
        .await
        .map_err(|_| anyhow::anyhow!("no connection within {REPAIR_CONNECT_LIMIT:?}"))??;
        let connected = fsm::start(connection, request, Default::default())
            .next()
            .await?;
        let fsm::ConnectedNext::StartRoot(root) = connected.next().await? else {
            anyhow::bail!("the peer answered with no blob");
        };
        let (mut content, got) = root.next().next().await?;
        if got != size {
            return Ok(false);
        }
        let mut have = Vec::new();
        let end = loop {
            match content.next().await {
                fsm::BlobContentNext::More((next, item)) => {
                    if let BaoContentItem::Leaf(leaf) = item? {
                        have.push((
                            leaf.offset,
                            leaf.offset.saturating_add(leaf.data.len() as u64),
                        ));
                    }
                    content = next;
                }
                fsm::BlobContentNext::Done(end) => break end,
            }
        };
        if let fsm::EndBlobNext::Closing(closing) = end.next() {
            closing.next().await?;
        }
        Ok(covered(want, &mut have))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_asks_a_random_chunk_and_the_last() {
        assert_eq!(proof_chunks(10, 7), vec![(0, 10)]);
        assert_eq!(proof_chunks(4096, 1), vec![(1024, 2048), (3072, 4096)]);
        assert_eq!(proof_chunks(4097, 4), vec![(4096, 4097)]);
        assert_eq!(proof_chunks(0, 3), vec![(0, 0)]);
    }

    #[test]
    fn coverage_needs_every_byte_asked() {
        let want = [(1024, 2048), (3072, 4096)];
        assert!(covered(&want, &mut [(0, 16384)]));
        assert!(covered(
            &want,
            &mut [(3072, 4096), (1024, 1500), (1500, 2048)]
        ));
        assert!(!covered(&want, &mut [(1024, 2048)]));
        assert!(!covered(&want, &mut [(1024, 2000), (3072, 4096)]));
    }
}
