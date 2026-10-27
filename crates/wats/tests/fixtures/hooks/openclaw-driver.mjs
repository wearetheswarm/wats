// Drives the wats OpenClaw plugin as OpenClaw 2026.6.35 calls it: the
// plugin config on `api.pluginConfig` only (typed hook events carry no
// `context`); `before_tool_call` with the session in its context, then the
// tool-result middleware with only `{runtime}` as context; `exec` results
// as `{content, details: {status, exitCode}}`.
// Usage: node openclaw-driver.mjs <index.js> <wats> <data dir>
// Prints one JSON object: what reached the model, and the plugin's logs.
import { pathToFileURL } from "node:url";

const [plugin, wats, dir] = process.argv.slice(2);
const { default: entry } = await import(pathToFileURL(plugin).href);

const hooks = {};
const middlewares = [];
const logs = [];
const log = (level) => (msg) => logs.push(`${level}: ${msg}`);
entry.register({
  id: "wats-hooks",
  pluginConfig: { wats, dir, skipAgents: ["wats"] },
  logger: { info: log("info"), warn: log("warn"), error: log("error"), debug: log("debug") },
  on: (name, handler) => {
    hooks[name] = handler;
  },
  registerAgentToolResultMiddleware: (handler, options) => {
    middlewares.push({ handler, runtimes: options && options.runtimes });
  },
});

const failure = {
  content: [
    {
      type: "text",
      text: "error[E0599]: no method named `frobnicate` found for struct `Widget` in the current scope\n\n(Command exited with code 101)",
    },
  ],
  details: { status: "completed", exitCode: 101, durationMs: 40 },
};

/** One tool call; the text the model reads back. */
async function tool(agentId, id, toolName, args, result) {
  const ctx = { agentId, sessionId: `oc-${agentId}`, sessionKey: `agent:${agentId}:main`, toolName, toolCallId: id };
  await hooks.before_tool_call({ toolName, params: args, toolCallId: id }, ctx);
  let current = result;
  for (const m of middlewares) {
    const out = await m.handler({ toolCallId: id, toolName, args, result: current }, { runtime: "openclaw", harness: "openclaw" });
    if (out && out.result) {
      current = out.result;
    }
  }
  return current.content.map((c) => c.text).join("");
}

const out = { runtimes: middlewares.map((m) => m.runtimes), hooks: Object.keys(hooks) };
out.seen = [
  await tool("main", "t1", "exec", { command: "cargo build" }, failure),
  await tool("main", "t2", "exec", { command: "cargo build" }, failure),
  await tool("main", "t3", "edit", { path: "src/w.rs", oldText: "a", newText: "b" }, { content: [{ type: "text", text: "ok" }] }),
  await tool("main", "t4", "exec", { command: "cargo build" }, {
    content: [{ type: "text", text: "Finished" }],
    details: { status: "completed", exitCode: 0 },
  }),
  // The node's own agent hears nothing.
  await tool("wats", "w1", "exec", { command: "cargo build" }, failure),
];
out.logs = logs;
console.log(JSON.stringify(out));
