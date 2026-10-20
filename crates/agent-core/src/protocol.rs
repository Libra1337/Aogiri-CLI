//! The JSON wire format shared by `lynshen serve` and the daemon: engine
//! events serialize to `{"type": ...}` objects and client ops arrive as
//! `{"op": ...}` objects. See `docs/serve-protocol.md`.

use crate::{event::AgentEvent, AgentCore, ApprovalMode, ApproveOptions};
use serde_json::{json, Value};

/// Wire protocol version, announced in the `hello` event. Clients refuse to
/// talk to a different version instead of guessing at field meanings.
pub const PROTOCOL_VERSION: u64 = 2;

/// First frame on every connection: the protocol version and the engine
/// build, so a client can refuse a mismatch before reading anything else.
pub fn hello_json(version: &str) -> Value {
    json!({ "type": "hello", "protocol": PROTOCOL_VERSION, "version": version })
}

/// An engine event on the wire, tagged with the session it belongs to so
/// one connection can carry several sessions.
pub fn session_event_json(session: &str, event: AgentEvent) -> Value {
    let mut value = event_json(event);
    value["session"] = json!(session);
    value
}

/// Applies one parsed client op to the engine. Returns `(quit, events)`;
/// `quit` is set by `shutdown` and `/quit`.
pub fn apply_op(core: &mut AgentCore, value: &Value) -> (bool, Vec<AgentEvent>) {
    let op = value.get("op").and_then(Value::as_str).unwrap_or_default();
    let events = match op {
        "user_message" => {
            let content = value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let images = value
                .get("images")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            core.submit_user_message_with_images(content.to_string(), images)
        }
        "command" => {
            let input = value
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return core.handle_command(input);
        }
        // A failed turn again, without a new user message (see continue_turn).
        "continue" => core.continue_turn(),
        "steer" => core.steer(),
        "unqueue" => core.unqueue(
            value
                .get("index")
                .and_then(Value::as_u64)
                .map_or(usize::MAX, |index| index as usize),
            value.get("text").and_then(Value::as_str),
        ),
        "interrupt" => core.interrupt(),
        // Structured twin of the `/approve` text command (GUI convenience):
        // {"op":"approve","call_id":"...","decision":"allow|deny",
        //  "scope":"once|session|project","prefix":"npm install",
        //  "feedback":"...","hunks":["f0h1"],"always":false}.
        "approve" => match parse_approve_op(value) {
            Ok((call_id, allow, options)) => core.approve_with(&call_id, allow, options),
            Err(error) => vec![AgentEvent::Error(error)],
        },
        "set_approval_mode" => {
            let mode = value
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match ApprovalMode::parse(mode) {
                Ok(mode) => core.set_approval_mode(mode),
                Err(error) => vec![AgentEvent::Error(error)],
            }
        }
        // {"op":"add_directory","path":"/abs","mode":"rw","persist":"session"}
        "add_directory" => {
            let text = |key: &str, default: &'static str| {
                value
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or(default)
                    .to_string()
            };
            core.add_directory(
                &text("path", ""),
                &text("mode", "rw"),
                &text("persist", "session"),
            )
        }
        // {"op":"remove_directory","path":"/abs","persist":"session"}
        "remove_directory" => {
            let text = |key: &str, default: &'static str| {
                value
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or(default)
                    .to_string()
            };
            core.remove_directory(&text("path", ""), &text("persist", "session"))
        }
        // {"op":"set_sandbox","mode":"workspace-write","network":true}
        "set_sandbox" => {
            let mode = value
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match crate::sandbox::SandboxMode::parse(mode) {
                Ok(mode) => {
                    core.set_sandbox_mode(mode, value.get("network").and_then(Value::as_bool))
                }
                Err(error) => vec![AgentEvent::Error(error)],
            }
        }
        // {"op":"set_style","style":"concise"}; null or "" follows the
        // default for new conversations.
        "set_style" => match value.get("style") {
            None | Some(Value::Null) => core.set_reply_style(None),
            Some(Value::String(id)) => core.set_reply_style(Some(id.as_str())),
            Some(_) => vec![AgentEvent::Error(
                "set_style requires style: a style id or null".to_string(),
            )],
        },
        "set_attended" => match value.get("attended").and_then(Value::as_bool) {
            Some(attended) => core.set_attended(attended),
            None => vec![AgentEvent::Error(
                "set_attended requires attended: true or false".to_string(),
            )],
        },
        // The action id travels as `action`: `id` is a client's request id.
        "decide_action" => match (
            value.get("action").and_then(Value::as_str),
            value.get("decision").and_then(Value::as_str),
        ) {
            (Some(id), Some("allow")) => core.decide_action(id, true),
            (Some(id), Some("deny")) => core.decide_action(id, false),
            _ => vec![AgentEvent::Error(
                "decide_action requires action and decision: allow or deny".to_string(),
            )],
        },
        "agent_runs" => vec![core.agent_runs_event()],
        // The desktop's merge / discard buttons: {"op":"merge_agent",
        // "target":"/root/worker","action":"apply|discard"}.
        "merge_agent" => {
            let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
            core.merge_agent(text("target"), text("action"))
        }
        // The desktop's stop button: {"op":"close_agent","target":"/root/w"}.
        "close_agent" => core.close_agent(
            value
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        // best-of-N: {"op":"pick_attempt","group":"fix","target":"fix_a2"}.
        "pick_attempt" => {
            let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
            core.pick_attempt(text("group"), text("target"))
        }
        "subagent_transcript" => {
            let id = value
                .get("agent_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            vec![core.subagent_transcript_event(id)]
        }
        "approve_plan" => {
            let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default();
            let mode = match value.get("mode").and_then(Value::as_str) {
                Some(mode) => match ApprovalMode::parse(mode) {
                    Ok(mode) => Some(mode),
                    Err(error) => return (false, vec![AgentEvent::Error(error)]),
                },
                None => None,
            };
            match (text("id"), text("decision")) {
                ("", _) => vec![AgentEvent::Error("approve_plan requires id".to_string())],
                (id, "approve") => core.approve_plan(id, true, mode, text("feedback")),
                (id, "revise") => core.approve_plan(id, false, None, text("feedback")),
                _ => vec![AgentEvent::Error(
                    "approve_plan requires decision: approve or revise".to_string(),
                )],
            }
        }
        "mcp_list" => vec![core.mcp_servers_event()],
        "context_usage" => vec![core.context_breakdown_event()],
        "mcp_set" => match value.get("server") {
            Some(server) => core.mcp_set(server),
            None => vec![AgentEvent::Error(
                "mcp_set requires a server object".to_string(),
            )],
        },
        "mcp_remove" => match value.get("name").and_then(Value::as_str) {
            Some(name) => core.mcp_remove(name),
            None => vec![AgentEvent::Error("mcp_remove requires name".to_string())],
        },
        "mcp_toggle" => {
            match (
                value.get("name").and_then(Value::as_str),
                value.get("enabled").and_then(Value::as_bool),
            ) {
                (Some(name), Some(enabled)) => core.mcp_toggle(name, enabled),
                _ => vec![AgentEvent::Error(
                    "mcp_toggle requires name and enabled".to_string(),
                )],
            }
        }
        "shutdown" => return (true, Vec::new()),
        other => {
            crate::log_warn!("serve", "unknown op", op = other);
            vec![AgentEvent::Error(format!("unknown op: {other}"))]
        }
    };
    (false, events)
}

