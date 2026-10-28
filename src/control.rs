// The service's unix socket: one JSON line in, one JSON line out. `watch` keeps the connection
// open and writes a line per event.

use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

use crate::{
    Error, Result,
    cli::{Cmd, Request},
    config::Paths,
    serve::{App, Feed, Filter},
};

// An error as the terminal prints it: what went wrong and the command that fixes it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Failure {
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

impl Failure {
    pub fn new(error: impl Into<String>, help: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            help: Some(help.into()),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for Failure {}

// A Failure raised in the service keeps its help; any other error has none.
impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        match e.downcast::<Self>() {
            Ok(f) => *f,
            Err(e) => Self {
                error: e.to_string(),
                help: None,
            },
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Error::from(e).into()
    }
}

pub async fn serve(app: Arc<App>, path: &Path) -> Result<()> {
    // a socket left by a crash would make bind fail; the state dir lock says no serve owns it
    let _ = tokio::fs::remove_file(path).await;
    let listener = UnixListener::bind(path).map_err(|e| err!("bind {}: {e}", path.display()))?;
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            if let Err(e) = handle(&app, stream).await {
                eprintln!("control: {e}");
            }
        });
    }
}

async fn handle(app: &App, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut line = String::new();
    read.read_line(&mut line).await?;
    let reply = match serde_json::from_str::<Request>(&line) {
        Ok(Request {
            cmd: Cmd::Watch { run, role, since },
            ..
        }) => return watch_loop(app, Filter { run, role, since }, read, write).await,
        Ok(r) => app.act(r.from, r.cmd).await.map_err(Failure::from),
        Err(e) => Err(Failure::from(err!("bad request: {e}"))),
    };
    write_reply(&mut write, &reply).await
}

async fn write_reply(write: &mut OwnedWriteHalf, reply: &impl Serialize) -> Result<()> {
    let mut out = serde_json::to_string(reply)?;
    out.push('\n');
    write.write_all(out.as_bytes()).await?;
    Ok(())
}

// Each event as it comes, until the client closes its end.
async fn watch_loop(
    app: &App,
    filter: Filter,
    mut read: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
) -> Result<()> {
    let mut feed = Feed::new(app, filter)?;
    loop {
        let mut gone = String::new();
        tokio::select! {
            events = feed.next(app) => {
                for e in events? {
                    write_reply(&mut write, &Ok::<_, Failure>(e)).await?;
                }
            }
            // the client sends nothing more, so any read result means it hung up
            _ = read.read_line(&mut gone) => return Ok(()),
        }
    }
}

pub async fn send(paths: &Paths, request: &Request) -> Result<Value, Failure> {
    let (read, _write) = connect(paths, request).await?;
    let mut reply = String::new();
    BufReader::new(read).read_line(&mut reply).await?;
    serde_json::from_str::<Result<Value, Failure>>(&reply)
        .map_err(|e| Failure::from(err!("reply from drover: {e}")))?
}

// Prints each event as one JSON line, until the service or this terminal closes.
pub async fn watch(paths: &Paths, request: &Request) -> Result<(), Failure> {
    // the write half stays open: dropping it shuts the socket down, which the service reads as leaving
    let (read, _write) = connect(paths, request).await?;
    let mut lines = BufReader::new(read).lines();
    while let Some(reply) = lines.next_line().await? {
        let event = serde_json::from_str::<Result<Value, Failure>>(&reply)
            .map_err(|e| Failure::from(err!("reply from drover: {e}")))??;
        println!("{event}");
    }
    Ok(())
}

async fn connect(
    paths: &Paths,
    request: &Request,
) -> Result<(OwnedReadHalf, OwnedWriteHalf), Failure> {
    let path = paths.socket();
    let Ok(stream) = UnixStream::connect(&path).await else {
        return Err(Failure::new(
            format!("drover is not running at {}", path.display()),
            "drover serve, with the same --state-dir",
        ));
    };
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_string(request).map_err(Error::from)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    Ok((read, write))
}
