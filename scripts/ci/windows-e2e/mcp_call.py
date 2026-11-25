"""A minimal MCP client for CI: opens a session on Recouvrance's MCP server, then prints the
tool list or one tool's text answer. Exits 1 when the server answers with an error.

    python mcp_call.py PORT TOKEN tools/list
    python mcp_call.py PORT TOKEN list_resources context=c kind=pods
"""

import json
import sys
import urllib.request


def post(url, token, body, session=None):
    headers = {
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    }
    if session:
        headers["Mcp-Session-Id"] = session
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers, method="POST")
    with urllib.request.urlopen(req, timeout=120) as resp:
        session = resp.headers.get("Mcp-Session-Id", session)
        if "id" not in body:
            return None, session
        if resp.headers.get_content_type() == "application/json":
            return json.load(resp), session
        # An event stream: the answer is the `data:` line that carries this request's id.
        for raw in resp:
            line = raw.decode("utf-8").strip()
            if line.startswith("data:") and line[5:].strip():
                msg = json.loads(line[5:])
                if msg.get("id") == body["id"]:
                    return msg, session
    raise SystemExit(f"no answer to {body['method']}")


def main():
    port, token, what = sys.argv[1], sys.argv[2], sys.argv[3]
    # `key=value` arguments rather than JSON: no quotes to get through PowerShell.
    args = dict(a.split("=", 1) for a in sys.argv[4:])
    url = f"http://127.0.0.1:{port}/mcp"
    init = {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "ci-windows", "version": "1"},
        },
    }
    answer, session = post(url, token, init)
    if "error" in answer:
        raise SystemExit(f"initialize: {answer['error']}")
    post(url, token, {"jsonrpc": "2.0", "method": "notifications/initialized"}, session)
    if what == "tools/list":
        answer, _ = post(url, token, {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}, session)
        if "error" in answer:
            raise SystemExit(f"tools/list: {answer['error']}")
        for tool in answer["result"]["tools"]:
            print(tool["name"])
        return
    call = {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": what, "arguments": args}}
    answer, _ = post(url, token, call, session)
    if "error" in answer:
        raise SystemExit(f"{what}: {answer['error']}")
    result = answer["result"]
    print("\n".join(c.get("text", "") for c in result.get("content", [])))
    if result.get("isError"):
        sys.exit(1)


if __name__ == "__main__":
    main()
