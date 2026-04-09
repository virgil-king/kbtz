use crate::stream::{parse_stream_line, StreamEvent};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use uuid::Uuid;

/// Orchestrator-internal session identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionKey {
    Implementation { job_id: String },
    Stakeholder { name: String },
    Leader,
}

impl std::fmt::Display for SessionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Implementation { job_id } => write!(f, "{}-impl", job_id),
            Self::Stakeholder { name } => write!(f, "stakeholder-{}", name),
            Self::Leader => write!(f, "leader"),
        }
    }
}

/// Agent backend session UUID, used for resumption (--session-id / --resume).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionId(pub Uuid);

#[derive(Debug)]
pub enum SessionMessage {
    Event(StreamEvent),
    RawLine(String),
}

/// An item waiting in a session's queue.
#[derive(Debug, Clone)]
pub struct QueueItem {
    pub prompt: String,
    pub system_prompt: Option<String>,
    pub job_id: Option<String>,
    pub working_dir: PathBuf,
    pub mcp_config: Option<PathBuf>,
}

/// A managed session: queue of pending invocations + optional running process.
pub struct ManagedSession {
    pub key: SessionKey,
    pub agent_session_id: AgentSessionId,
    pub queue: VecDeque<QueueItem>,
    pub active: Option<ActiveSession>,
    invocation_count: u64,
}

/// A currently running `claude -p` process.
pub struct ActiveSession {
    pub job_id: Option<String>,
    child: Child,
    pub rx: mpsc::Receiver<SessionMessage>,
}

impl ManagedSession {
    pub fn new(key: SessionKey) -> Self {
        Self {
            key,
            agent_session_id: AgentSessionId(Uuid::new_v4()),
            queue: VecDeque::new(),
            active: None,
            invocation_count: 0,
        }
    }

    pub fn with_agent_session_id(key: SessionKey, id: AgentSessionId, invocation_count: u64) -> Self {
        Self {
            key,
            agent_session_id: id,
            queue: VecDeque::new(),
            active: None,
            invocation_count,
        }
    }

    /// Push an item onto the queue.
    pub fn enqueue(&mut self, item: QueueItem) {
        self.queue.push_back(item);
    }

    /// If idle and queue is non-empty, pop and spawn.
    pub fn try_dispatch(&mut self) -> io::Result<bool> {
        if self.active.is_some() || self.queue.is_empty() {
            return Ok(false);
        }
        let item = self.queue.pop_front().unwrap();
        let resume = self.invocation_count > 0;
        let mut active = ActiveSession::spawn(
            &self.agent_session_id,
            resume,
            &item.prompt,
            item.system_prompt.as_deref(),
            &item.working_dir,
            item.mcp_config.as_deref(),
        )?;
        active.job_id = item.job_id;
        self.invocation_count += 1;
        self.active = Some(active);
        Ok(true)
    }

    /// Check if the active process has exited. Returns true if it exited.
    pub fn poll_exit(&mut self) -> io::Result<bool> {
        if let Some(ref mut active) = self.active {
            if let Some(_code) = active.try_wait()? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Remove the active process after it exits.
    pub fn reap(&mut self) -> Option<ActiveSession> {
        self.active.take()
    }

    pub fn is_running(&self) -> bool {
        self.active.is_some()
    }

    pub fn is_idle(&self) -> bool {
        self.active.is_none() && self.queue.is_empty()
    }

    /// Kill the active process.
    pub fn kill(&mut self) -> io::Result<()> {
        if let Some(ref mut active) = self.active {
            active.kill()?;
        }
        Ok(())
    }
}

impl ActiveSession {
    fn spawn(
        agent_session_id: &AgentSessionId,
        resume: bool,
        prompt: &str,
        system_prompt: Option<&str>,
        working_dir: &Path,
        mcp_config: Option<&Path>,
    ) -> io::Result<Self> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--verbose")
            .arg("--permission-mode")
            .arg("bypassPermissions")
            .current_dir(working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(sp) = system_prompt {
            cmd.arg("--append-system-prompt").arg(sp);
        }

        if resume {
            cmd.arg("--resume").arg(agent_session_id.0.to_string());
        } else {
            cmd.arg("--session-id").arg(agent_session_id.0.to_string());
        }

        if let Some(config) = mcp_config {
            cmd.arg("--mcp-config").arg(config);
        }

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdout"))?;
        let stderr = child
            .stderr
            .take();

        let (tx, rx) = mpsc::channel();

        // Read stdout (stream-json)
        let tx_stdout = tx.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if line.is_empty() => continue,
                    Ok(line) => {
                        let event = parse_stream_line(&line)
                            .unwrap_or(StreamEvent::Other(line.clone()));
                        let _ = tx_stdout.send(SessionMessage::Event(event));
                        let _ = tx_stdout.send(SessionMessage::RawLine(line));
                    }
                    Err(_) => break,
                }
            }
        });

        // Read stderr (errors, debug output)
        if let Some(stderr) = stderr {
            let tx_stderr = tx.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        if !line.is_empty() {
                            let _ = tx_stderr.send(SessionMessage::Event(
                                StreamEvent::Other(format!("[stderr] {}", line)),
                            ));
                            let _ = tx_stderr.send(SessionMessage::RawLine(
                                format!("{{\"type\":\"stderr\",\"message\":{}}}", serde_json::json!(line)),
                            ));
                        }
                    }
                }
            });
        }

        Ok(Self {
            job_id: None,
            child,
            rx,
        })
    }

    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        match self.child.try_wait()? {
            Some(status) => Ok(Some(status.code().unwrap_or(-1))),
            None => Ok(None),
        }
    }

    fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}
