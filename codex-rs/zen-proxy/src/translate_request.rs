//! Translate between OAI Responses API and Anthropic Messages API request/response formats.

use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use uuid::Uuid;

/// Convert an OAI Responses API request body into an Anthropic Messages API body.
pub(crate) fn oai_to_anthropic(oai: &Value) -> Value {
    let model = oai["model"].as_str().unwrap_or("");
    let is_stream = oai["stream"].as_bool().unwrap_or(false);
    let max_tokens = oai["max_output_tokens"].as_u64().unwrap_or(16384);

    let instructions = oai["instructions"].as_str();
    let input = oai["input"].as_array();

    let (system_from_input, messages) = convert_input_to_messages(input);
    let system = instructions.or(system_from_input.as_deref());

    let tools = convert_tools(oai["tools"].as_array());

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": is_stream,
    });

    if let Some(sys) = system {
        body["system"] = Value::String(sys.to_string());
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }

    body
}

/// Convert a non-streaming Anthropic response into OAI Responses format.
pub(crate) fn anthropic_response_to_oai(ant: &Value, model: &str) -> Value {
    let resp_id = format!("resp_{}", Uuid::new_v4().simple());
    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let mut output = Vec::new();
    if let Some(content) = ant["content"].as_array() {
        for block in content {
            match block["type"].as_str() {
                Some("text") => {
                    let msg_id = format!("msg_{}", Uuid::new_v4().simple());
                    let text = block["text"].as_str().unwrap_or("");
                    output.push(json!({
                        "id": msg_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text, "annotations": []}],
                    }));
                }
                Some("tool_use") => {
                    let call_id = block["id"].as_str().unwrap_or("").to_string();
                    let name = block["name"].as_str().unwrap_or("");
                    let arguments = serde_json::to_string(
                        block.get("input").unwrap_or(&Value::Object(Map::new())),
                    )
                    .unwrap_or_default();
                    output.push(json!({
                        "id": call_id,
                        "type": "function_call",
                        "status": "completed",
                        "name": name,
                        "call_id": call_id,
                        "arguments": arguments,
                    }));
                }
                _ => {}
            }
        }
    }

    let usage_raw = &ant["usage"];
    let input_tokens = usage_raw["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = usage_raw["output_tokens"].as_u64().unwrap_or(0);

    json!({
        "id": resp_id,
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": output_tokens,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input_tokens + output_tokens,
        },
        "error": null,
    })
}

/// Convert OAI `input` array to (optional system prompt, Anthropic messages).
fn convert_input_to_messages(input: Option<&Vec<Value>>) -> (Option<String>, Vec<Value>) {
    let Some(items) = input else {
        return (None, vec![]);
    };

    let mut messages: Vec<Value> = Vec::new();
    let mut system: Option<String> = None;

    for item in items {
        let itype = item["type"].as_str().unwrap_or("message");
        let role = item["role"].as_str().unwrap_or("user");

        match itype {
            "message" => {
                let text = extract_message_text(item);

                if role == "system" || role == "developer" {
                    system = Some(match system {
                        Some(existing) => format!("{existing}\n\n{text}"),
                        None => text,
                    });
                } else {
                    messages.push(json!({"role": role, "content": text}));
                }
            }
            "function_call" => {
                let call_id = item["call_id"]
                    .as_str()
                    .or_else(|| item["id"].as_str())
                    .unwrap_or("call_unknown")
                    .to_string();
                let name = item["name"].as_str().unwrap_or("");
                let arguments = item["arguments"].as_str().unwrap_or("{}");
                let input_val: Value =
                    serde_json::from_str(arguments).unwrap_or(Value::Object(Map::new()));

                let tool_use_block = json!({
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": input_val,
                });

                // Append to existing assistant message or create new one.
                if let Some(last) = messages.last_mut()
                    && last["role"].as_str() == Some("assistant")
                {
                    let content = last.get_mut("content").unwrap_or_else(|| unreachable!());
                    if let Some(arr) = content.as_array_mut() {
                        arr.push(tool_use_block);
                    } else {
                        // Convert string content to array
                        let text_val = content.take();
                        let text_str = text_val.as_str().unwrap_or("").to_string();
                        let mut arr = Vec::new();
                        if !text_str.is_empty() {
                            arr.push(json!({"type": "text", "text": text_str}));
                        }
                        arr.push(tool_use_block);
                        *content = Value::Array(arr);
                    }
                    continue;
                }
                messages.push(json!({
                    "role": "assistant",
                    "content": [tool_use_block],
                }));
            }
            "function_call_output" => {
                let call_id = item["call_id"].as_str().unwrap_or("");
                let output = item["output"].as_str().unwrap_or("");
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": output,
                    }],
                }));
            }
            _ => {}
        }
    }

    (system, messages)
}