/// `(call_id, allow, options)` parsed from the serve `approve` op.
type ParsedApprove = (String, bool, ApproveOptions);

/// Parses the serve `approve` op into `(call_id, allow, options)`.
/// Combination rules (scope vs hunks, unknown ids, the prefix) are
/// validated by `AgentCore::approve_with` so text and structured paths
/// behave identically.
fn parse_approve_op(value: &Value) -> Result<ParsedApprove, String> {
    let call_id = value
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| "approve requires call_id".to_string())?;
    let allow = match value.get("decision").and_then(Value::as_str) {
        Some("allow") => true,
        Some("deny") => false,
        _ => return Err("approve requires decision: allow or deny".to_string()),
    };
    let always = value
        .get("always")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let hunks = match value.get("hunks") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            let mut ids = Vec::new();
            for item in items {
                match item.as_str().map(str::trim) {
                    Some(id) if !id.is_empty() => ids.push(id.to_string()),
                    _ => return Err("hunks must be an array of hunk id strings".to_string()),
                }
            }
            Some(ids)
        }
        Some(_) => return Err("hunks must be an array of hunk id strings".to_string()),
    };
    let scope = match value.get("scope") {
        None | Some(Value::Null) => None,
        Some(scope) => Some(crate::approval::ApprovalScope::parse(
            scope.as_str().unwrap_or_default(),
        )?),
    };
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let add_dir = match value.get("add_dir") {
        None | Some(Value::Null) => None,
        Some(dir) => {
            let path = dir
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| !path.trim().is_empty())
                .ok_or_else(|| "add_dir requires path".to_string())?;
            let persist = dir
                .get("persist")
                .and_then(Value::as_str)
                .unwrap_or("session");
            Some((std::path::PathBuf::from(path.trim()), persist.to_string()))
        }
    };
    Ok((
        call_id.to_string(),
        allow,
        ApproveOptions {
            scope,
            always,
            prefix: text("prefix"),
            feedback: text("feedback"),
            hunks,
            add_dir,
        },
    ))
}

