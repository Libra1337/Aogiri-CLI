#!/usr/bin/env python3
"""A stand-in for `claude --print --input-format stream-json ...`: enough of
the stream-json protocol for the daemon's engine tests. Every start appends
its arguments to $FAKE_CLAUDE_LOG; turns are saved like Claude Code saves
them, under <config>/projects/<munged cwd>/<session id>.jsonl, where config
is $CLAUDE_CONFIG_DIR or ~/.claude."""
import json, os, re, sys

args = sys.argv[1:]
with open(os.environ["FAKE_CLAUDE_LOG"], "a") as log:
    log.write(json.dumps(args) + "\n")

def arg(flag):
    return args[args.index(flag) + 1] if flag in args else None

# A fork resumes another conversation into a new one under --session-id.
fork = "--fork-session" in args
source = arg("--resume") if fork else None
session = arg("--session-id") if fork else (arg("--resume") or arg("--session-id"))
mode = "bypassPermissions" if "--dangerously-skip-permissions" in args else (arg("--permission-mode") or "default")
cwd = os.getcwd()
config = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.join(os.environ["HOME"], ".claude")
saved = os.path.join(config, "projects", re.sub(r"[^A-Za-z0-9]", "-", cwd))
os.makedirs(saved, exist_ok=True)
# Like Claude Code: a saved conversation can only be resumed.
if "--session-id" in args and os.path.exists(os.path.join(saved, session + ".jsonl")):
    sys.stderr.write("Error: Session ID " + session + " is already in use.\n")
    sys.exit(1)

# A fork copies the source's saved rows up to --resume-session-at.
if source:
    at = arg("--resume-session-at")
    found = None
    for d in os.listdir(os.path.join(config, "projects")):
        path = os.path.join(config, "projects", d, source + ".jsonl")
        if os.path.exists(path):
            found = path
    if found is None:
        sys.stderr.write("No conversation found with session ID: " + source + "\n")
        sys.exit(1)
    copied = []
    for line in open(found):
        copied.append(line)
        if at and json.loads(line).get("uuid") == at:
            break
    pending_copy = copied
    last = [json.loads(copied[-1]).get("uuid")] if copied else [None]
else:
    pending_copy = None
    last = [None]
uuids = [0]
# A resumed conversation goes on from its last row.
if not source and os.path.exists(os.path.join(saved, session + ".jsonl")):
    for line in open(os.path.join(saved, session + ".jsonl")):
        uuids[0] += 1
        last[0] = json.loads(line).get("uuid") or last[0]

def out(frame):
    sys.stdout.write(json.dumps(frame) + "\n")
    sys.stdout.flush()

def save(role, text):
    global pending_copy
    if "--no-session-persistence" in args:
        return
    path = os.path.join(saved, session + ".jsonl")
    # Like Claude Code, a fork's file is written with its first turn.
    if pending_copy is not None:
        with open(path, "a") as f:
            f.writelines(pending_copy)
        pending_copy = None
    uuids[0] += 1
    row = {"type": role, "uuid": "%s-%d" % (session[:8], uuids[0]), "parentUuid": last[0], "message": {"content": text}}
    last[0] = row["uuid"]
    with open(path, "a") as f:
        f.write(json.dumps(row) + "\n")

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

# Without --print it stands in for Claude Code's TUI: each line typed is a
# turn saved to the conversation, until "exit".
if "--print" not in args:
    out_line = lambda text: (sys.stdout.write(text + "\r\n"), sys.stdout.flush())
    out_line("TUI " + session)
    for line in sys.stdin:
        line = line.strip()
        if line == "exit":
            break
        if line:
            save("user", line)
            save("assistant", "tui: " + line)
            out_line("tui: " + line)
    sys.exit(0)

while True:
    frame = read()
    if frame is None:
        break
    if frame["type"] == "control_request":
        request = frame["request"]
        response = {}
        if request["subtype"] == "set_permission_mode":
            mode = request["mode"]
            response = {"mode": mode}
        elif request["subtype"] == "list_models":
            response = {"models": [{"value": "sonnet", "resolvedModel": "claude-sonnet-4-5", "displayName": "Sonnet"}]}
        elif request["subtype"] == "initialize":
            response = {"commands": [{"name": "compact", "description": "Free up context", "argumentHint": "", "builtin": True}], "pid": os.getpid(), "user_output_styles_dir": os.path.join(config, "output-styles")}
        out({"type": "control_response", "response": {"subtype": "success", "request_id": frame["request_id"], "response": response}})
        continue
    if frame["type"] != "user":
        continue
    text = frame["message"]["content"][0]["text"]
    out({"type": "user", "isReplay": True, "message": frame["message"]})
    out({"type": "system", "subtype": "init", "session_id": session, "model": "claude-sonnet-4-5", "permissionMode": mode, "cwd": cwd, "slash_commands": ["compact"]})
    out({"type": "system", "subtype": "status", "status": "requesting"})
    save("user", text)
    if "tool" in text:
        out({"type": "control_request", "request_id": "perm-1", "request": {"subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "tool-1", "input": {"command": "echo hi"}}})
        while True:
            answer = read()
            if answer is None:
                sys.exit(0)
            if answer["type"] == "control_response":
                break
        allowed = answer["response"]["response"]["behavior"] == "allow"
        out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "hi" if allowed else "denied", "is_error": not allowed}]}, "tool_use_result": {"stdout": "hi"} if allowed else {}})
    # "api": one Messages request where its settings send it (a gateway or
    # custom provider session's --settings file), with the key they hold, as
    # Claude Code would; the reply says the status.
    if text.startswith("api"):
        import urllib.request, urllib.error
        env = json.load(open(arg("--settings")))["env"] if arg("--settings") else {}
        model = env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL") or arg("--model") or "claude-sonnet-4-5"
        body = json.dumps({"model": model, "max_tokens": 16, "stream": False, "messages": [{"role": "user", "content": text}]}).encode()
        request = urllib.request.Request(env.get("ANTHROPIC_BASE_URL", "") + "/v1/messages?beta=true", data=body, method="POST", headers={
            "Authorization": "Bearer " + env.get("ANTHROPIC_AUTH_TOKEN", ""), "Content-Type": "application/json",
            "anthropic-version": "2023-06-01", "X-LynShen-Device": "v1;hw=engine"})
        try:
            status = urllib.request.urlopen(request, timeout=20).status
        except urllib.error.HTTPError as error:
            status = error.code
        except Exception as error:
            status = "failed " + str(error)
        text = "%s %s" % (text, status)
    reply = "mode: " + mode if text == "which mode" else "ok: " + text
    out({"type": "stream_event", "event": {"type": "message_start", "message": {"usage": {"input_tokens": 5}}}})
    out({"type": "assistant", "uuid": "u-1", "message": {"content": [{"type": "text", "text": reply}]}})
    if text.startswith("api"):
        out({"type": "stream_event", "event": {"type": "message_delta", "usage": {"input_tokens": 11, "output_tokens": 2}}})
    save("assistant", reply)
    out({"type": "result", "subtype": "success", "total_cost_usd": 0.01})
