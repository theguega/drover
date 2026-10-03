// herdr's socket: newline-delimited JSON, one request per connection (the server
// closes it after the reply). Local connects directly. A remote keeps one ssh open
// to a Python relay that opens a connection per request line and streams back every
// line it reads, so requests and subscriptions both cost a round trip, not a process.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines},
    net::UnixStream,
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
};

use crate::herdr::{Error, PaneId, Result, Status};

const RELAY: &str = r#"
import json, os, socket, sys, threading
path = os.path.expanduser(os.environ.get("HERDR_SOCKET_PATH") or "~/.config/herdr/herdr.sock")
lock = threading.Lock()
def emit(b):
    with lock:
        sys.stdout.buffer.write(b)
        sys.stdout.buffer.flush()
def serve(line):
    try:
        s = socket.socket(socket.AF_UNIX)
        s.connect(path)
        s.sendall(line)
        for reply in s.makefile("rb"):
            emit(reply)
    except Exception as e:
        rid = json.loads(line).get("id", "")
        emit((json.dumps({"id": rid, "error": {"code": "relay", "message": str(e)}}) + "\n").encode())
for line in sys.stdin.buffer:
    threading.Thread(target=serve, args=(line,), daemon=True).start()
"#;

fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("HERDR_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/herdr/herdr.sock")
}

/// A silent drop (wifi switch, NAT timeout) ends ssh in ~45 s instead of whenever TCP gives up.
pub const SSH_OPTS: [&str; 8] =
    ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"];

fn relay(host: &str) -> Result<Child> {
    Ok(Command::new("ssh")
        .args(SSH_OPTS)
        .args(["-T", host, &format!("python3 -u -c '{RELAY}'")]) // the remote shell parses this; RELAY has no single quotes
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?)
}

fn closed() -> Error {
    Error::failed("herdr socket closed".into())
}

#[derive(Deserialize)]
struct Wire {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<WireError>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    data: Option<Value>,
}

#[derive(Deserialize)]
struct WireError {
    code: String,
    message: String,
}

impl Wire {
    fn into_result<T: DeserializeOwned>(self) -> Result<T> {
        match (self.result, self.error) {
            (_, Some(e)) => Err(Error::Herdr { code: e.code.into(), message: e.message }),
            (Some(r), None) => Ok(serde_json::from_value(r)?),
            (None, None) => Err(Error::failed("herdr reply has neither result nor error".into())),
        }
    }
}

type Pending = Arc<StdMutex<HashMap<String, oneshot::Sender<Wire>>>>;

struct Relay {
    stdin: Mutex<ChildStdin>,
    pending: Pending,
    _child: Child, // kill_on_drop ends the ssh
}

/// Requests to one machine's herdr server.
pub struct Link {
    host: Option<String>, // ssh name of a remote, none for this machine
    relay: Mutex<Option<Arc<Relay>>>,
    next: AtomicU64,
}

impl Link {
    #[must_use]
    pub fn new(host: Option<String>) -> Self {
        Self { host, relay: Mutex::new(None), next: AtomicU64::new(1) }
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value, timeout: Duration) -> Result<T> {
        let id = format!("d{}", self.next.fetch_add(1, Ordering::Relaxed));
        let line = format!("{}\n", json!({"id": id, "method": method, "params": params}));
        let reply = tokio::time::timeout(timeout, self.send(&id, &line)).await;
        let reply = match reply {
            Ok(r) => r?,
            Err(_) => {
                // a write into a dead ssh still succeeds, so a timeout is the only sign; calls
                // already waiting keep the old relay alive until they finish, new ones start fresh
                if let Some(r) = self.relay.lock().await.take() {
                    r.pending.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
                }
                return Err(Error::Timeout(method.into()));
            }
        };
        reply.into_result()
    }

