// The wats nudge hooks for OpenClaw (ADR 0056 §1, §5).
//
// OpenClaw's hooks are plugin code, not commands, so this plugin runs
// `wats hook post-tool|stop --host openclaw` itself, with a JSON payload on
// stdin, and adds what wats prints to the tool result the model reads next:
//
// - `before_tool_call` notes each call's session and agent (the tool-result
//   middleware gets neither);
// - the tool-result middleware runs `wats hook post-tool`, and after a
//   command that passed, `wats hook stop` (the fix found), and appends the
//   nudge to that result, in the same run. OpenClaw 2026.6.35 refuses
//   `before_agent_finalize` revisions after a tool with side effects
//   (every fix), and `tool_result_persist` only changes the stored
//   transcript, so neither reaches the model.
//
// wats never posts from a hook; it reads its store only and always exits 0.
// A failure here never changes the tool's result.
import { execFileSync } from "node:child_process";

const TIMEOUT_MS = 5000;
const MAX_PENDING = 512;
const SHELLS = new Set(["exec", "bash", "shell", "process"]);

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

/** A tool result with `text` added, as model-visible content. */
function withNudge(result, text) {
  const note = { type: "text", text: `\n\n${text}` };
  const content = Array.isArray(result && result.content) ? result.content : [];
  return { ...result, content: [...content, note] };
}

/** Did this shell result pass? Its exit code, else no error flag. */
function passed(event) {
  const details = (event.result && event.result.details) || {};
  if (typeof details.exitCode === "number") {
    return details.exitCode === 0;
  }
  return event.isError !== true && details.status !== "failed";
}

export default {
  id: "wats-hooks",
  name: "wats hooks",
  description:
    "Nudges the agent toward WeAreTheSwarm at the right moment (ADR 0056). Never posts.",
  register(api) {
    const logger = api.logger;
    const start = configOf(api);
    // Each tool call's session and agent, from before_tool_call.
    const calls = new Map();
    logger?.info?.(
      `[wats-hooks] registered: wats ${start.wats || "from PATH"}, dir ${start.dir || "default"}, skip ${(start.skipAgents || []).join(",") || "none"}`,
    );

    api.on("before_tool_call", (event, ctx) => {
      const id = event.toolCallId || (ctx && ctx.toolCallId);
      if (!id) {
        return undefined;
      }
      calls.set(id, {
        session: (ctx && (ctx.sessionId || ctx.sessionKey)) || "default",
        agent: ctx && ctx.agentId,
      });
      while (calls.size > MAX_PENDING) {
        calls.delete(calls.keys().next().value);
      }
      return undefined;
    });

    const middleware = (event) => {
      try {
        const call = calls.get(event.toolCallId);
        if (!call) {
          return undefined;
        }
        calls.delete(event.toolCallId);
        const config = configOf(api, event);
        if (call.agent && (config.skipAgents || []).includes(call.agent)) {
          return undefined;
        }
        logger?.debug?.(`[wats-hooks] tool result ${event.toolName} (${call.session})`);
        const said = [];
        const tool = wats(
          config,
          "post-tool",
          {
            session_id: call.session,
            hook_event_name: "tool_result",
            tool_name: event.toolName,
            tool_input: event.args || {},
            tool_response: event.result,
          },
          logger,
        );
        if (tool) {
          said.push(tool);
        }
        if (SHELLS.has(event.toolName) && passed(event)) {
          const fix = wats(
            config,
            "stop",
            { session_id: call.session, hook_event_name: "tool_result", stop_hook_active: false },
            logger,
          );
          if (fix) {
            said.push(fix);
          }
        }
        if (said.length === 0) {
          return undefined;
        }
        return { result: withNudge(event.result, said.join("\n\n")) };
      } catch (err) {
        logger?.warn?.(`[wats-hooks] tool result: ${String(err).slice(0, 300)}`);
        return undefined;
      }
    };
    api.registerAgentToolResultMiddleware(middleware, { runtimes: ["openclaw", "codex"] });
  },
};
