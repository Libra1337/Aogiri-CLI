#!/usr/bin/env python3
"""A stand-in ACP agent for the daemon's engine tests. Prints its arguments
and $FAKE_ACP_GREETING into the first reply. With $FAKE_ACP_STORE it keeps
its conversations in that folder and reopens them (`session/load` replays
one), and numbers its replies by turn."""
import json, os, sys

STORE = os.environ.get("FAKE_ACP_STORE", "")

def out(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

def history(session):
    try:
        with open(os.path.join(STORE, session + ".json")) as f:
            return json.load(f)
    except OSError:
        return None

def save(session, turns):
    with open(os.path.join(STORE, session + ".json"), "w") as f:
        json.dump(turns, f)

def update(session, kind, text):
    out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": kind, "content": {"type": "text", "text": text}}}})

session = "agent-1"
while True:
    msg = read()
    if msg is None:
        break
    method = msg.get("method")
    if method == "initialize":
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": bool(STORE)}}})
    elif method == "session/new":
        if STORE:
            save(session, [])
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"sessionId": session}})
    elif method == "session/load":
        turns = history(msg["params"]["sessionId"]) if STORE else None
        if turns is None:
            out({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32603, "message": "session not found"}})
            continue
        session = msg["params"]["sessionId"]
        for user, reply in turns:
            update(session, "user_message_chunk", user)
            update(session, "agent_message_chunk", reply)
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {}})
    elif method == "session/prompt":
        text = msg["params"]["prompt"][0]["text"]
        if "tool" in text:
            out({"jsonrpc": "2.0", "id": 50, "method": "session/request_permission", "params": {"toolCall": {"title": "Run ls", "kind": "execute"}, "options": [{"optionId": "ok", "kind": "allow_once"}, {"optionId": "no", "kind": "reject_once"}]}})
            while True:
                answer = read()
                if answer is None:
                    sys.exit(0)
                if answer.get("id") == 50:
                    break
            status = "completed" if answer["result"]["outcome"].get("optionId") == "ok" else "failed"
            out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "kind": "execute", "status": status}}})
        if STORE:
            turns = history(session) or []
            reply = "ok: %s #%d" % (text, len(turns) + 1)
            save(session, turns + [[text, reply]])
        else:
            reply = "ok: %s %s %s" % (text, " ".join(sys.argv[1:]), os.environ.get("FAKE_ACP_GREETING", ""))
        update(session, "agent_message_chunk", reply.strip())
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "end_turn"}})
