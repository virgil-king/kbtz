use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;

pub enum LeaderMessage {
    Output(Vec<u8>),
    Exited,
}

pub struct LeaderSession {
    writer: Box<dyn Write + Send>,
    pub rx: mpsc::Receiver<LeaderMessage>,
    child: Box<dyn portable_pty::Child + Send>,
}

impl LeaderSession {
    /// Spawn an interactive leader Claude Code session.
    pub fn spawn(
        working_dir: &std::path::Path,
        session_id: Option<&str>,
        mcp_config_path: &std::path::Path,
        system_prompt: &str,
        rows: u16,
        cols: u16,
    ) -> io::Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut cmd = CommandBuilder::new("claude");
        cmd.arg("--append-system-prompt");
        cmd.arg(system_prompt);
        if let Some(sid) = session_id {
            cmd.arg("--resume");
            cmd.arg(sid);
        }
        cmd.arg("--mcp-config");
        cmd.arg(mcp_config_path);
        cmd.cwd(working_dir);

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let (tx, rx) = mpsc::channel();

        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(LeaderMessage::Exited);
                        break;
                    }
                    Ok(n) => {
                        let _ = tx.send(LeaderMessage::Output(buf[..n].to_vec()));
                    }
                    Err(_) => {
                        let _ = tx.send(LeaderMessage::Exited);
                        break;
                    }
                }
            }
        });

        Ok(Self { writer, rx, child })
    }

    pub fn write_input(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    pub fn is_alive(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}