pub fn event_json(event: AgentEvent) -> Value {
    match event {
        AgentEvent::Startup {
            version,
            session_id,
            profile_dir,
            config_path,
            cwd,
            model,
            context_window,
        } => {
            json!({
                "type": "startup",
                "version": version,
                "session_id": session_id,
                "profile_dir": profile_dir,
                "config_path": config_path,
                "cwd": cwd,
                "model": model,
                "context_window": context_window
            })
        }
        AgentEvent::ModelStatus {
            provider,
            model,
            model_label,
            reasoning_effort,
            context_window,
            context_limit,
            max_output_tokens,
            reasoning_efforts,
            input_modalities,
            state,
        } => json!({
            "type": "model_status",
            "provider": provider,
            "model": model,
            "model_label": model_label,
            "reasoning_effort": reasoning_effort,
            "context_window": context_window,
            "context_limit": context_limit,
            "max_output_tokens": max_output_tokens,
            "reasoning_efforts": reasoning_efforts,
            "input_modalities": input_modalities,
            "state": state
        }),
        AgentEvent::TurnStats(stats) => {
            let mut event = stats.to_json();
            event["type"] = json!("turn_stats");
            event
        }
        AgentEvent::PendingMessages(messages) => {
            json!({ "type": "pending_messages", "messages": messages })
        }
        AgentEvent::Unqueued(text) => json!({ "type": "unqueued", "text": text }),
        AgentEvent::UserMessage(content) => json!({ "type": "user_message", "content": content }),
        AgentEvent::FillInput(content) => json!({ "type": "fill_input", "content": content }),
        AgentEvent::Connecting => json!({ "type": "connecting" }),
        AgentEvent::CompactionStart => json!({ "type": "compaction_start" }),
        AgentEvent::CompactionProgress { output_tokens } => {
            json!({ "type": "compaction_progress", "output_tokens": output_tokens })
        }
        AgentEvent::CompactionEnd { summary } => {
            json!({ "type": "compaction_end", "summary": summary })
        }
        AgentEvent::CompactionFailed(error) => {
            json!({ "type": "compaction_failed", "error": error })
        }
        AgentEvent::ContextUsage {
            tokens,
            tokenizer,
            cost,
            breakdown,
        } => {
            let mut event = json!({ "type": "context_usage", "tokens": tokens, "tokenizer": tokenizer, "cost": cost });
            if let Some(b) = breakdown {
                event["breakdown"] = json!({
                    "system_prompt": b.system_prompt,
                    "skills": b.skills,
                    "system_tools": b.system_tools,
                    "mcp_tools": b.mcp_tools,
                    "messages": b.messages,
                });
            }
            event
        }
        AgentEvent::ContextBreakdown {
            total,
            max,
            categories,
            memory_files,
            error,
        } => match error {
            Some(error) => json!({ "type": "context_breakdown", "error": error }),
            None => json!({
                "type": "context_breakdown",
                "total": total,
                "max": max,
                "categories": categories.iter().map(|c| json!({ "name": c.name, "tokens": c.tokens, "kind": c.kind })).collect::<Vec<_>>(),
                "memory_files": memory_files.iter().map(|(path, tokens)| json!({ "path": path, "tokens": tokens })).collect::<Vec<_>>(),
            }),
        },
        AgentEvent::ThinkingStart => json!({ "type": "thinking_start" }),
        AgentEvent::ReasoningDelta(delta) => {
            json!({ "type": "reasoning_delta", "delta": delta })
        }
        AgentEvent::AssistantStart => json!({ "type": "assistant_start" }),
        AgentEvent::AssistantDelta(delta) => {
            json!({ "type": "assistant_delta", "delta": delta })
        }
        AgentEvent::ResponseInterrupted => json!({ "type": "response_interrupted" }),
        AgentEvent::Retrying {
            attempt,
            max_attempts,
            reason,
            delay_ms,
        } => json!({
            "type": "retrying",
            "attempt": attempt,
            "max_attempts": max_attempts,
            "reason": reason,
            "delay_ms": delay_ms,
        }),
        AgentEvent::ToolStart { call_id, name } => {
            json!({ "type": "tool_start", "call_id": call_id, "name": name })
        }
        AgentEvent::ToolUpdate {
            call_id,
            name,
            output,
        } => {
            json!({ "type": "tool_update", "call_id": call_id, "name": name, "output": output })
        }
        AgentEvent::ToolOutput {
            call_id,
            name,
            output,
            is_error,
            ..
        } => json!({
            "type": "tool_output",
            "call_id": call_id,
            "name": name,
            "output": output,
            "is_error": is_error
        }),
        AgentEvent::SubagentLifecycle {
            path,
            status,
            message,
            label,
            model,
            tool_use_id,
            role,
            plan_step,
            background,
            attempt_group,
            attempt,
        } => json!({
            "type": "subagent_lifecycle",
            "path": path,
            "status": status,
            "message": message,
            "label": label,
            "model": model,
            "tool_use_id": tool_use_id,
            "role": role,
            "plan_step": plan_step,
            "background": background,
            "attempt_group": attempt_group,
            "attempt": attempt,
        }),
        AgentEvent::AgentMessage { from, to, summary } => json!({
            "type": "agent_message",
            "from": from,
            "to": to,
            "summary": summary,
        }),
        AgentEvent::MergeResult {
            target,
            action,
            ok,
            files,
            conflicts,
            error,
        } => json!({
            "type": "merge_result",
            "target": target,
            "action": action,
            "ok": ok,
            "files": files,
            "conflicts": conflicts,
            "error": error,
        }),
        AgentEvent::TeamBudget { used, limit } => json!({
            "type": "team_budget",
            "used": used,
            "limit": limit,
        }),
        AgentEvent::AgentRuns(agents) => json!({
            "type": "agent_runs",
            "workflows": [],
            "agents": agents,
        }),
        AgentEvent::TaskBoard(tasks) => json!({ "type": "task_board", "tasks": tasks }),
        AgentEvent::SubagentTranscript { agent_id, items } => match items {
            Some(items) => {
                json!({ "type": "subagent_transcript", "agent_id": agent_id, "items": items })
            }
            None => json!({
                "type": "subagent_transcript",
                "agent_id": agent_id,
                "error": format!("unknown agent: {agent_id}"),
            }),
        },
        AgentEvent::Usage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
            reasoning_tokens,
        } => {
            json!({ "type": "usage", "input_tokens": input_tokens, "cached_input_tokens": cached_input_tokens, "output_tokens": output_tokens, "reasoning_tokens": reasoning_tokens })
        }
        AgentEvent::TreeView(nodes) => json!({
            "type": "tree_view",
            "nodes": nodes.into_iter().map(|node| {
                json!({ "id": node.id, "parent_id": node.parent_id, "label": node.label, "active": node.active })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::ResumeView(items) => json!({
            "type": "resume_view",
            "items": items.into_iter().map(|item| {
                json!({ "id": item.id, "label": item.label, "active": item.active })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::CheckpointView(items) => json!({
            "type": "checkpoint_view",
            "items": items.into_iter().map(|item| {
                json!({ "id": item.id, "label": item.label, "detail": item.detail })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::McpServers { servers } => json!({
            "type": "mcp_servers",
            "servers": servers.into_iter().map(|server| {
                let mut entry = json!({
                    "name": server.name,
                    "transport": server.transport,
                    "state": server.state,
                    "tools": server.tools.into_iter().map(|tool| {
                        json!({ "name": tool.name, "description": tool.description })
                    }).collect::<Vec<_>>(),
                });
                if let Some(error) = server.error {
                    entry["error"] = json!(error);
                }
                entry
            }).collect::<Vec<_>>()
        }),
        AgentEvent::ApprovalRequest {
            call_id,
            name,
            summary,
            subagent_id,
            hunks,
            info,
        } => {
            let mut value = json!({
                "type": "approval_request",
                "call_id": call_id,
                "name": name,
                "summary": summary,
                "subagent_id": subagent_id,
                // Edit tools: the selectable hunks of the pending change; answer
                // with the `approve` op (or /approve --hunks) to apply a subset.
                "hunks": hunks.map(|hunks| hunks.into_iter().map(|hunk| json!({
                    "id": hunk.id,
                    "file": hunk.file,
                    "header": hunk.header,
                    "lines": hunk.lines,
                })).collect::<Vec<_>>()),
            });
            info.wire_fields(&mut value);
            value
        }
        AgentEvent::ApprovalMode { mode } => json!({
            "type": "approval_mode",
            "mode": mode,
        }),
        AgentEvent::TrustPrompt { cwd, repo_root } => json!({
            "type": "trust_prompt",
            "cwd": cwd,
            "repo_root": repo_root,
        }),
        AgentEvent::ModelView {
            models,
            active_effort,
        } => json!({
            "type": "model_view",
            "active_effort": active_effort,
            "models": models.into_iter().map(|model| {
                json!({
                    "model": model.model,
                    "label": model.label,
                    "active": model.active,
                    "context_window": model.context_window,
                    "max_output_tokens": model.max_output_tokens,
                    "reasoning_efforts": model.reasoning_efforts,
                    "input_modalities": model.input_modalities
                })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::LoginPicker(providers) => json!({
            "type": "login_picker",
            "providers": providers.into_iter().map(|p| json!({
                "id": p.id,
                "label": p.label,
                "detail": p.detail,
                "active": p.active,
                "wants_key": p.wants_key
            })).collect::<Vec<_>>()
        }),
        AgentEvent::LoginPastePrompt { provider } => json!({
            "type": "login_paste_prompt",
            "provider": provider
        }),
        AgentEvent::CommandList(commands) => json!({
            "type": "command_list",
            "commands": commands.into_iter().map(|command| {
                json!({ "command": command.command, "marker": command.marker, "args": command.args, "description": command.description })
            }).collect::<Vec<_>>()
        }),
        AgentEvent::Goal(goal) => json!({
            "type": "goal",
            "goal": goal.map(|goal| json!({
                "objective": goal.objective,
                "status": goal.status,
                "token_budget": goal.token_budget,
                "tokens_used": goal.tokens_used,
                "time_used_seconds": goal.time_used_seconds,
                "created_at": goal.created_at,
                "updated_at": goal.updated_at,
            }))
        }),
        AgentEvent::Plan(items) => json!({
            "type": "plan",
            "plan": items.into_iter().map(|item| json!({
                "step": item.step,
                "status": item.status,
                "agent": item.agent,
                "files": item.files,
            })).collect::<Vec<_>>()
        }),
        AgentEvent::PlanDraft { id, title, append } => json!({
            "type": "plan_draft",
            "id": id,
            "title": title,
            "append": append,
        }),
        AgentEvent::ProposedPlan {
            id,
            title,
            markdown,
            status,
        } => json!({
            "type": "proposed_plan",
            "id": id,
            "title": title,
            "markdown": markdown,
            "status": status,
        }),
        AgentEvent::Transcript(items) => json!({
            "type": "transcript",
            "items": items.into_iter().map(|item| match item {
                crate::TranscriptItem::User(content) => json!({ "role": "user", "content": content }),
                crate::TranscriptItem::UserWithImages { content, images } => {
                    json!({ "role": "user", "content": content, "images": images })
                }
                crate::TranscriptItem::Assistant(content) => json!({ "role": "assistant", "content": content }),
                crate::TranscriptItem::Tool { name, output } => json!({ "role": "tool", "name": name, "output": output }),
                crate::TranscriptItem::Branch(label) => json!({ "role": "branch", "label": label }),
                crate::TranscriptItem::Compaction { summary } => json!({ "role": "compaction", "summary": summary }),
                crate::TranscriptItem::Plan { id, title, content, status } => json!({
                    "role": "plan", "id": id, "title": title, "content": content, "status": status,
                }),
                crate::TranscriptItem::TurnStats(stats) => {
                    let mut item = stats.to_json();
                    item["role"] = json!("turn_stats");
                    item
                }
            }).collect::<Vec<_>>()
        }),
        AgentEvent::Info(message) => json!({ "type": "info", "message": message }),
        AgentEvent::Error(message) => json!({ "type": "error", "message": message }),
        AgentEvent::Status(message) => json!({ "type": "status", "message": message }),
        AgentEvent::ActionDeferred(action) => {
            let mut value = action.to_json();
            value["type"] = json!("action_deferred");
            value
        }
        AgentEvent::ActionDecided {
            id,
            allow,
            output,
            is_error,
        } => json!({
            "type": "action_decided",
            "id": id,
            "decision": if allow { "allow" } else { "deny" },
            "output": output,
            "is_error": is_error,
        }),
        AgentEvent::Attended(attended) => json!({ "type": "attended", "attended": attended }),
        AgentEvent::Sandbox(view) => json!({
            "type": "sandbox",
            "mode": view.mode,
            "network": view.network,
            "enforced": view.enforced,
            "unavailable": view.unavailable,
            "directories": view.directories.into_iter().map(|dir| json!({
                "path": dir.path,
                "mode": dir.mode,
                "source": dir.source,
            })).collect::<Vec<_>>(),
        }),
        AgentEvent::ReplyStyle(view) => json!({
            "type": "reply_style",
            "style": view.style,
            "name": view.name,
            "source": view.source,
            "chosen": view.chosen,
            "missing": view.missing,
        }),
        AgentEvent::Escalated { call_id, by, info } => json!({
            "type": "escalated",
            "call_id": call_id,
            "kind": info.kind.as_str(),
            "command": info.command,
            "justification": info.justification,
            "cwd": info.cwd.display().to_string(),
            "paths": info.paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
            "by": by,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_usage_carries_the_breakdown_when_known() {
        let plain = event_json(AgentEvent::ContextUsage {
            tokens: 10,
            tokenizer: "gpt-5".to_string(),
            cost: 0.0,
            breakdown: None,
        });
        assert!(plain.get("breakdown").is_none());
        let full = event_json(AgentEvent::ContextUsage {
            tokens: 10,
            tokenizer: "gpt-5".to_string(),
            cost: 0.0,
            breakdown: Some(crate::event::ContextBreakdown {
                system_prompt: 1,
                skills: 2,
                system_tools: 3,
                mcp_tools: 4,
                messages: 10,
            }),
        });
        assert_eq!(
            full["breakdown"],
            json!({ "system_prompt": 1, "skills": 2, "system_tools": 3, "mcp_tools": 4, "messages": 10 })
        );
    }

    #[test]
    fn approve_op_round_trips_decision_always_and_hunks() {
        let op = json!({
            "op": "approve",
            "call_id": "call_9",
            "decision": "allow",
            "hunks": ["f0h1", "f1h2"],
            "always": false,
        });
        assert_eq!(
            parse_approve_op(&op).unwrap(),
            (
                "call_9".to_string(),
                true,
                ApproveOptions {
                    hunks: Some(vec!["f0h1".to_string(), "f1h2".to_string()]),
                    ..ApproveOptions::default()
                }
            )
        );

        let plain_deny = json!({ "op": "approve", "call_id": "call_9", "decision": "deny" });
        assert_eq!(
            parse_approve_op(&plain_deny).unwrap(),
            ("call_9".to_string(), false, ApproveOptions::default())
        );

        let always = json!({
            "op": "approve", "call_id": "call_9", "decision": "allow", "always": true,
            "hunks": null,
        });
        assert_eq!(
            parse_approve_op(&always).unwrap(),
            (
                "call_9".to_string(),
                true,
                ApproveOptions {
                    always: true,
                    ..ApproveOptions::default()
                }
            )
        );

        let scoped = json!({
            "op": "approve", "call_id": "call_9", "decision": "allow",
            "scope": "project", "prefix": "npm install --save",
        });
        assert_eq!(
            parse_approve_op(&scoped).unwrap().2,
            ApproveOptions {
                scope: Some(crate::approval::ApprovalScope::Project),
                prefix: Some("npm install --save".to_string()),
                ..ApproveOptions::default()
            }
        );
        let told = json!({
            "op": "approve", "call_id": "call_9", "decision": "deny",
            "feedback": "Use pnpm instead.",
        });
        assert_eq!(
            parse_approve_op(&told).unwrap().2.feedback.as_deref(),
            Some("Use pnpm instead.")
        );
        let bad =
            json!({ "op": "approve", "call_id": "c", "decision": "allow", "scope": "forever" });
        assert!(parse_approve_op(&bad)
            .unwrap_err()
            .contains("unknown scope"));
        let with_dir = json!({
            "op": "approve", "call_id": "c", "decision": "allow",
            "add_dir": { "path": "/Users/me/Desktop", "persist": "project" },
        });
        assert_eq!(
            parse_approve_op(&with_dir).unwrap().2.add_dir,
            Some((
                std::path::PathBuf::from("/Users/me/Desktop"),
                "project".to_string()
            ))
        );
        let no_path =
            json!({ "op": "approve", "call_id": "c", "decision": "allow", "add_dir": {} });
        assert!(parse_approve_op(&no_path)
            .unwrap_err()
            .contains("add_dir requires path"));
    }

    #[test]
    fn approve_op_rejects_missing_fields_and_bad_hunks() {
        let missing_call = json!({ "op": "approve", "decision": "allow" });
        assert!(parse_approve_op(&missing_call)
            .unwrap_err()
            .contains("call_id"));

        let bad_decision = json!({ "op": "approve", "call_id": "c", "decision": "maybe" });
        assert!(parse_approve_op(&bad_decision)
            .unwrap_err()
            .contains("allow or deny"));

        let bad_hunks =
            json!({ "op": "approve", "call_id": "c", "decision": "allow", "hunks": [1] });
        assert!(parse_approve_op(&bad_hunks)
            .unwrap_err()
            .contains("array of hunk id strings"));

        let bad_hunks_type =
            json!({ "op": "approve", "call_id": "c", "decision": "allow", "hunks": "f0h1" });
        assert!(parse_approve_op(&bad_hunks_type)
            .unwrap_err()
            .contains("array"));
    }

    #[test]
    fn team_events_have_their_wire_shapes() {
        let lifecycle = event_json(AgentEvent::SubagentLifecycle {
            path: "/root/w".to_string(),
            status: "merged".to_string(),
            message: "applied 2 files".to_string(),
            label: "w".to_string(),
            model: "m".to_string(),
            tool_use_id: "call_1".to_string(),
            role: Some("worker".to_string()),
            plan_step: None,
            background: true,
            attempt_group: Some("fix".to_string()),
            attempt: Some(2),
        });
        assert_eq!(lifecycle["type"], "subagent_lifecycle");
        assert_eq!(lifecycle["role"], "worker");
        assert_eq!(lifecycle["plan_step"], Value::Null);
        assert_eq!(lifecycle["background"], true);
        assert_eq!(lifecycle["attempt_group"], "fix");
        assert_eq!(lifecycle["attempt"], 2);
        assert_eq!(
            event_json(AgentEvent::TaskBoard(vec![json!({ "id": "t1" })])),
            json!({ "type": "task_board", "tasks": [{ "id": "t1" }] })
        );
        assert_eq!(
            event_json(AgentEvent::AgentMessage {
                from: "/root/w".to_string(),
                to: "/root".to_string(),
                summary: "which key?".to_string(),
            }),
            json!({ "type": "agent_message", "from": "/root/w", "to": "/root", "summary": "which key?" })
        );
        assert_eq!(
            event_json(AgentEvent::MergeResult {
                target: "/root/w".to_string(),
                action: "apply".to_string(),
                ok: false,
                files: vec!["a.rs".to_string()],
                conflicts: vec!["a.rs".to_string()],
                error: None,
            }),
            json!({
                "type": "merge_result", "target": "/root/w", "action": "apply", "ok": false,
                "files": ["a.rs"], "conflicts": ["a.rs"], "error": null
            })
        );
        assert_eq!(
            event_json(AgentEvent::TeamBudget {
                used: 320_000,
                limit: 400_000
            }),
            json!({ "type": "team_budget", "used": 320000, "limit": 400000 })
        );
        let plan = event_json(AgentEvent::Plan(vec![crate::PlanItem {
            step: "Add parser".to_string(),
            status: "in_progress".to_string(),
            agent: Some("/root/parser".to_string()),
            files: vec!["src/parse.rs".to_string()],
        }]));
        assert_eq!(
            plan["plan"][0],
            json!({ "step": "Add parser", "status": "in_progress", "agent": "/root/parser", "files": ["src/parse.rs"] })
        );
    }

    #[test]
    fn sandbox_and_escalated_events_have_their_wire_shapes() {
        let sandbox = event_json(AgentEvent::Sandbox(crate::event::SandboxView {
            mode: "workspace-write".to_string(),
            network: true,
            enforced: false,
            unavailable: None,
            directories: vec![crate::event::SandboxDirectoryView {
                path: "/srv/x".to_string(),
                mode: "rw",
                source: "project",
            }],
        }));
        assert_eq!(
            sandbox,
            json!({
                "type": "sandbox", "mode": "workspace-write", "network": true,
                "enforced": false, "unavailable": null,
                "directories": [{ "path": "/srv/x", "mode": "rw", "source": "project" }],
            })
        );
        let escalated = event_json(AgentEvent::Escalated {
            call_id: "call_3".to_string(),
            by: "mode",
            info: crate::approval::ApprovalInfo {
                kind: crate::approval::ApprovalKind::OutsideWrite,
                justification: Some("save the chart".to_string()),
                cwd: std::path::PathBuf::from("/work"),
                command: Some("cp chart.png ~/Desktop/".to_string()),
                paths: vec![std::path::PathBuf::from("/home/me/Desktop")],
                ..Default::default()
            },
        });
        assert_eq!(
            escalated,
            json!({
                "type": "escalated", "call_id": "call_3", "kind": "outside_write",
                "command": "cp chart.png ~/Desktop/", "justification": "save the chart",
                "cwd": "/work", "paths": ["/home/me/Desktop"], "by": "mode",
            })
        );
    }

    #[test]
    fn approval_request_event_serializes_hunks_for_the_wire() {
        let event = AgentEvent::ApprovalRequest {
            call_id: "call_7".to_string(),
            name: "apply_patch".to_string(),
            summary: "src/lib.rs".to_string(),
            subagent_id: None,
            info: crate::approval::ApprovalInfo::default(),
            hunks: Some(vec![crate::HunkView {
                id: "f0h1".to_string(),
                file: "src/lib.rs".to_string(),
                header: "@@ -1,3 +1,3 @@".to_string(),
                lines: vec![" a".to_string(), "-b".to_string(), "+B".to_string()],
            }]),
        };

        let value = event_json(event);
        assert_eq!(value["type"], "approval_request");
        assert_eq!(value["hunks"][0]["id"], "f0h1");
        assert_eq!(value["hunks"][0]["file"], "src/lib.rs");
        assert_eq!(value["hunks"][0]["header"], "@@ -1,3 +1,3 @@");
        assert_eq!(value["hunks"][0]["lines"][2], "+B");

        let plain = AgentEvent::ApprovalRequest {
            call_id: "call_8".to_string(),
            name: "bash".to_string(),
            summary: "git push".to_string(),
            subagent_id: None,
            hunks: None,
            info: crate::approval::ApprovalInfo {
                kind: crate::approval::ApprovalKind::Rule,
                must_ask: true,
                cwd: std::path::PathBuf::from("/work"),
                command: Some("git push".to_string()),
                scopes: vec![crate::approval::ApprovalScope::Once],
                ..Default::default()
            },
        };
        let plain = event_json(plain);
        assert_eq!(plain["hunks"], Value::Null);
        assert_eq!(plain["kind"], "rule");
        assert_eq!(plain["must_ask"], true);
        assert_eq!(plain["command"], "git push");
        assert_eq!(plain["cwd"], "/work");
        assert_eq!(plain["scopes"], json!(["once"]));
        assert_eq!(plain["paths"], json!([]));
        assert_eq!(plain["prefix"], Value::Null);
        assert_eq!(plain["elevation"], Value::Null);
    }
}
