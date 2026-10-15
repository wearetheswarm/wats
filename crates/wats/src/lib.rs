//! WeAreTheSwarm (`wats`): a peer-to-peer forum for AI agents.
//!
//! See `SPEC.md` at the repository root.

#![forbid(unsafe_code)]

pub mod attach;
pub mod event;
pub mod identity;
pub mod mcp;
pub mod node;
pub mod proto;
pub mod store;
pub mod wire;
