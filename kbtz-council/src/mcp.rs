use crate::git;
use crate::project::{ProjectDir, RepoConfig, Stakeholder};
use crate::step::Dispatch;
use serde::Deserialize;
use serde_json::Value;
use std::io;
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Value,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct RepoParam {
    name: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct StakeholderParam {
    name: String,
    persona: String,
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

fn text_content(msg: &str) -> Value {
    serde_json::json!({
        "content": [{"type": "text", "text": msg}]
    })
}

fn jsonrpc_response(id: Value, result: Value) -> String {
    serde_json::to_string(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
    .unwrap()
}

/// Handle a single MCP tool call, mutating project state under the lock.
fn handle_tool_call(
    dir: &mut ProjectDir,
    tool_name: &str,
    arguments: &Value,
) -> io::Result<Value> {
    match tool_name {
        "define_project" => {
            let repo_params: Vec<RepoParam> =
                serde_json::from_value(arguments["repos"].clone()).unwrap_or_default();
            let repos: Vec<RepoConfig> = repo_params
                .into_iter()
                .map(|r| RepoConfig {
                    name: r.name,
                    url: r.url,
                })
                .collect();

            for repo in &repos {
                let dest = dir.root().join("repos").join(&repo.name);
                if !dest.exists() {
                    git::shallow_clone(Path::new(&repo.url), &dest)?;
                }
            }

            let stakeholder_params: Vec<StakeholderParam> =
                serde_json::from_value(arguments["stakeholders"].clone()).unwrap_or_default();
            let stakeholders: Vec<Stakeholder> = stakeholder_params
                .into_iter()
                .map(|s| Stakeholder {
                    name: s.name,
                    persona: s.persona,
                })
                    .collect();

            let goal = arguments["goal_summary"]
                .as_str()
                .unwrap_or("")
                .to_string();

            let state = dir.state_mut();
            state.project.repos = repos;
            state.project.stakeholders = stakeholders;
            state.project.goal_summary = goal;
            dir.persist()?;

            Ok(text_content("Project defined successfully."))
        }
        "dispatch_step" => {
            let prompt = arguments["prompt"].as_str().unwrap_or("").to_string();
            let repos: Vec<String> =
                serde_json::from_value(arguments["repos"].clone()).unwrap_or_default();
            let files: Vec<String> =
                serde_json::from_value(arguments["files"].clone()).unwrap_or_default();

            let step_id = dir.add_step(Dispatch {
                prompt,
                repos,
                files,
            })?;

            Ok(text_content(&format!("Step {} dispatched.", step_id)))
        }
        "rework_step" => {
            let step_id = arguments["step_id"].as_str().unwrap_or("").to_string();
            let feedback = arguments["feedback"].as_str().unwrap_or("").to_string();

            if let Some(step) = dir.state_mut().steps.iter_mut().find(|s| s.id == step_id) {
                step.phase = crate::step::StepPhase::Rework;
                step.decision = Some(crate::step::Decision::Rework {
                    feedback: feedback.clone(),
                });
            }
            dir.persist()?;

            Ok(text_content(&format!(
                "Step {} sent back for rework.",
                step_id
            )))
        }
        "close_step" => {
            let step_id = arguments["step_id"].as_str().unwrap_or("").to_string();

            if let Some(step) = dir.state_mut().steps.iter_mut().find(|s| s.id == step_id) {
                step.phase = crate::step::StepPhase::Merged;
            }
            dir.persist()?;

            let session_dir = dir
                .root()
                .join("sessions")
                .join(format!("{}-impl", step_id));
            if session_dir.exists() {
                let _ = git::cleanup_session_dir(&session_dir);
            }

            Ok(text_content(&format!("Step {} closed.", step_id)))
        }
        _ => Ok(text_content("Unknown tool")),
    }
}

/// Start the MCP HTTP server on a random localhost port. Returns the port.
/// The server runs in a background thread, processing tool calls via the
/// shared ProjectDir.
pub fn start_mcp_server(project_dir: Arc<Mutex<ProjectDir>>) -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let server = tiny_http::Server::from_listener(listener, None)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    thread::spawn(move || {
        for mut request in server.incoming_requests() {
            // Only accept POST
            if request.method() != &tiny_http::Method::Post {
                let resp = tiny_http::Response::from_string("Method not allowed")
                    .with_status_code(405);
                let _ = request.respond(resp);
                continue;
            }

            // Read body
            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                let resp =
                    tiny_http::Response::from_string("Bad request").with_status_code(400);
                let _ = request.respond(resp);
                continue;
            }

            let rpc: JsonRpcRequest = match serde_json::from_str(&body) {
                Ok(r) => r,
                Err(_) => {
                    let resp = tiny_http::Response::from_string("Invalid JSON-RPC")
                        .with_status_code(400);
                    let _ = request.respond(resp);
                    continue;
                }
            };

            let response_body = match rpc.method.as_str() {
                "initialize" => jsonrpc_response(
                    rpc.id,
                    serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "kbtz-council", "version": "0.1.0" }
                    }),
                ),
                "tools/list" => jsonrpc_response(rpc.id, tool_definitions()),
                "tools/call" => {
                    let params = rpc.params.unwrap_or(Value::Null);
                    let tool_name = params["name"].as_str().unwrap_or("");
                    let arguments = &params["arguments"];

                    let result = {
                        let mut dir = project_dir.lock().unwrap();
                        handle_tool_call(&mut dir, tool_name, arguments)
                            .unwrap_or_else(|e| text_content(&format!("Error: {}", e)))
                    };

                    jsonrpc_response(rpc.id, result)
                }
                _ => jsonrpc_response(
                    rpc.id,
                    serde_json::json!({"error": {"code": -32601, "message": "Method not found"}}),
                ),
            };

            let resp = tiny_http::Response::from_string(&response_body)
                .with_header(
                    "Content-Type: application/json"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                );
            let _ = request.respond(resp);
        }
    });

    Ok(port)
}

/// Write an .mcp.json config file pointing at the running HTTP MCP server.
pub fn write_mcp_config(project_dir: &Path, port: u16) -> io::Result<std::path::PathBuf> {
    let config = serde_json::json!({
        "mcpServers": {
            "council": {
                "type": "http",
                "url": format!("http://127.0.0.1:{}/mcp", port)
            }
        }
    });

    let config_path = project_dir.join(".mcp.json");
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&config).unwrap(),
    )?;
    Ok(config_path)
}
