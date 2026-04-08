use serde_json::Value;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    AssistantText(String),
    Thinking(String),
    ToolUse { name: String, input: String },
    ToolResult { content: String },
    Result { result: String, session_id: Option<String> },
    Other(String),
}

pub fn parse_stream_line(line: &str) -> Result<StreamEvent, serde_json::Error> {
    let v: Value = serde_json::from_str(line)?;
    let event_type = v["type"].as_str().unwrap_or("");

    match event_type {
        "assistant" => {
            let content = &v["message"]["content"];
            if let Some(arr) = content.as_array() {
                for item in arr {
                    match item["type"].as_str().unwrap_or("") {
                        "thinking" => {
                            if let Some(text) = item["thinking"].as_str() {
                                return Ok(StreamEvent::Thinking(text.to_string()));
                            }
                        }
                        "text" => {
                            if let Some(text) = item["text"].as_str() {
                                return Ok(StreamEvent::AssistantText(text.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(StreamEvent::Other(line.to_string()))
        }
        "tool_use" => {
            let name = v["name"].as_str().unwrap_or("unknown").to_string();
            let input = v["input"].to_string();
            Ok(StreamEvent::ToolUse { name, input })
        }
        "tool_result" => {
            let content = v["content"].as_str().unwrap_or("").to_string();
            Ok(StreamEvent::ToolResult { content })
        }
        "result" => {
            let result = v["result"].as_str().unwrap_or("").to_string();
            let session_id = v["session_id"].as_str().map(|s| s.to_string());
            Ok(StreamEvent::Result { result, session_id })
        }
        _ => Ok(StreamEvent::Other(line.to_string())),
    }
}
