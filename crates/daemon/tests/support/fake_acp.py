#!/usr/bin/env python3
"""A stand-in ACP agent for the daemon's engine tests. Prints its arguments
and $FAKE_ACP_GREETING into the first reply. With $FAKE_ACP_STORE it keeps
its conversations in that folder and reopens or copies them (`session/load`
and `session/fork` replay one), and numbers its replies by turn.

Run as `opencode` (a copy of this file by that name) it plays OpenCode 1.x:
`opencode debug config` prints $FAKE_OPENCODE_CONFIG, `initialize` names it
(version $FAKE_OPENCODE_VERSION), and each reply says which agent ran it
(`session/set_config_option` or `session/set_mode`), what the daemon's ask
agent does with `rm` commands, and whether the prompt carried a block for the
model only (the plan reminder). The agents it knows are build, plan and those
of $OPENCODE_CONFIG_CONTENT, or $FAKE_OPENCODE_AGENTS (comma-separated).
$FAKE_OPENCODE_MODES: `set_mode` has no session config options (OpenCode
1.0), `none` cannot switch at all. With $FAKE_ACP_STORE it writes the config
it was started with to opencode-config.json there."""
import json, os, sys

STORE = os.environ.get("FAKE_ACP_STORE", "")
OPENCODE = os.path.basename(sys.argv[0]) == "opencode"
MODES = os.environ.get("FAKE_OPENCODE_MODES", "")

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

def config():
    try:
        return json.loads(os.environ.get("OPENCODE_CONFIG_CONTENT", "{}"))
    except ValueError:
        return {}

def rm_rule():
    try:
        return config()["agent"]["aogiri"]["permission"]["bash"]["rm *"]
    except (KeyError, TypeError):
        return "none"

def agents():
    names = os.environ.get("FAKE_OPENCODE_AGENTS")
    if names is not None:
        return names.split(",")
    return ["build", "plan"] + list((config().get("agent") or {}).keys())

def unknown_method(msg):
    out({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "Method not found"}})

# Run as `omp` (tests/support/fake_omp.sh starts it with `acp ...`), it plays
# oh-my-pi: its config options (model, effort, a boolean), a sign-in it needs
# until `authenticate` ($FAKE_ACP_AUTH set), the MCP servers it was given
# (named in its reply), form elicitations ("form" in a prompt) and its
# approval mode from --approval-mode.
OMP = "--approval-mode" in sys.argv
AUTH = os.environ.get("FAKE_ACP_AUTH")
signed_in = [not AUTH]
omp_config = [
    {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "lynshen/one",
     "options": [{"value": "lynshen/one", "name": "One"}, {"value": "lynshen/two", "name": "Two"}]},
    {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": "auto",
     "options": [{"value": "low", "name": "Low"}, {"value": "auto", "name": "Auto"}, {"value": "high", "name": "High"}]},
    {"id": "compact", "name": "Auto compact", "type": "boolean", "currentValue": True},
]
mcp_names = []

if OPENCODE and STORE and sys.argv[1:2] == ["acp"]:
    with open(os.path.join(STORE, "opencode-config.json"), "w") as f:
        f.write(os.environ.get("OPENCODE_CONFIG_CONTENT", "{}"))

