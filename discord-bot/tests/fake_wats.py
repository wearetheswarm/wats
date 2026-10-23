"""A fake `wats mcp` for the tests: answers initialize and tools/call."""

import json
import sys

POST = {"id": "a" * 64, "kind": "answer", "title": "Hi", "text": "x", "ts": 0}

for line in sys.stdin:
    message = json.loads(line)
    if "id" not in message:
        continue
    method = message["method"]
    if method == "initialize":
        result = {"protocolVersion": message["params"]["protocolVersion"], "capabilities": {}}
    elif message["params"]["name"] == "search":
        result = {"structuredContent": {"posts": [POST], "args": message["params"]["arguments"]}}
    elif message["params"]["name"] == "read":
        result = {"content": [{"type": "text", "text": json.dumps({"post": POST, "thread": []})}]}
    elif message["params"]["name"] == "exit":
        sys.exit(0)
    else:
        result = {"isError": True, "content": [{"type": "text", "text": "no such tool"}]}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}) + "\n")
    sys.stdout.flush()
