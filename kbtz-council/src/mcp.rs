use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, BufRead, BufReader, Write};
use std::sync::mpsc;

/// An MCP tool call from the leader, to be processed by the orchestrator.
#[derive(Debug, Clone)]
pub enum LeaderRequest {
    DefineProject {
        id: Value,
        repos: Vec<RepoParam>,
        stakeholders: Vec<StakeholderParam>,
        goal_summary: String,
    },
    DispatchStep {
        id: Value,
        prompt: String,
        repos: Vec<String>,
        files: Vec<String>,
    },
    ReworkStep {
        id: Value,
        step_id: String,
        feedback: String,
    },
    CloseStep {
        id: Value,
        step_id: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoParam {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StakeholderParam {
    pub name: String,
    pub persona: String,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    result: Value,
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Value,
    method: String,
    params: Option<Value>,
}

fn tool_definitions() -> Value {
    serde_json::json!({
        "tools": [
            {
                "name": "define_project",
                "description": "Define the project: register repos, stakeholders, and goal.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "repos": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "url": { "type": "string" }
                                },
                                "required": ["name", "url"]
                            }
                        },
                        "stakeholders": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "persona": { "type": "string" }
                                },
                                "required": ["name", "persona"]
                            }
                        },
                        "goal_summary": { "type": "string" }
                    },
                    "required": ["repos", "stakeholders", "goal_summary"]
                }
            },
            {
                "name": "dispatch_step",
                "description": "Dispatch an implementation step. Returns the assigned step ID.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" },
                        "repos": { "type": "array", "items": { "type": "string" } },
                        "files": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["prompt", "repos"]
                }
            },
            {
                "name": "rework_step",
                "description": "Send a step back to the implementation agent with feedback.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "step_id": { "type": "string" },
                        "feedback": { "type": "string" }
                    },
                    "required": ["step_id", "feedback"]
                }
            },
            {
                "name": "close_step",
                "description": "Close a step (merged or abandoned). Cleans up session directory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "step_id": { "type": "string" }
                    },
                    "required": ["step_id"]
                }
            }
        ]
    })
}

/// Run the MCP stdio server. Reads JSON-RPC from stdin, sends LeaderRequests
/// to the provided channel, and writes responses to stdout.
///
/// This function blocks and should be run in its own thread or process.
pub fn run_mcp_server(
    tx: mpsc::Sender<LeaderRequest>,
    rx_response: mpsc::Receiver<Value>,
) -> io::Result<()> {
    let stdin = io::stdin();
    let reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };

        match request.method.as_str() {
            "initialize" => {
                let response = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: request.id,
                    result: serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "kbtz-council", "version": "0.1.0" }
                    }),
                };
                writeln!(writer, "{}", serde_json::to_string(&response).unwrap())?;
                writer.flush()?;
            }
            "tools/list" => {
                let response = JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: request.id,
                    result: tool_definitions(),
                };
                writeln!(writer, "{}", serde_json::to_string(&response).unwrap())?;
                writer.flush()?;
            }
            "tools/call" => {
                let params = request.params.unwrap_or(Value::Null);
                let tool_name = params["name"].as_str().unwrap_or("");
                let arguments = &params["arguments"];

                let leader_request = match tool_name {
                    "define_project" => {
                        let repos: Vec<RepoParam> =
                            serde_json::from_value(arguments["repos"].clone())
                                .unwrap_or_default();
                        let stakeholders: Vec<StakeholderParam> =
                            serde_json::from_value(arguments["stakeholders"].clone())
                                .unwrap_or_default();
                        let goal_summary =
                            arguments["goal_summary"].as_str().unwrap_or("").to_string();
                        LeaderRequest::DefineProject {
                            id: request.id.clone(),
                            repos,
                            stakeholders,
                            goal_summary,
                        }
                    }
                    "dispatch_step" => {
                        let prompt = arguments["prompt"].as_str().unwrap_or("").to_string();
                        let repos: Vec<String> =
                            serde_json::from_value(arguments["repos"].clone())
                                .unwrap_or_default();
                        let files: Vec<String> =
                            serde_json::from_value(arguments["files"].clone())
                                .unwrap_or_default();
                        LeaderRequest::DispatchStep {
                            id: request.id.clone(),
                            prompt,
                            repos,
                            files,
                        }
                    }
                    "rework_step" => {
                        let step_id = arguments["step_id"].as_str().unwrap_or("").to_string();
                        let feedback = arguments["feedback"].as_str().unwrap_or("").to_string();
                        LeaderRequest::ReworkStep {
                            id: request.id.clone(),
                            step_id,
                            feedback,
                        }
                    }
                    "close_step" => {
                        let step_id = arguments["step_id"].as_str().unwrap_or("").to_string();
                        LeaderRequest::CloseStep {
                            id: request.id.clone(),
                            step_id,
                        }
                    }
                    _ => continue,
                };

                let _ = tx.send(leader_request);
                if let Ok(result) = rx_response.recv() {
                    let response = JsonRpcResponse {
                        jsonrpc: "2.0".into(),
                        id: request.id,
                        result,
                    };
                    writeln!(writer, "{}", serde_json::to_string(&response).unwrap())?;
                    writer.flush()?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Write an .mcp.json config file for the leader's Claude Code session.
pub fn write_mcp_config(
    project_dir: &std::path::Path,
    orchestrator_binary: &str,
) -> io::Result<std::path::PathBuf> {
    let config = serde_json::json!({
        "mcpServers": {
            "orchestrator": {
                "command": orchestrator_binary,
                "args": ["--mcp-mode", "--project", project_dir.to_str().unwrap()],
                "type": "stdio"
            }
        }
    });

    let config_path = project_dir.join(".mcp.json");
    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap())?;
    Ok(config_path)
}
