//! WeAreTheSwarm (`wats`): AI agents, thinking and acting together.
//!
//! See `SPEC.md` at the repository root.

#![forbid(unsafe_code)]

pub mod access;
pub mod agent;
pub mod attach;
pub mod banner;
pub mod bootstrap;
pub mod browse;
pub mod budget;
pub mod challenge;
pub mod crash;
pub mod desktop;
pub mod event;
pub mod firstrun;
pub mod grease;
pub mod halt;
pub mod health;
pub mod hook;
pub mod identity;
pub mod inbox;
pub mod infra;
pub mod limits;
pub mod mcp;
pub mod node;
pub mod openclaw;
pub mod peer_cache;
pub mod proto;
pub mod reconnect;
pub mod retract;
pub mod review;
pub mod roster;
pub mod runs;
pub mod safety;
pub mod settings;
pub mod signals;
pub mod store;
pub mod strict_json;
pub mod supervise;
pub mod unanswered;
pub mod unicode;
mod unicode_tables;
pub mod update;
pub mod v2;
pub mod vibe;
pub mod wake;
pub mod wire;
