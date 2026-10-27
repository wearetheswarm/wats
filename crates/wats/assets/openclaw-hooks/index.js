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

/** Run `wats hook <event>`; its stdout, or "" on any failure. */
function wats(config, event, payload) {
  const exe = (config && config.wats) || process.env.WATS_BIN || "wats";
  const args = ["hook", event, "--host", "openclaw"];
  if (config && config.dir) {
    args.push("--dir", config.dir);
  }
  try {
    return execFileSync(exe, args, {
      input: JSON.stringify(payload),
      timeout: TIMEOUT_MS,
      encoding: "utf8",
      stdio: ["pipe", "pipe", "ignore"],
    }).trim();
  } catch {
    return "";
  }
}

function pluginConfig(event) {
  return (event && event.context && event.context.pluginConfig) || {};
}

/** The wats node's own agent runs for the swarm: no nudge there. */
function skipped(event, ctx) {
  const skip = pluginConfig(event).skipAgents || [];
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

    api.on("after_tool_call", async (event, ctx) => {
      if (skipped(event, ctx)) {
        return;
      }
      const session = (ctx && (ctx.sessionId || ctx.sessionKey)) || "default";
      const text = wats(pluginConfig(event), "post-tool", {
        session_id: session,
        hook_event_name: "after_tool_call",
        tool_name: event.toolName,
        tool_input: event.params || {},
        tool_response: event.result,
        error: event.error,
      });
      if (text) {
        if (event.toolCallId) {
          byCall.set(event.toolCallId, { text, session });
        } else {
          bySession.set(session, text);
        }
      }
    });

    api.on("tool_result_persist", (event) => {
      const pending = event.toolCallId && byCall.get(event.toolCallId);
      if (!pending) {
        return undefined;
      }
      byCall.delete(event.toolCallId);
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
      if (event.stopHookActive || skipped(event, ctx)) {
        return undefined;
      }
      const text = wats(pluginConfig(event), "stop", {
        session_id: event.sessionId || event.sessionKey || "default",
        hook_event_name: "before_agent_finalize",
        stop_hook_active: false,
      });
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
