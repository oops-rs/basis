//! A model-authored function runs inside one exclusive Mentra tool call.
//! Only the turn dispatcher can resolve and execute the names it sees. Calls
//! the script starts together are dispatched together, in the lanes the
//! dispatcher gives the calls of one model response.

use std::time::Duration;

use async_trait::async_trait;
use mentra::tool::{
    RuntimeToolDescriptor, ToolContext, ToolDefinition, ToolExecutionCategory, ToolExecutor,
    ToolOutput,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy)]
pub(crate) struct CodeTool {
    script_time: Duration,
}

impl CodeTool {
    pub(crate) fn new(script_time: Option<Duration>) -> Self {
        Self {
            script_time: script_time.unwrap_or(super::SCRIPT_TIME),
        }
    }
}

impl ToolDefinition for CodeTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder("code")
            .description(concat!(
                "Run a fresh async JavaScript function body. Use await tools[name](args) ",
                "with the session's tool names and JSON input schemas. Await every call. ",
                "Calls started together, as with Promise.all, run like the calls of one ",
                "response: read-only tools in parallel, other tools one at a time in order. ",
                "Tool errors throw. Text stays text. Use JSON.parse for documented JSON text. ",
                "Return the JSON value you want to read."
            ))
            .input_schema(json!({
                "type": "object",
                "properties": {"source": {"type": "string", "description": "Async JavaScript function body."}},
                "required": ["source"],
                "additionalProperties": false
            }))
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for CodeTool {
    async fn execute_mut_output(
        &self,
        mut ctx: ToolContext<'_>,
        input: Value,
    ) -> Result<ToolOutput, String> {
        let source = input["source"]
            .as_str()
            .ok_or("code requires a source string")?;
        let names = ctx
            .visible_tool_specs()
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        let drive = |mut calls: mpsc::Receiver<super::Batch>| async move {
            while let Some(batch) = calls.recv().await {
                let (requests, replies): (Vec<_>, Vec<_>) = batch
                    .into_iter()
                    .map(|call| ((call.name.clone(), call.arguments), (call.name, call.reply)))
                    .unzip();
                let children = ctx.invoke_children(requests).await;
                for ((name, reply), child) in replies.into_iter().zip(children) {
                    let answer = if child.is_error {
                        Err(format!("{name}: {}", child.content.to_display_string()))
                    } else {
                        Ok(child.content)
                    };
                    let _ = reply.send(answer);
                }
            }
        };
        let result = super::evaluate(source.to_owned(), names, drive, self.script_time).await?;
        Ok(ToolOutput::structured(result))
    }
}
