// Long-lived herdr `events.subscribe` streams. Local uses the Unix socket;
// remotes relay stdin/stdout over ssh through a short Python bridge.

use std::{path::PathBuf, process::Stdio, time::Duration};

use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, ChildStdin, ChildStdout, Command},
};

use crate::herdr::{Error, Machine, PaneId, Reach, Result, Status};

const SHORT: Duration = Duration::from_secs(15);

/// Bidirectional bridge used on remotes: ssh → python → herdr.sock.
const RELAY: &str = r#"
import fcntl, os, select, socket, sys
path = os.path.expanduser(os.environ.get("HERDR_SOCKET_PATH") or "~/.config/herdr/herdr.sock")
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(path)
sin, sout = sys.stdin.buffer, sys.stdout.buffer
for stream in (sin, s):
    fl = fcntl.fcntl(stream.fileno(), fcntl.F_GETFL)
    fcntl.fcntl(stream.fileno(), fcntl.F_SETFL, fl | os.O_NONBLOCK)
while True:
    r, _, x = select.select([sin, s], [], [sin, s])
    if x:
        break
    if sin in r:
        data = sin.read(65536)
        if not data:
            break
        s.sendall(data)
    if s in r:
        data = s.recv(65536)
        if not data:
            break
        sout.write(data)
        sout.flush()
"#;

fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("HERDR_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/herdr/herdr.sock")
}

enum Transport {
    Local {
        stream: UnixStream,
    },
    Remote {
        // kept so kill_on_drop ends the ssh relay
        _child: Box<Child>,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
    },
}

pub struct Conn {
    transport: Transport,
    next_id: u64,
    buf: Vec<u8>,
}

#[derive(Debug)]
pub enum Push {
    Changed,
    Lost,
}

impl Conn {
    pub async fn connect(m: &Machine) -> Result<Self> {
        let transport = match m.reach {
            Reach::Local => {
                let stream = UnixStream::connect(socket_path()).await?;
                Transport::Local { stream }
            }
            Reach::Ssh => {
                let mut child = Command::new("ssh")
                    .args(["-o", "BatchMode=yes", "-T", &m.name, "python3", "-u", "-c", RELAY])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()?;
                let stdin = child.stdin.take().ok_or_else(|| Error::failed("ssh relay stdin".into()))?;
                let stdout = child.stdout.take().ok_or_else(|| Error::failed("ssh relay stdout".into()))?;
                Transport::Remote {
                    _child: Box::new(child),
                    stdin,
                    stdout: BufReader::new(stdout),
                }
            }
        };
        Ok(Self { transport, next_id: 1, buf: Vec::new() })
    }

    async fn write_line(&mut self, line: &str) -> Result<()> {
        let bytes = format!("{line}\n");
        match &mut self.transport {
            Transport::Local { stream } => {
                stream.write_all(bytes.as_bytes()).await?;
                stream.flush().await?;
            }
            Transport::Remote { stdin, .. } => {
                stdin.write_all(bytes.as_bytes()).await?;
                stdin.flush().await?;
            }
        }
        Ok(())
    }

    async fn read_line(&mut self) -> Result<String> {
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line = self.buf.drain(..=i).collect::<Vec<_>>();
                let body = line.get(..line.len().saturating_sub(1)).unwrap_or(&[]);
                return Ok(String::from_utf8_lossy(body).into_owned());
            }
            let mut tmp = [0u8; 8192];
            let n = match &mut self.transport {
                Transport::Local { stream } => stream.read(&mut tmp).await?,
                Transport::Remote { stdout, .. } => stdout.read(&mut tmp).await?,
            };
            if n == 0 {
                return Err(Error::failed("herdr event stream closed".into()));
            }
            self.buf.extend_from_slice(tmp.get(..n).unwrap_or(&[]));
        }
    }

    /// Subscribe to agent status for `panes`, plus pane/workspace teardown.
    /// `panes` must all still exist or herdr rejects the whole request.
    pub async fn subscribe(&mut self, panes: &[PaneId]) -> Result<()> {
        let id = {
            let n = self.next_id;
            self.next_id = n.saturating_add(1);
            format!("sub{n}")
        };
        let mut subs = serde_json::Value::Array(vec![
            serde_json::json!({"type": "pane.closed"}),
            serde_json::json!({"type": "pane.exited"}),
            serde_json::json!({"type": "workspace.closed"}),
        ]);
        if let Some(arr) = subs.as_array_mut() {
            for p in panes {
                arr.push(serde_json::json!({"type": "pane.agent_status_changed", "pane_id": p.0}));
            }
        }
        let req = serde_json::json!({"id": id, "method": "events.subscribe", "params": {"subscriptions": subs}});
        self.write_line(&req.to_string()).await?;

        let deadline = tokio::time::Instant::now() + SHORT;
        loop {
            let line = tokio::time::timeout_at(deadline, self.read_line())
                .await
                .map_err(|_| Error::Timeout("events.subscribe".into()))??;
            let msg: Wire = serde_json::from_str(&line)?;
            if msg.id.as_deref() == Some(id.as_str()) {
                if let Some(err) = msg.error {
                    return Err(Error::Herdr { code: err.code.into(), message: err.message });
                }
                if msg.result.as_ref().is_some_and(|r| r.kind == "subscription_started") {
                    return Ok(());
                }
                return Err(Error::failed(format!("unexpected subscribe reply: {line}")));
            }
        }
    }

    pub async fn recv(&mut self) -> Result<Push> {
        loop {
            let line = self.read_line().await?;
            let msg: Wire = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if let Some(err) = msg.error {
                if err.code == "events_lost" {
                    return Ok(Push::Lost);
                }
                return Err(Error::Herdr { code: err.code.into(), message: err.message });
            }
            let Some(event) = msg.event.as_deref() else { continue };
            let data = msg.data.unwrap_or(serde_json::Value::Null);
            match event {
                "pane_agent_status_changed" => {
                    #[derive(Deserialize)]
                    struct D {
                        #[allow(dead_code, reason = "validates shape")]
                        pane_id: String,
                        #[allow(dead_code, reason = "validates shape")]
                        agent_status: Status,
                    }
                    let _: D = serde_json::from_value(data)?;
                    return Ok(Push::Changed);
                }
                "pane_closed" | "pane_exited" => {
                    #[derive(Deserialize)]
                    struct D {
                        #[allow(dead_code, reason = "validates shape")]
                        pane_id: String,
                    }
                    let _: D = serde_json::from_value(data)?;
                    return Ok(Push::Changed);
                }
                "workspace_closed" => {
                    // payload varies; any closed workspace is enough to reconcile
                    return Ok(Push::Changed);
                }
                _ => {}
            }
        }
    }
}

#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    result: Option<WireResult>,
    #[serde(default)]
    error: Option<WireError>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct WireResult {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct WireError {
    code: String,
    message: String,
}
