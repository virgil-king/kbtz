use crate::git;
use crate::project::{ProjectDir, RepoConfig, Stakeholder};
use crate::job::{Dispatch, RepoRef};
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
                "name": "dispatch_job",
                "description": "Dispatch an implementation job. Returns the assigned job ID.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string" },
                        "repos": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": { "type": "string" },
                                    "branch": { "type": "string" }
                                },
                                "required": ["name"]
                            }
                        },
                        "files": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["prompt", "repos"]
                }
            },
            {
                "name": "rework_job",
                "description": "Send a job back to the implementation agent with feedback.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string" },
                        "feedback": { "type": "string" }
                    },
                    "required": ["job_id", "feedback"]
                }
            },
            {
                "name": "close_job",
                "description": "Close a job (merged or abandoned). Cleans up session directory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string" }
                    },
                    "required": ["job_id"]
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
                    branch: None,
                })
                .collect();

            // No cloning here — repos are cloned on demand per job

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
        "dispatch_job" => {
            let prompt = arguments["prompt"].as_str().unwrap_or("").to_string();
            let repos: Vec<RepoRef> =
                serde_json::from_value(arguments["repos"].clone()).unwrap_or_default();
            let files: Vec<String> =
                serde_json::from_value(arguments["files"].clone()).unwrap_or_default();

            let job_id = dir.add_job(Dispatch {
                prompt,
                repos,
                files,
            })?;

            Ok(text_content(&format!("Job {} dispatched.", job_id)))
        }
        "rework_job" => {
            let job_id = arguments["job_id"].as_str().unwrap_or("").to_string();
            let feedback = arguments["feedback"].as_str().unwrap_or("").to_string();

            // Record decision on the latest artifact
            if let Some(artifact) = dir.latest_artifact_mut(&job_id) {
                artifact.decision = Some(crate::job::Decision::Rework {
                    feedback: feedback.clone(),
                });
            }
            if let Some(job) = dir.state_mut().jobs.iter_mut().find(|s| s.id == job_id) {
                job.phase = crate::job::JobPhase::Rework;
            }
            dir.persist()?;

            Ok(text_content(&format!(
                "Job {} sent back for rework.",
                job_id
            )))
        }
        "close_job" => {
            let job_id = arguments["job_id"].as_str().unwrap_or("").to_string();

            // Record merge decision on the latest artifact
            if let Some(artifact) = dir.latest_artifact_mut(&job_id) {
                artifact.decision = Some(crate::job::Decision::Merge);
            }
            if let Some(job) = dir.state_mut().jobs.iter_mut().find(|s| s.id == job_id) {
                job.phase = crate::job::JobPhase::Merged;
            }
            dir.persist()?;

            let session_dir = dir
                .root()
                .join("sessions")
                .join(format!("{}-impl", job_id));
            if session_dir.exists() {
                let _ = git::cleanup_session_dir(&session_dir);
            }

            Ok(text_content(&format!("Job {} closed.", job_id)))
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
        let json_header: tiny_http::Header = "Content-Type: application/json"
            .parse()
            .unwrap();

        for mut request in server.incoming_requests() {
            // Handle GET (health check / SSE endpoint discovery)
            if request.method() == &tiny_http::Method::Get {
                let resp = tiny_http::Response::from_string("")
                    .with_status_code(405);
                let _ = request.respond(resp);
                continue;
            }

            // Handle DELETE (session termination)
            if request.method() == &tiny_http::Method::Delete {
                let resp = tiny_http::Response::from_string("")
                    .with_status_code(202);
                let _ = request.respond(resp);
                continue;
            }

            // POST: read JSON-RPC body
            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                let resp = tiny_http::Response::from_string("Bad request")
                    .with_status_code(400);
                let _ = request.respond(resp);
                continue;
            }

            // Parse as JSON — check if it's a notification (no "id" field)
            let parsed: Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => {
                    let resp = tiny_http::Response::from_string("Invalid JSON")
                        .with_status_code(400);
                    let _ = request.respond(resp);
                    continue;
                }
            };

            // Notifications have no "id" — respond with 202
            if parsed.get("id").is_none() || parsed["id"].is_null() {
                let resp = tiny_http::Response::from_string("")
                    .with_status_code(202);
                let _ = request.respond(resp);
                continue;
            }

            let rpc: JsonRpcRequest = match serde_json::from_value(parsed) {
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
                .with_header(json_header.clone());
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

/// Run the MCP stdio server. Reads JSON-RPC from stdin, processes tool
/// calls by reading/writing the project directory, writes responses to
/// stdout. Used when invoked with --mcp-stdio.
pub fn run_mcp_stdio(project_path: &Path) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Write};

    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }

        #[derive(serde::Deserialize)]
        struct Req {
            #[allow(dead_code)]
            jsonrpc: String,
            id: serde_json::Value,
            method: String,
            params: Option<serde_json::Value>,
        }

        let request: Req = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };

        let response_body = match request.method.as_str() {
            "initialize" => jsonrpc_response(
                request.id,
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "kbtz-council", "version": "0.1.0" }
                }),
            ),
            "tools/list" => jsonrpc_response(request.id, tool_definitions()),
            "tools/call" => {
                let params = request.params.unwrap_or(serde_json::Value::Null);
                let tool_name = params["name"].as_str().unwrap_or("");
                let arguments = &params["arguments"];

                let mut dir = ProjectDir::load(project_path)?;
                let result = handle_tool_call(&mut dir, tool_name, arguments)
                    .unwrap_or_else(|e| text_content(&format!("Error: {}", e)));

                jsonrpc_response(request.id, result)
            }
            _ => jsonrpc_response(
                request.id,
                serde_json::json!({"error": {"code": -32601, "message": "Method not found"}}),
            ),
        };

        writeln!(writer, "{}", response_body)?;
        writer.flush()?;
    }

    Ok(())
}
