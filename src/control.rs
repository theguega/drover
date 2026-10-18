// Terminal control: one JSON line in, one JSON line out, over a unix socket owned by the service.
// `watch` keeps the connection open and writes a line per change.

use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};

use serenity::all::Http;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        UnixListener, UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};

use super::*;
use crate::action::Action;

// Under the home directory, so the CLI finds the service from any folder.
pub fn path() -> Result<PathBuf> {
    Ok(PathBuf::from(env("HOME", None)?).join(".local/state/drover/drover.sock"))
}

pub async fn serve(app: Arc<App>, http: Arc<Http>) -> Result<()> {
    let path = path()?;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await.context("socket dir")?;
    }
    // a socket left by a crash would make bind fail
    let _ = tokio::fs::remove_file(&path).await;
    let listener = UnixListener::bind(&path).context(format!("bind {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    loop {
        let (stream, _) = listener.accept().await?;
        let (app, http) = (Arc::clone(&app), Arc::clone(&http));
        tokio::spawn(async move {
            if let Err(e) = handle(&app, &http, stream).await {
                eprintln!("control: {e}");
            }
        });
    }
}

async fn handle(app: &App, http: &Http, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut line = String::new();
    read.read_line(&mut line).await?;
    // the reply is the action's text, or the error it failed with
    let reply: Result<String, String> = match serde_json::from_str::<Action>(&line) {
        Ok(Action::Watch) => return watch(app, read, write).await,
        Ok(action) => app.act(http, action).await.map_err(|e| e.to_string()),
        Err(e) => Err(format!("bad request: {e}")),
    };
    write_reply(&mut write, &reply).await
}

async fn write_reply(write: &mut OwnedWriteHalf, reply: &Result<String, String>) -> Result<()> {
    let mut out = serde_json::to_string(reply)?;
    out.push('\n');
    write.write_all(out.as_bytes()).await?;
    Ok(())
}

// A snapshot now, then one per change, until the client closes its end. Saves that land
// while a snapshot is built or written coalesce into the next line.
async fn watch(
    app: &App,
    mut read: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
) -> Result<()> {
    loop {
        // enabled before the snapshot is built, so a save during it still wakes the wait
        let changed = app.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let reply = app.snapshot().await.map_err(|e| e.to_string());
        write_reply(&mut write, &reply).await?;
        let mut gone = String::new();
        tokio::select! {
            () = &mut changed => {}
            // the client sends nothing more, so any read result means it hung up
            _ = read.read_line(&mut gone) => return Ok(()),
        }
    }
}

pub async fn send(action: &Action) -> Result<String> {
    let stream = UnixStream::connect(path()?)
        .await
        .context("drover is not running here")?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_string(action)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    let mut reply = String::new();
    BufReader::new(read).read_line(&mut reply).await?;
    let r: Result<String, String> = serde_json::from_str(&reply).context("reply from drover")?;
    r.map_err(Error::from)
}

// Prints each snapshot line as it arrives, until the service or this terminal closes.
pub async fn watch_client() -> Result<()> {
    let stream = UnixStream::connect(path()?)
        .await
        .context("drover is not running here")?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_string(&Action::Watch)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    let mut lines = BufReader::new(read).lines();
    while let Some(reply) = lines.next_line().await? {
        let r: Result<String, String> =
            serde_json::from_str(&reply).context("reply from drover")?;
        println!("{}", r.map_err(Error::from)?);
    }
    Ok(())
}
