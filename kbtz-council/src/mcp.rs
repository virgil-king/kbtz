use crate::git;
use crate::global::GlobalState;
use crate::project::{Project, ProjectDir, RepoConfig, Stakeholder};
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
                "name": "create_artifact",
                "description": "Submit leader-produced work for stakeholder review without dispatching an implementation agent. Creates a job implicitly if job_id is omitted.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "description": { "type": "string", "description": "Summary of the work produced." },
                        "job_id": { "type": "string", "description": "Existing job ID. If omitted, a new job is created." }
                    },
                    "required": ["description"]
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
        "create_artifact" => {
            let description = arguments["description"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let job_id = arguments["job_id"].as_str().map(|s| s.to_string());

            let job_id = match job_id {
                Some(id) => {
                    dir.complete_job_with_artifact(&id, description)?;
                    id
                }
                None => {
                    // Create a new job at Completed phase with artifact
                    dir.add_completed_job(description)?
                }
            };

            Ok(text_content(&format!(
                "Artifact created for job {}. Stakeholder review will begin automatically.",
                job_id
            )))
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

/// Start an MCP HTTP server on a random localhost port. The `handler` closure
/// is called for each `tools/call` request with (tool_name, arguments) and
/// must return a JSON result value.
fn start_mcp_http_server(
    server_name: &str,
    tools_list: Value,
    handler: impl Fn(&str, &Value) -> Value + Send + 'static,
) -> io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();

    let server = tiny_http::Server::from_listener(listener, None)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let init_response = serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": server_name, "version": "0.1.0" }
    });

    thread::spawn(move || {
        let json_header: tiny_http::Header = "Content-Type: application/json"
            .parse()
            .unwrap();

        for mut request in server.incoming_requests() {
            if request.method() == &tiny_http::Method::Get {
                let resp = tiny_http::Response::from_string("").with_status_code(405);
                let _ = request.respond(resp);
                continue;
            }

            if request.method() == &tiny_http::Method::Delete {
                let resp = tiny_http::Response::from_string("").with_status_code(202);
                let _ = request.respond(resp);
                continue;
            }

            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                let resp =
                    tiny_http::Response::from_string("Bad request").with_status_code(400);
                let _ = request.respond(resp);
                continue;
            }

            let parsed: Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => {
                    let resp =
                        tiny_http::Response::from_string("Invalid JSON").with_status_code(400);
                    let _ = request.respond(resp);
                    continue;
                }
            };

            // Notifications have no "id" — respond with 202
            if parsed.get("id").is_none() || parsed["id"].is_null() {
                let resp = tiny_http::Response::from_string("").with_status_code(202);
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
                "initialize" => jsonrpc_response(rpc.id, init_response.clone()),
                "tools/list" => jsonrpc_response(rpc.id, tools_list.clone()),
                "tools/call" => {
                    let params = rpc.params.unwrap_or(Value::Null);
                    let tool_name = params["name"].as_str().unwrap_or("");
                    let arguments = &params["arguments"];
                    jsonrpc_response(rpc.id, handler(tool_name, arguments))
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

/// Start the per-project MCP HTTP server. Returns the port.
pub fn start_mcp_server(project_dir: Arc<Mutex<ProjectDir>>) -> io::Result<u16> {
    start_mcp_http_server("kbtz-council", tool_definitions(), move |tool_name, arguments| {
        let mut dir = project_dir.lock().unwrap();
        handle_tool_call(&mut dir, tool_name, arguments)
            .unwrap_or_else(|e| text_content(&format!("Error: {}", e)))
    })
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

// --- Concierge MCP server (global scope) ---

fn concierge_tool_definitions() -> Value {
    serde_json::json!({
        "tools": [
            {
                "name": "create_project",
                "description": "Create a new project. Returns the project name and path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Project name (alphanumeric and hyphens)." },
                        "goal": { "type": "string", "description": "One-line project goal." }
                    },
                    "required": ["name", "goal"]
                }
            },
            {
                "name": "list_projects",
                "description": "List projects, optionally filtered by status.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["active", "paused", "archived"],
                            "description": "Filter by status. Omit to list all."
                        }
                    }
                }
            },
            {
                "name": "archive_project",
                "description": "Archive a project. Moves it to the archive directory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Project name to archive." }
                    },
                    "required": ["name"]
                }
            },
            {
                "name": "resume_project",
                "description": "Resume (un-archive) a project, setting it back to active.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Project name to resume." }
                    },
                    "required": ["name"]
                }
            }
        ]
    })
}