    async fn send(&self, id: &str, line: &str) -> Result<Wire> {
        let Some(host) = &self.host else {
            let mut s = UnixStream::connect(socket_path()).await?;
            s.write_all(line.as_bytes()).await?;
            let mut lines = BufReader::new(s).lines();
            return Ok(serde_json::from_str(&lines.next_line().await?.ok_or_else(closed)?)?);
        };
        let r = self.relay(host).await?;
        let (tx, rx) = oneshot::channel();
        r.pending.lock().unwrap_or_else(PoisonError::into_inner).insert(id.into(), tx);
        let sent = r.stdin.lock().await.write_all(line.as_bytes()).await;
        if let Some(reply) = async { sent.ok()?; rx.await.ok() }.await {
            return Ok(reply);
        }
        // the ssh died; the next call starts a fresh one
        let mut slot = self.relay.lock().await;
        if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, &r)) {
            *slot = None;
        }
        Err(closed())
    }

    async fn relay(&self, host: &str) -> Result<Arc<Relay>> {
        let mut slot = self.relay.lock().await;
        if let Some(r) = slot.as_ref() {
            return Ok(r.clone());
        }
        let mut child = relay(host)?;
        let stdin = child.stdin.take().ok_or_else(closed)?;
        let stdout = child.stdout.take().ok_or_else(closed)?;
        let pending: Pending = Arc::default();
        let routes = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(w) = serde_json::from_str::<Wire>(&line) else { continue };
                let tx = w.id.as_ref().and_then(|id| routes.lock().unwrap_or_else(PoisonError::into_inner).remove(id));
                if let Some(tx) = tx {
                    let _ = tx.send(w);
                }
            }
            // dropping the senders fails every call still waiting
            routes.lock().unwrap_or_else(PoisonError::into_inner).clear();
        });
        let r = Arc::new(Relay { stdin: Mutex::new(stdin), pending, _child: child });
        *slot = Some(r.clone());
        Ok(r)
    }
}

#[derive(Debug)]
pub enum Push {
    Status(PaneId, Status),
    Detected(PaneId), // an agent (re)started in a pane, so its session may be new
    Gone(PaneId),
    Reconcile, // a workspace closed; its payload names no pane
    Lost,
}

/// One long-lived `events.subscribe`.
pub struct Events {
    lines: Lines<BufReader<Box<dyn AsyncRead + Send + Unpin>>>,
    _write: Box<dyn AsyncWrite + Send + Unpin>, // the relay exits once its stdin closes
    _child: Option<Child>,
}

impl Events {
    /// Status for `panes`, plus agent starts and pane teardown anywhere.
    /// `panes` must all still exist or herdr rejects the whole request.
    pub async fn subscribe(host: Option<&str>, panes: &[PaneId]) -> Result<Self> {
        let mut subs = vec![
            json!({"type": "pane.closed"}),
            json!({"type": "pane.exited"}),
            json!({"type": "workspace.closed"}),
            json!({"type": "pane.agent_detected"}),
        ];
        subs.extend(panes.iter().map(|p| json!({"type": "pane.agent_status_changed", "pane_id": p.0})));
        let line = format!("{}\n", json!({"id": "sub", "method": "events.subscribe", "params": {"subscriptions": subs}}));

        let (mut write, read, child): (Box<dyn AsyncWrite + Send + Unpin>, Box<dyn AsyncRead + Send + Unpin>, _) =
            match host {
                None => {
                    let (r, w) = UnixStream::connect(socket_path()).await?.into_split();
                    (Box::new(w), Box::new(r), None)
                }
                Some(host) => {
                    let mut child = relay(host)?;
                    let w = child.stdin.take().ok_or_else(closed)?;
                    let r = child.stdout.take().ok_or_else(closed)?;
                    (Box::new(w), Box::new(r), Some(child))
                }
            };
        write.write_all(line.as_bytes()).await?;

        let mut lines = BufReader::new(read).lines();
        let ack = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await
            .map_err(|_| Error::Timeout("events.subscribe".into()))??
            .ok_or_else(closed)?;
        serde_json::from_str::<Wire>(&ack)?.into_result::<IgnoredResult>()?;
        Ok(Self { lines, _write: write, _child: child })
    }

    pub async fn recv(&mut self) -> Result<Push> {
        loop {
            let line = self.lines.next_line().await?.ok_or_else(closed)?;
            let Ok(w) = serde_json::from_str::<Wire>(&line) else { continue };
            if let Some(e) = w.error {
                if e.code == "events_lost" {
                    return Ok(Push::Lost);
                }
                return Err(Error::Herdr { code: e.code.into(), message: e.message });
            }
            let (Some(event), Some(data)) = (w.event, w.data) else { continue };
            // herdr 0.9.3 names this one with dots and the others with underscores
            return Ok(match event.replace('.', "_").as_str() {
                "pane_agent_status_changed" => {
                    let d: Data = serde_json::from_value(data)?;
                    Push::Status(d.pane_id, d.agent_status.unwrap_or(Status::Unknown))
                }
                "pane_agent_detected" => Push::Detected(serde_json::from_value::<Data>(data)?.pane_id),
                "pane_closed" | "pane_exited" => Push::Gone(serde_json::from_value::<Data>(data)?.pane_id),
                "workspace_closed" => Push::Reconcile,
                _ => continue,
            });
        }
    }
}

#[derive(Deserialize)]
struct IgnoredResult {}

#[derive(Deserialize)]
struct Data {
    pane_id: PaneId,
    agent_status: Option<Status>,
}

