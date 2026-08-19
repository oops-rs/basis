#!/usr/bin/env python3
"""Drive basis's ACP server over real stdio, as a client would.

    BASIS_API_KEY=... python3 scripts/acp-smoke.py

Speaks the protocol by hand rather than through a library, so what it proves
is that the bytes on the wire are right — not that two of our own pieces
agree with each other.
"""

import json
import os
import subprocess
import sys
import threading

BASE_URL = os.environ.get("BASIS_BASE_URL", "http://127.0.0.1:3455/v1")
MODEL = os.environ.get("BASIS_MODEL", "gpt-5.6-sol")
BINARY = os.environ.get("BASIS_BIN", "./target/debug/basis")


def main() -> int:
    agent = subprocess.Popen(
        [
            BINARY,
            "serve",
            "--acp",
            "--base-url",
            BASE_URL,
            "--model",
            MODEL,
            "--approve",
            "always",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )

    # Surface the agent's own diagnostics rather than swallowing them.
    threading.Thread(
        target=lambda: [sys.stderr.write(f"  [basis] {line}") for line in agent.stderr],
        daemon=True,
    ).start()

    next_id = iter(range(1, 1000))
    updates = []

    def send(method, params):
        request_id = next(next_id)
        agent.stdin.write(
            json.dumps({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
            + "\n"
        )
        agent.stdin.flush()

        # Notifications arrive interleaved with the response we are waiting for.
        while True:
            line = agent.stdout.readline()
            if not line:
                raise SystemExit("agent closed the connection")
            message = json.loads(line)
            if message.get("method") == "session/update":
                updates.append(message["params"]["update"])
                continue
            if message.get("id") == request_id:
                if "error" in message:
                    raise SystemExit(f"{method} failed: {message['error']}")
                return message["result"]

    initialized = send("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
    print(f"initialize   -> {initialized['agentInfo']}")
    assert initialized["agentCapabilities"]["loadSession"], "basis resumes sessions"

    session = send("session/new", {"cwd": os.path.abspath("/tmp/lanlive"), "mcpServers": []})
    session_id = session["sessionId"]
    print(f"session/new  -> {session_id}")

    first = send(
        "session/prompt",
        {
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "Remember the number 41. Just acknowledge."}],
        },
    )
    print(f"prompt 1     -> {first['stopReason']}")

    before = len(updates)
    second = send(
        "session/prompt",
        {
            "sessionId": session_id,
            "prompt": [
                {"type": "text", "text": "What number did I ask you to remember? Digits only."}
            ],
        },
    )
    print(f"prompt 2     -> {second['stopReason']}")

    answer = "".join(
        update["content"]["text"]
        for update in updates[before:]
        if update.get("sessionUpdate") == "agent_message_chunk"
    )
    print(f"streamed     -> {answer.strip()!r}")

    agent.stdin.close()
    agent.wait(timeout=10)

    if "41" not in answer:
        print("\nFAIL: the second turn did not recall the first over ACP")
        return 1

    print("\nOK: two turns over real stdio ACP, streamed back as session/update")
    return 0


if __name__ == "__main__":
    sys.exit(main())
