#!/usr/bin/env python3
"""A stand-in ACP agent for the daemon's engine tests. Prints its arguments
and $FAKE_ACP_GREETING into the first reply. With $FAKE_ACP_STORE it keeps
its conversations in that folder and reopens or copies them (`session/load`
and `session/fork` replay one), and numbers its replies by turn.

Run as `opencode` (a copy of this file by that name) it plays OpenCode 1.x:
`opencode debug config` prints $FAKE_OPENCODE_CONFIG, `initialize` names it
(version $FAKE_OPENCODE_VERSION), and each reply says which agent ran it
(`session/set_config_option` mode) and what the daemon's ask agent does with
`rm` commands."""
import json, os, sys

STORE = os.environ.get("FAKE_ACP_STORE", "")
OPENCODE = os.path.basename(sys.argv[0]) == "opencode"

if OPENCODE and sys.argv[1:3] == ["debug", "config"]:
    print("starting opencode")
    print(os.environ.get("FAKE_OPENCODE_CONFIG", "{}"))
    sys.exit(0)

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

def replay(session, turns):
    for user, reply in turns:
        update(session, "user_message_chunk", user)
        update(session, "agent_message_chunk", reply)

def ask(kind, title):
    """Asks for permission; True when allowed."""
    out({"jsonrpc": "2.0", "id": 50, "method": "session/request_permission", "params": {"toolCall": {"title": title, "kind": kind}, "options": [{"optionId": "ok", "kind": "allow_once"}, {"optionId": "no", "kind": "reject_once"}]}})
    while True:
        answer = read()
        if answer is None:
            sys.exit(0)
        if answer.get("id") == 50:
            return answer["result"]["outcome"].get("optionId") == "ok"

def rm_rule():
    try:
        config = json.loads(os.environ.get("OPENCODE_CONFIG_CONTENT", "{}"))
        return config["agent"]["aogiri"]["permission"]["bash"]["rm *"]
    except (KeyError, TypeError, ValueError):
        return "none"

session = "agent-1"
mode = "build"
copies = 0
while True:
    msg = read()
    if msg is None:
        break
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": bool(STORE)}}
        if STORE:
            result["agentCapabilities"]["sessionCapabilities"] = {"fork": {}, "resume": {}}
        if OPENCODE:
            result["agentInfo"] = {"name": "OpenCode", "version": os.environ.get("FAKE_OPENCODE_VERSION", "1.19.0")}
        out({"jsonrpc": "2.0", "id": msg["id"], "result": result})
    elif method == "session/new":
        if STORE:
            save(session, [])
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"sessionId": session}})
    elif method in ("session/load", "session/resume"):
        turns = history(msg["params"]["sessionId"]) if STORE else None
        if turns is None:
            out({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32603, "message": "session not found"}})
            continue
        session = msg["params"]["sessionId"]
        if method == "session/load":
            replay(session, turns)
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {}})
    elif method == "session/fork":
        turns = history(msg["params"]["sessionId"]) if STORE else None
        if turns is None:
            out({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32603, "message": "session not found"}})
            continue
        copies += 1
        session = "agent-copy-%d" % copies
        save(session, turns)
        replay(session, turns)
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"sessionId": session}})
    elif method == "session/set_config_option":
        mode = msg["params"]["value"]
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"configOptions": []}})
    elif method == "session/prompt":
        text = msg["params"]["prompt"][0]["text"]
        if "tool" in text:
            status = "completed" if ask("execute", "Run ls") else "failed"
            out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "kind": "execute", "status": status}}})
        if "edit" in text:
            text += " (edited)" if ask("edit", "a.txt") else " (not edited)"
        if STORE:
            turns = history(session) or []
            reply = "ok: %s #%d" % (text, len(turns) + 1)
            save(session, turns + [[text, reply]])
        else:
            reply = "ok: %s %s %s" % (text, " ".join(sys.argv[1:]), os.environ.get("FAKE_ACP_GREETING", ""))
        if OPENCODE:
            reply += " mode=%s rm=%s" % (mode, rm_rule())
        update(session, "agent_message_chunk", reply.strip())
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "end_turn"}})
