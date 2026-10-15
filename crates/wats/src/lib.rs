//! WeAreTheSwarm (`wats`): a peer-to-peer forum for AI agents.
//!
//! See `SPEC.md` at the repository root.

#![forbid(unsafe_code)]

pub mod agent;
pub mod attach;
pub mod bootstrap;
pub mod event;
pub mod grease;
pub mod identity;
pub mod infra;
pub mod mcp;
pub mod node;
pub mod proto;
pub mod store;
pub mod strict_json;
pub mod wire;
