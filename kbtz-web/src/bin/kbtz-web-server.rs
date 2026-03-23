use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{Message as WsMessage, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use clap::Parser;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use kbtz::config::Config;
use kbtz_web::auth;
use kbtz_web::session_manager::{self, SessionEvent, SessionManager};
use kbtz_web::task_tree;
use kbtz_web::ws_messages::{ClientMessage, ServerMessage};

#[derive(Parser)]
#[command(name = "kbtz-web-server", about = "Web server for kbtz workspace")]
struct Cli {
    /// Bind address (host:port)
    #[arg(long, default_value = "127.0.0.1:3000")]
    bind: String,

    /// Max concurrent agent sessions
    #[arg(long, default_value_t = 4)]
    max: usize,

    /// Allow insecure connections (skip auth for development)
    #[arg(long)]
    allow_insecure: bool,

    /// Path to static SPA assets directory
    #[arg(long)]
    static_dir: Option<PathBuf>,
}

struct AppState {
    manager: Arc<Mutex<SessionManager>>,
    token: String,
    db_path: String,
    allow_insecure: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let config = Config::load().unwrap_or_default();
    let bind = config
        .web
        .as_ref()
        .and_then(|w| w.bind.clone())
        .unwrap_or_else(|| cli.bind.clone());

    let event_cap = config
        .web
        .as_ref()
        .map(|w| w.event_history_limit())
        .unwrap_or(10_000);

    let db_path = kbtz::paths::db_path();
    let workspace_dir = PathBuf::from(kbtz::paths::workspace_dir());
    std::fs::create_dir_all(&workspace_dir)
        .with_context(|| format!("creating workspace dir {}", workspace_dir.display()))?;

    let default_backend = config
        .workspace
        .backend
        .clone()
        .unwrap_or_else(|| "claude".to_string());

    let default_directory = config
        .workspace
        .directory
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let token_path = auth::default_token_path();
    let token = auth::load_or_create_token(&token_path)?;
    info!("auth token stored at {}", token_path.display());

    let (manager, _event_rx) =
        SessionManager::new(db_path.clone(), workspace_dir, cli.max, default_backend, event_cap, default_directory);

    let manager = Arc::new(Mutex::new(manager));

    // Reconnect to existing shepherd sessions
    {
        let mut mgr = manager.lock().await;
        if let Err(e) = mgr.reconnect_existing() {
            warn!("failed to reconnect existing sessions: {e:#}");
        }
    }

    // Start lifecycle loop
    let manager_clone = manager.clone();
    tokio::spawn(session_manager::run_lifecycle_loop(manager_clone));

    let state = Arc::new(AppState {
        manager,
        token,
        db_path,
        allow_insecure: cli.allow_insecure,
    });

    let mut app = axum::Router::new()
        .route("/auth", post(handle_auth))
        .route("/ws", get(handle_ws_upgrade))
        .with_state(state.clone());

    if let Some(static_dir) = cli.static_dir {
        let index_path = static_dir.join("index.html");
        app = app.fallback_service(
            tower_http::services::ServeDir::new(static_dir)
                .fallback(tower_http::services::ServeFile::new(index_path)),
        );
    }

    let addr: SocketAddr = bind.parse().context("invalid bind address")?;
    info!("listening on {addr}");
    if !cli.allow_insecure {
        info!("auth required — use POST /auth with token to authenticate");
    } else {
        warn!("--allow-insecure: authentication disabled");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ── Auth endpoint ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AuthRequest {
    token: String,
}

#[derive(Serialize)]
struct AuthResponse {
    ok: bool,
}

async fn handle_auth(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AuthRequest>,
) -> (StatusCode, Json<AuthResponse>) {
    if auth::verify_token(&state.token, &body.token) {
        (
            StatusCode::OK,
            Json(AuthResponse { ok: true }),
        )
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(AuthResponse { ok: false }),
        )
    }
}

// ── WebSocket endpoint ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct WsQuery {
    token: Option<String>,
}

async fn handle_ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    // Authenticate
    if !state.allow_insecure {
        match query.token {
            Some(ref t) if auth::verify_token(&state.token, t) => {}
            _ => return StatusCode::UNAUTHORIZED.into_response(),
        }
    }

    ws.on_upgrade(move |socket| handle_ws(socket, state))
        .into_response()
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    // Send initial task tree
    if let Err(e) = send_task_tree(&mut socket, &state).await {
        error!("failed to send initial task tree: {e:#}");
        return;
    }

    // Subscribe to session events
    let mut event_rx = {
        let mgr = state.manager.lock().await;
        mgr.subscribe()
    };

    loop {
        tokio::select! {
            // Client message
            msg = socket.recv() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Err(e) = handle_client_message(&text, &mut socket, &state).await {
                            warn!("error handling client message: {e:#}");
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    _ => {}
                }
            }
            // Server-side events
            event = event_rx.recv() => {
                match event {
                    Ok(SessionEvent::AgentEvent { session_id, event }) => {
                        let msg = ServerMessage::SessionEvent { session_id, event };
                        if send_server_message(&mut socket, &msg).await.is_err() {
                            break;
                        }
                    }
                    Ok(SessionEvent::StatusChange { session_id, status, task_name }) => {
                        let msg = ServerMessage::SessionStatus { session_id, status, task_name };
                        if send_server_message(&mut socket, &msg).await.is_err() {
                            break;
                        }
                    }
                    Ok(SessionEvent::TreeChanged) => {
                        if send_task_tree(&mut socket, &state).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WebSocket client lagged by {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn handle_client_message(
    text: &str,
    socket: &mut WebSocket,
    state: &Arc<AppState>,
) -> Result<()> {
    let msg: ClientMessage = serde_json::from_str(text).context("invalid client message")?;
    match msg {
        ClientMessage::Subscribe { session_id } => {
            // Send session history
            let events = {
                let mgr = state.manager.lock().await;
                mgr.get_session_history(&session_id)?
            };
            let msg = ServerMessage::SessionEvents {
                session_id,
                events,
            };
            send_server_message(socket, &msg).await?;
        }
        ClientMessage::Input { session_id, data } => {
            let mut mgr = state.manager.lock().await;
            mgr.send_input(&session_id, &data)?;
        }
        ClientMessage::GetTree => {
            send_task_tree(socket, state).await?;
        }
    }
    Ok(())
}

async fn send_task_tree(socket: &mut WebSocket, state: &Arc<AppState>) -> Result<()> {
    let tasks = tokio::task::spawn_blocking({
        let db_path = state.db_path.clone();
        move || -> Result<Vec<task_tree::TaskNode>> {
            let conn = kbtz::db::open(&db_path)?;
            kbtz::db::init(&conn)?;
            task_tree::build_task_tree(&conn, false)
        }
    })
    .await??;

    let msg = ServerMessage::TaskTree { tasks };
    send_server_message(socket, &msg).await
}

async fn send_server_message(socket: &mut WebSocket, msg: &ServerMessage) -> Result<()> {
    let json = serde_json::to_string(msg)?;
    socket
        .send(WsMessage::Text(json.into()))
        .await
        .context("sending WebSocket message")?;
    Ok(())
}
