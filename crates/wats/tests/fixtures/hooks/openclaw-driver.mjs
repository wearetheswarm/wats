// Drives the wats OpenClaw plugin as OpenClaw 2026.6.35 calls it: the
// plugin config on `api.pluginConfig` only (typed hook events carry no
// `context`), `exec` results as `{content, details: {status, exitCode}}`.
// Usage: node openclaw-driver.mjs <index.js> <wats> <data dir>
// Prints one JSON object: what reached the model, and the plugin's logs.
import { pathToFileURL } from "node:url";

const [plugin, wats, dir] = process.argv.slice(2);
const { default: entry } = await import(pathToFileURL(plugin).href);

const hooks = {};
const logs = [];
const log = (level) => (msg) => logs.push(`${level}: ${msg}`);
entry.register({
  id: "wats-hooks",
  pluginConfig: { wats, dir, skipAgents: ["wats"] },
  logger: { info: log("info"), warn: log("warn"), error: log("error"), debug: log("debug") },
  on: (name, handler) => {
    hooks[name] = handler;
  },
});

const ctx = { agentId: "main", sessionId: "oc-s1", sessionKey: "agent:main:main" };
const failure = {
  content: [
    {
      type: "text",
      text: "error[E0599]: no method named `frobnicate` found for struct `Widget` in the current scope\n\n(Command exited with code 101)",
    },
  ],
  details: { status: "completed", exitCode: 101, durationMs: 40 },
};
const out = { persisted: [], prompt: null, finalize: null, skipped: null };

async function tool(id, toolName, params, result) {
  await hooks.after_tool_call({ toolName, params, toolCallId: id, result }, { ...ctx, toolName, toolCallId: id });
  const message = { role: "toolResult", toolCallId: id, content: [{ type: "text", text: "tool output" }] };
  const persisted = hooks.tool_result_persist({ toolName, toolCallId: id, message }, { agentId: "main", toolCallId: id });
  out.persisted.push(persisted ? persisted.message.content.map((c) => c.text).join("") : null);
}

await tool("t1", "exec", { command: "cargo build" }, failure);
await tool("t2", "exec", { command: "cargo build" }, failure);
await tool("t3", "edit", { path: "src/w.rs", oldText: "a", newText: "b" }, { content: [{ type: "text", text: "ok" }] });
await tool("t4", "exec", { command: "cargo build" }, {
  content: [{ type: "text", text: "Finished" }],
  details: { status: "completed", exitCode: 0 },
});
out.prompt = (await hooks.before_prompt_build({ prompt: "next" }, ctx)) || null;
out.finalize =
  (await hooks.before_agent_finalize?.({ sessionId: "oc-s1", stopHookActive: false, turnId: "u1" }, ctx)) || null;
// The node's own agent hears nothing.
await hooks.after_tool_call(
  { toolName: "exec", params: { command: "cargo build" }, toolCallId: "w1", result: failure },
  { agentId: "wats", sessionId: "oc-w", toolCallId: "w1" },
);
out.skipped = hooks.tool_result_persist({ toolCallId: "w1", message: { content: [] } }, { agentId: "wats" }) || null;
out.logs = logs;
console.log(JSON.stringify(out));