session = "agent-1"
mode = "build"
copies = 0
while True:
    msg = read()
    if msg is None:
        break
    method = msg.get("method")
    if OMP and method in ("session/new", "session/load", "session/resume") and not signed_in[0]:
        out({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32000, "message": "Authentication required"}})
        continue
    if OMP and method in ("session/new", "session/load", "session/resume"):
        mcp_names = [m.get("name") for m in msg["params"].get("mcpServers", [])]
    if method == "authenticate":
        signed_in[0] = True
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {}})
        continue
    if OMP and method == "session/set_config_option":
        for option in omp_config:
            if option["id"] == msg["params"]["configId"]:
                option["currentValue"] = msg["params"]["value"]
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"configOptions": omp_config}})
        continue
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": bool(STORE)}}
        if OMP:
            result["agentInfo"] = {"name": "omp", "title": "oh-my-pi", "version": "18.1.21"}
            result["authMethods"] = [{"id": "agent", "name": "omp login", "description": "Run omp login"}]
            result["agentCapabilities"]["mcpCapabilities"] = {"http": False}
        if STORE:
            result["agentCapabilities"]["sessionCapabilities"] = {"fork": {}, "resume": {}}
        if OPENCODE:
            result["agentInfo"] = {"name": "OpenCode", "version": os.environ.get("FAKE_OPENCODE_VERSION", "1.19.0")}
        out({"jsonrpc": "2.0", "id": msg["id"], "result": result})
    elif method == "session/new":
        if STORE:
            save(session, [])
        result = {"sessionId": session}
        if OMP:
            result["configOptions"] = omp_config
        out({"jsonrpc": "2.0", "id": msg["id"], "result": result})
    elif method in ("session/load", "session/resume"):
        turns = history(msg["params"]["sessionId"]) if STORE else None
        if turns is None:
            out({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32603, "message": "session not found"}})
            continue
        session = msg["params"]["sessionId"]
        if method == "session/load":
            replay(session, turns)
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"configOptions": omp_config} if OMP else {}})
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
        if MODES in ("set_mode", "none"):
            unknown_method(msg)
            continue
        value = msg["params"]["value"]
        if value not in agents():
            out({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32602, "message": "Invalid params: mode not found: " + value}})
            continue
        mode = value
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"configOptions": [{"id": "mode", "category": "mode", "type": "select", "currentValue": mode, "options": [{"value": a, "name": a} for a in agents()]}]}})
    elif method == "session/set_mode":
        if MODES == "none":
            unknown_method(msg)
            continue
        value = msg["params"]["modeId"]
        if value not in agents():
            out({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32603, "message": "Agent not found: " + value}})
            continue
        mode = value
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {}})
    elif method == "session/prompt":
        text = msg["params"]["prompt"][0]["text"]
        if "tool" in text:
            status = "completed" if ask("execute", "Run ls") else "failed"
            out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": {"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "ls", "kind": "execute", "status": status}}})
        if "edit" in text:
            text += " (edited)" if ask("edit", "a.txt") else " (not edited)"
        if "form" in text:
            out({"jsonrpc": "2.0", "id": 60, "method": "elicitation/create", "params": {"sessionId": session, "mode": "form",
                 "message": "Allow tool: write\nFile: b.txt",
                 "requestedSchema": {"type": "object", "properties": {"value": {"type": "string", "enum": ["Approve", "Deny"]}}}}})
            while True:
                answer = read()
                if answer is None:
                    sys.exit(0)
                if answer.get("id") == 60:
                    break
            result = answer["result"]
            text += " form=%s:%s" % (result.get("action"), (result.get("content") or {}).get("value"))
        if STORE:
            turns = history(session) or []
            reply = "ok: %s #%d" % (text, len(turns) + 1)
            save(session, turns + [[text, reply]])
        else:
            reply = "ok: %s %s %s" % (text, " ".join(sys.argv[1:]), os.environ.get("FAKE_ACP_GREETING", ""))
        if OMP:
            approval = sys.argv[sys.argv.index("--approval-mode") + 1]
            model = [o["currentValue"] for o in omp_config if o["id"] == "model"][0]
            reply += " omp=%s model=%s mcp=%s" % (approval, model, ",".join(mcp_names))
        if OPENCODE:
            reply += " mode=%s rm=%s" % (mode, rm_rule())
            if any((b.get("annotations") or {}).get("audience") == ["assistant"] for b in msg["params"]["prompt"]):
                reply += " reminded"
        update(session, "agent_message_chunk", reply.strip())
        out({"jsonrpc": "2.0", "id": msg["id"], "result": {"stopReason": "end_turn"}})
