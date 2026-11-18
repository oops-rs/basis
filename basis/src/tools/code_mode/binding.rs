//! A model-authored function runs inside one exclusive Mentra tool call.
//! Only the turn dispatcher can resolve and execute the names it sees.

use async_trait::async_trait;
use mentra::tool::{
    RuntimeToolDescriptor, ToolContext, ToolDefinition, ToolExecutionCategory, ToolExecutor,
    ToolOutput,
};
use serde_json::{Value, json};

pub(crate) struct CodeTool;

impl ToolDefinition for CodeTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder("code")
            .description(concat!(
                "Run a fresh async JavaScript function body. Use await tools[name](args) ",
                "with the session's tool names and JSON input schemas. Await every call. ",
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
        let result = super::evaluate(source.to_owned(), names, |mut calls| async move {
            while let Some(call) = calls.recv().await {
                let child = ctx.invoke_child(&call.name, call.arguments).await;
                let answer = if child.is_error {
                    Err(format!(
                        "{}: {}",
                        call.name,
                        child.content.to_display_string()
                    ))
                } else {
                    Ok(child.content)
                };
                let _ = call.reply.send(answer);
            }
        })
        .await?;
        Ok(ToolOutput::structured(result))
    }
}
