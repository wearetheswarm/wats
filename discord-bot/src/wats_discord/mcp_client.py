"""A small MCP client over stdio: `wats mcp`, one JSON-RPC message per line.

Standard library only. It speaks what the bot needs: `initialize`, then
`tools/call`. The server is restarted on the next call if it exits.
"""

import asyncio
import itertools
import json

PROTOCOL_VERSION = "2025-06-18"
CLIENT = {"name": "wats-discord", "version": "0.1.0"}

# One line of the server can be a whole post with its thread.
LINE_LIMIT = 8 * 1024 * 1024


class McpError(Exception):
    """A tool call that failed, or a server that did not answer."""


class McpClient:
    def __init__(self, argv, env=None, timeout=60.0):
        self.argv = list(argv)
        self.env = env
        self.timeout = timeout
        self._proc = None
        self._reader = None
        self._pending = {}
        self._ids = itertools.count(1)
        self._lock = asyncio.Lock()

    async def _start(self):
        self._proc = await asyncio.create_subprocess_exec(
            *self.argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
            env=self.env,
            limit=LINE_LIMIT,
        )
        self._reader = asyncio.create_task(self._read())
        await self._request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": CLIENT,
            },
        )
        await self._send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    async def _read(self):
        proc = self._proc
        try:
            while True:
                line = await proc.stdout.readline()
                if not line:
                    break
                try:
                    message = json.loads(line)
                except ValueError:
                    continue
                future = self._pending.pop(message.get("id"), None)
                if future is not None and not future.done():
                    future.set_result(message)
        finally:
            for future in self._pending.values():
                if not future.done():
                    future.set_exception(McpError("the wats MCP server exited"))
            self._pending.clear()

    async def _send(self, message):
        self._proc.stdin.write(json.dumps(message).encode() + b"\n")
        await self._proc.stdin.drain()

    async def _request(self, method, params):
        request_id = next(self._ids)
        future = asyncio.get_running_loop().create_future()
        self._pending[request_id] = future
        await self._send(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        )
        try:
            message = await asyncio.wait_for(future, self.timeout)
        except asyncio.TimeoutError:
            self._pending.pop(request_id, None)
            raise McpError(f"{method}: no answer in {self.timeout:.0f} s") from None
        if "error" in message:
            raise McpError(message["error"].get("message", "error"))
        return message.get("result", {})

    async def call(self, tool, arguments):
        """The structured result of one tool call (a dict)."""
        async with self._lock:
            if self._proc is None or self._proc.returncode is not None:
                await self._start()
        result = await self._request("tools/call", {"name": tool, "arguments": arguments})
        if result.get("isError"):
            raise McpError(text_of(result) or f"{tool} failed")
        if isinstance(result.get("structuredContent"), dict):
            return result["structuredContent"]
        try:
            value = json.loads(text_of(result))
        except ValueError:
            raise McpError(f"{tool}: not a JSON result") from None
        return value if isinstance(value, dict) else {"value": value}

    async def close(self):
        if self._proc is not None and self._proc.returncode is None:
            self._proc.stdin.close()
            try:
                await asyncio.wait_for(self._proc.wait(), 5)
            except asyncio.TimeoutError:
                self._proc.kill()
        if self._reader is not None:
            self._reader.cancel()


def text_of(result):
    """The text blocks of a tool result, joined."""
    return "\n".join(
        block.get("text", "")
        for block in result.get("content", [])
        if isinstance(block, dict) and block.get("type") == "text"
    )
