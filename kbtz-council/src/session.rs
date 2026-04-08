use crate::stream::{parse_stream_line, StreamEvent};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use uuid::Uuid;

/// Orchestrator-internal session key (e.g. "step-001-impl", "leader-leader").
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey(pub String);

/// Agent backend session UUID, used for resumption (--session-id / --resume).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionId(pub Uuid);

#[derive(Debug)]
pub enum SessionMessage {
    Event(StreamEvent),
    RawLine(String),
    Exited { code: Option<i32> },
}

#[derive(Debug, Clone)]
pub enum SessionRole {
    Implementation,
    Stakeholder { name: String },
    LeaderDecision,
}

/// A running headless Claude Code session.
pub struct HeadlessSession {
    pub key: SessionKey,
    pub agent_session_id: AgentSessionId,
    pub step_id: String,
    pub role: SessionRole,
    child: Child,
    pub rx: mpsc::Receiver<SessionMessage>,
}

impl HeadlessSession {
    /// Spawn a `claude -p` session. If `agent_session_id` is provided, resumes that
    /// session with `--resume`. Otherwise generates a new UUID and passes
    /// `--session-id`.
    pub fn spawn(
        step_id: &str,
        role: SessionRole,
        prompt: &str,
        working_dir: &Path,
        agent_session_id: Option<AgentSessionId>,
    ) -> io::Result<Self> {
        let key = SessionKey(format!(
            "{}-{}",
            step_id,
            match &role {
                SessionRole::Implementation => "impl".to_string(),
                SessionRole::Stakeholder { name } => name.clone(),
                SessionRole::LeaderDecision => "leader".to_string(),
            }
        ));

        let (agent_session_id, resume) = match agent_session_id {
            Some(id) => (id, true),
            None => (AgentSessionId(Uuid::new_v4()), false),
        };

        let mut cmd = Command::new("claude");
        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .current_dir(working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if resume {
            cmd.arg("--resume").arg(agent_session_id.0.to_string());
        } else {
            cmd.arg("--session-id").arg(agent_session_id.0.to_string());
        }

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "no stdout"))?;

        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if line.is_empty() => continue,
                    Ok(line) => {
                        let event = parse_stream_line(&line)
                            .unwrap_or(StreamEvent::Other(line.clone()));
                        let _ = tx.send(SessionMessage::Event(event));
                        let _ = tx.send(SessionMessage::RawLine(line));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            key,
            agent_session_id,
            step_id: step_id.to_string(),
            role,
            child,
            rx,
        })
    }

    /// Check if the process has exited. Returns exit code if done.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        match self.child.try_wait()? {
            Some(status) => Ok(Some(status.code().unwrap_or(-1))),
            None => Ok(None),
        }
    }

    /// Kill the process.
    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }
}
