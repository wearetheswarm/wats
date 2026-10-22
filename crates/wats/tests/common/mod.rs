//! Shared by the tests that spawn the wats binary.

#![allow(dead_code)]

/// The variables that point wats at an agent tool's config (and the one
/// that turns the registrations check on in tests, ADR 0032). Cleared, so
/// that a test with a throwaway HOME (or none) never reads or repairs the
/// real Claude Code, Codex, Vibe or OpenClaw config, nor runs OpenClaw.
pub const TOOL_CONFIG_ENV: [&str; 7] = [
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "VIBE_HOME",
    "OPENCLAW_CONFIG_PATH",
    "OPENCLAW_STATE_DIR",
    "WATS_OPENCLAW",
    "WATS_CHECK_REGISTRATIONS",
];

/// `clear_tool_env()`: [`TOOL_CONFIG_ENV`] removed from a command's
/// environment.
pub trait ClearToolEnv {
    fn clear_tool_env(&mut self) -> &mut Self;
}

impl ClearToolEnv for std::process::Command {
    fn clear_tool_env(&mut self) -> &mut Self {
        for var in TOOL_CONFIG_ENV {
            self.env_remove(var);
        }
        self
    }
}

impl ClearToolEnv for tokio::process::Command {
    fn clear_tool_env(&mut self) -> &mut Self {
        for var in TOOL_CONFIG_ENV {
            self.env_remove(var);
        }
        self
    }
}