fn require_str<'a>(arguments: &'a Value, field: &str) -> io::Result<&'a str> {
    arguments[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("'{}' is required", field)))
}

fn handle_concierge_tool_call(
    global: &mut GlobalState,
    tool_name: &str,
    arguments: &Value,
) -> io::Result<Value> {
    match tool_name {
        "create_project" => {
            let name = require_str(arguments, "name")?.to_string();
            let goal = require_str(arguments, "goal")?.to_string();
            let project = Project {
                repos: vec![],
                stakeholders: vec![],
                goal_summary: goal.clone(),
            };
            global.create_project(&name, &goal, &project)?;
            let path = global.project_path(&name)?;
            Ok(text_content(&format!(
                "Project '{}' created at {}.",
                name,
                path.display()
            )))
        }
        "list_projects" => {
            use crate::global::ProjectStatus;
            let status_filter = arguments
                .get("status")
                .and_then(|v| v.as_str())
                .and_then(|s| match s {
                    "active" => Some(ProjectStatus::Active),
                    "paused" => Some(ProjectStatus::Paused),
                    "archived" => Some(ProjectStatus::Archived),
                    _ => None,
                });
            let entries = global.list_projects(status_filter);
            if entries.is_empty() {
                return Ok(text_content("No projects found."));
            }
            let mut lines = Vec::new();
            for e in &entries {
                lines.push(format!(
                    "- {} [{}] — {} (created {})",
                    e.name,
                    match e.status {
                        ProjectStatus::Active => "active",
                        ProjectStatus::Paused => "paused",
                        ProjectStatus::Archived => "archived",
                    },
                    if e.goal.is_empty() { "(no goal)" } else { &e.goal },
                    e.created_at,
                ));
            }
            Ok(text_content(&lines.join("\n")))
        }
        "archive_project" => {
            use crate::global::ProjectStatus;
            let name = require_str(arguments, "name")?.to_string();
            global.set_status(&name, ProjectStatus::Archived)?;
            Ok(text_content(&format!("Project '{}' archived.", name)))
        }
        "resume_project" => {
            use crate::global::ProjectStatus;
            let name = require_str(arguments, "name")?.to_string();
            global.set_status(&name, ProjectStatus::Active)?;
            Ok(text_content(&format!(
                "Project '{}' resumed (now active).",
                name
            )))
        }
        _ => Ok(text_content("Unknown tool")),
    }
}

/// Start the concierge MCP HTTP server. Returns the port.
pub fn start_concierge_mcp_server(global: Arc<Mutex<GlobalState>>) -> io::Result<u16> {
    start_mcp_http_server(
        "kbtz-council-concierge",
        concierge_tool_definitions(),
        move |tool_name, arguments| {
            let mut g = global.lock().unwrap();
            handle_concierge_tool_call(&mut g, tool_name, arguments)
                .unwrap_or_else(|e| text_content(&format!("Error: {}", e)))
        },
    )
}

/// Write an .mcp.json config for the concierge session.
pub fn write_concierge_mcp_config(global_dir: &Path, port: u16) -> io::Result<std::path::PathBuf> {
    let config = serde_json::json!({
        "mcpServers": {
            "concierge": {
                "type": "http",
                "url": format!("http://127.0.0.1:{}/mcp", port)
            }
        }
    });

    let config_path = global_dir.join(".concierge-mcp.json");
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
