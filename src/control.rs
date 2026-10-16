// Terminal control: one JSON line in, one JSON line out, over a unix socket owned by the service.

use std::{os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};

use serde::{Deserialize, Serialize};
use serenity::all::Http;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use super::*;
use crate::action::Action;

#[derive(Serialize, Deserialize)]
struct Reply {
    ok: bool,
    text: String,
}

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
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let reply = match serde_json::from_str::<Action>(&line) {
        Ok(action) => match app.act(http, action).await {
            Ok(text) => Reply { ok: true, text },
            Err(e) => Reply {
                ok: false,
                text: e.to_string(),
            },
        },
        Err(e) => Reply {
            ok: false,
            text: format!("bad request: {e}"),
        },
    };
    let mut out = serde_json::to_string(&reply)?;
    out.push('\n');
    write.write_all(out.as_bytes()).await?;
    Ok(())
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
    let r: Reply = serde_json::from_str(&reply).context("reply from drover")?;
    if r.ok {
        Ok(r.text)
    } else {
        bail!("{}", r.text)
    }
}