/// Extract text content from an OAI message item.
fn extract_message_text(item: &Value) -> String {
    match &item["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut texts = Vec::new();
            for part in parts {
                if let Some("input_text" | "text") = part["type"].as_str()
                    && let Some(t) = part["text"].as_str()
                {
                    texts.push(t.to_string());
                }
            }
            texts.join("\n")
        }
        _ => String::new(),
    }
}

/// Convert OAI Responses tool definitions to Anthropic tool format.
fn convert_tools(tools: Option<&Vec<Value>>) -> Vec<Value> {
    let Some(tools) = tools else {
        return vec![];
    };

    let mut result = Vec::new();
    for tool in tools {
        let (name, description, parameters) = if tool["type"].as_str() == Some("function") {
            // Could be nested {"function": {...}} or flat
            if let Some(func) = tool.get("function") {
                (
                    func["name"].as_str().unwrap_or(""),
                    func["description"].as_str().unwrap_or(""),
                    func.get("parameters")
                        .cloned()
                        .unwrap_or(json!({"type": "object", "properties": {}})),
                )
            } else {
                (
                    tool["name"].as_str().unwrap_or(""),
                    tool["description"].as_str().unwrap_or(""),
                    tool.get("parameters")
                        .cloned()
                        .unwrap_or(json!({"type": "object", "properties": {}})),
                )
            }
        } else {
            (
                tool["name"].as_str().unwrap_or(""),
                tool["description"].as_str().unwrap_or(""),
                tool.get("parameters")
                    .cloned()
                    .unwrap_or(json!({"type": "object", "properties": {}})),
            )
        };

        if name.is_empty() {
            continue;
        }

        result.push(json!({
            "name": name,
            "description": description,
            "input_schema": parameters,
        }));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_basic_request_translation() {
        let oai = json!({
            "model": "claude-sonnet-4-20250514",
            "stream": true,
            "instructions": "You are helpful.",
            "input": [
                {"type": "message", "role": "user", "content": "Hello"}
            ],
            "tools": [{
                "type": "function",
                "name": "shell",
                "description": "Run a shell command",
                "parameters": {"type": "object", "properties": {"command": {"type": "string"}}}
            }],
            "max_output_tokens": 8192,
        });

        let result = oai_to_anthropic(&oai);

        assert_eq!(result["model"], "claude-sonnet-4-20250514");
        assert_eq!(result["max_tokens"], 8192);
        assert_eq!(result["stream"], true);
        assert_eq!(result["system"], "You are helpful.");
        assert_eq!(result["messages"][0]["role"], "user");
        assert_eq!(result["messages"][0]["content"], "Hello");
        assert_eq!(result["tools"][0]["name"], "shell");
        assert_eq!(
            result["tools"][0]["input_schema"]["properties"]["command"]["type"],
            "string"
        );
    }

    #[test]
    fn test_function_call_roundtrip() {
        let oai = json!({
            "model": "claude-sonnet-4-20250514",
            "stream": true,
            "input": [
                {"type": "message", "role": "user", "content": "List files"},
                {"type": "function_call", "call_id": "call_123", "name": "shell", "arguments": "{\"command\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_123", "output": "file1.txt\nfile2.txt"}
            ],
            "max_output_tokens": 16384,
        });

        let result = oai_to_anthropic(&oai);
        let messages = result["messages"]
            .as_array()
            .unwrap_or_else(|| unreachable!());

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["id"], "call_123");
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_123");
    }
}
