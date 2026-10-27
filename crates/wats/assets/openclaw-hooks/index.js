// The wats nudge hooks for OpenClaw (ADR 0056 §1, §5).
//
// OpenClaw's hooks are plugin code, not commands, so this plugin runs
// `wats hook post-tool|stop --host openclaw` itself, with a JSON payload on
// stdin, and puts what wats prints where the model reads it:
//
// - after a tool call (`after_tool_call`, observe-only), the nudge is kept
//   and added to the tool's result as it is written (`tool_result_persist`),
//   so the model reads it on its next pass; if that already happened, it
//   goes into the next prompt (`before_prompt_build`);
// - at the end of a turn (`before_agent_finalize`), a nudge asks for one
//   more pass (`revise`, at most once).
//
// wats never posts from a hook; it reads its store only and always exits 0.
import { execFileSync } from "node:child_process";

const TIMEOUT_MS = 5000;

/** Run `wats hook <event>`; its stdout, or "" on any failure (logged). */
function wats(config, event, payload, logger) {
  const exe = config.wats || process.env.WATS_BIN || "wats";
  const args = ["hook", event, "--host", "openclaw"];
  if (config.dir) {
    args.push("--dir", config.dir);
  }
  try {
    return execFileSync(exe, args, {
      input: JSON.stringify(payload),
      timeout: TIMEOUT_MS,
      encoding: "utf8",
      stdio: ["pipe", "pipe", "pipe"],
    }).trim();
  } catch (err) {
    // Never the payload: it holds the tool's output.
    const why = (err && (err.code || (err.stderr && String(err.stderr).trim()))) || err;
    logger?.warn?.(`[wats-hooks] \`${exe} hook ${event}\` failed: ${String(why).slice(0, 300)}`);
    return "";
  }
}

/**
 * The plugin's config: what OpenClaw resolved at registration
 * (`api.pluginConfig`), else what a hook event carries. OpenClaw
 * 2026.6.35 does not put `event.context.pluginConfig` on typed hooks.
 */
function configOf(api, event) {
  return (
    (api && api.pluginConfig) ||
    (event && event.context && event.context.pluginConfig) ||
    {}
  );
}

/** The wats node's own agent runs for the swarm: no nudge there. */
function skipped(config, ctx) {
  const skip = config.skipAgents || [];
  return Boolean(ctx && ctx.agentId && skip.includes(ctx.agentId));
}

/** Add `text` to a tool-result message, as model-visible content. */
function withNudge(message, text) {
  if (!message || typeof message !== "object") {
    return undefined;
  }
  const note = { type: "text", text: `\n\n${text}` };
  if (Array.isArray(message.content)) {
    return { ...message, content: [...message.content, note] };
  }
  if (typeof message.content === "string") {
    return { ...message, content: `${message.content}\n\n${text}` };
  }
  return { ...message, content: [note] };
}

export default {
  id: "wats-hooks",
  name: "wats hooks",
  description:
    "Nudges the agent toward WeAreTheSwarm at the right moment (ADR 0056). Never posts.",
  register(api) {
    // Nudges waiting to reach the model, by tool call, then by session.
    const byCall = new Map();
    const bySession = new Map();
    const logger = api.logger;
    const start = configOf(api);
    logger?.info?.(
      `[wats-hooks] registered: wats ${start.wats || "from PATH"}, dir ${start.dir || "default"}, skip ${(start.skipAgents || []).join(",") || "none"}`,
    );

    api.on("after_tool_call", async (event, ctx) => {
      const config = configOf(api, event);
      if (skipped(config, ctx)) {
        return;
      }
      const session = (ctx && (ctx.sessionId || ctx.sessionKey)) || "default";
      logger?.debug?.(`[wats-hooks] after_tool_call ${event.toolName} (${session})`);
      const text = wats(config, "post-tool", {
        session_id: session,
        hook_event_name: "after_tool_call",
        tool_name: event.toolName,
        tool_input: event.params || {},
        tool_response: event.result,
        error: event.error,
      }, logger);
      const callId = event.toolCallId || (ctx && ctx.toolCallId);
      if (text) {
        if (callId) {
          byCall.set(callId, { text, session });
        } else {
          bySession.set(session, text);
        }
      }
    });

    api.on("tool_result_persist", (event, ctx) => {
      const callId = event.toolCallId || (ctx && ctx.toolCallId);
      const pending = callId && byCall.get(callId);
      if (!pending) {
        return undefined;
      }
      byCall.delete(callId);
      const message = withNudge(event.message, pending.text);
      if (!message) {
        bySession.set(pending.session, pending.text);
        return undefined;
      }
      return { message };
    });

    api.on("before_prompt_build", async (_event, ctx) => {
      const session = (ctx && (ctx.sessionId || ctx.sessionKey)) || "default";
      const leftover = [];
      for (const [id, pending] of byCall) {
        if (pending.session === session) {
          leftover.push(pending.text);
          byCall.delete(id);
        }
      }
      if (bySession.has(session)) {
        leftover.push(bySession.get(session));
        bySession.delete(session);
      }
      return leftover.length ? { appendContext: leftover.join("\n\n") } : undefined;
    });

    api.on("before_agent_finalize", async (event, ctx) => {
      const config = configOf(api, event);
      if (event.stopHookActive || skipped(config, ctx)) {
        return undefined;
      }
      logger?.debug?.("[wats-hooks] before_agent_finalize");
      const text = wats(config, "stop", {
        session_id: event.sessionId || event.sessionKey || "default",
        hook_event_name: "before_agent_finalize",
        stop_hook_active: false,
      }, logger);
      if (!text) {
        return undefined;
      }
      return {
        action: "revise",
        reason: text,
        retry: {
          instruction: text,
          idempotencyKey: `wats-stop-${event.turnId || event.runId || event.sessionId}`,
          maxAttempts: 1,
        },
      };
    });
  },
};
