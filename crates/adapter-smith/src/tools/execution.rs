//! Shared smith tool execution primitives used by multiple harness modes.

use crate::hooks::Hooks;
use crate::provider::ToolCall;
use crate::tools::{ToolCtx, ToolOutcome, ToolRegistry};
use agentd_protocol::adapter::EventEmitter;
use agentd_protocol::SessionEvent;
use serde_json::json;

pub struct PreparedToolCall {
    pub call: ToolCall,
    pub args_summary: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn prepare_tool_call(
    call: &ToolCall,
    registry: &ToolRegistry,
    tool_ctx: &ToolCtx,
    emit: &EventEmitter,
    hooks: &crate::hooks::Hooks,
    base_hook_payload: &serde_json::Value,
) -> std::result::Result<PreparedToolCall, String> {
    let mut call = call.clone();
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            emit.emit(SessionEvent::ToolUse {
                tool: call.name.clone(),
                args: call.input.clone(),
                call_id: Some(call.id.clone()),
            });
            let msg = format!("unknown tool: {}", call.name);
            emit.emit(SessionEvent::ToolResult {
                tool: call.name.clone(),
                ok: false,
                output: msg.clone(),
                call_id: Some(call.id.clone()),
            });
            return Err(msg);
        }
    };

    let mutation = hooks
        .mutate(
            "pre_tool_use_mutate",
            &tool_ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": tool.args_summary(&call.input),
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;
    if let Some(args) = mutation.get("args") {
        call.input = args.clone();
    }
    let args_summary = tool.args_summary(&call.input);
    emit.emit(SessionEvent::ToolUse {
        tool: call.name.clone(),
        args: call.input.clone(),
        call_id: Some(call.id.clone()),
    });
    hooks
        .run(
            "pre_tool_use",
            &tool_ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": args_summary,
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;

    Ok(PreparedToolCall { call, args_summary })
}

pub async fn emit_tool_result_events(
    call_id: &str,
    tool_name: &str,
    emit: &EventEmitter,
    outcome: &std::result::Result<ToolOutcome, String>,
    emit_task_end: bool,
    skip_task_end_if_output_is: Option<&str>,
) -> (bool, String) {
    let (ok, output) = match outcome {
        Ok(o) => (o.ok, o.output.clone()),
        Err(reason) => (false, format!("({reason})")),
    };
    emit.emit(SessionEvent::ToolResult {
        tool: tool_name.to_string(),
        ok,
        output: output.clone(),
        call_id: Some(call_id.to_string()),
    });
    if emit_task_end && skip_task_end_if_output_is.is_none_or(|skip| output != skip) {
        let preview: String = output.chars().take(200).collect();
        emit.emit(SessionEvent::TaskEnd {
            call_id: call_id.to_string(),
            ok,
            output_preview: preview,
        });
    }
    (ok, output)
}

pub fn extract_tool_outcome(
    outcome: &std::result::Result<ToolOutcome, String>,
) -> (bool, String) {
    match outcome {
        Ok(o) => (o.ok, o.output.clone()),
        Err(reason) => (false, format!("({reason})")),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn emit_post_tool_use(
    call: &ToolCall,
    tool_ctx: &ToolCtx,
    emit: &EventEmitter,
    hooks: &Hooks,
    base_hook_payload: &serde_json::Value,
    outcome: &std::result::Result<ToolOutcome, String>,
) {
    let (ok, output) = extract_tool_outcome(outcome);
    hooks
        .run(
            "post_tool_use",
            &tool_ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "ok": ok,
                    "output": crate::tools::truncate_for_model(&output, 8_000),
                }),
            ),
        )
        .await;
}
