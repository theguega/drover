// The dashboard: a read-only page and its live feed of the desk, served over the network.
// The page and its stream need the token in the query. The script is a separate file, so the CSP can forbid inline code.
// It shares nothing with Discord and the control socket except the action layer.

use std::{sync::Arc, time::Duration};

use serenity::all::Http;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, tcp::OwnedReadHalf},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    time::{sleep, timeout},
};

use super::*;
use crate::action::Action;

const PAGE: &str = include_str!("../web/dashboard.html");
const SCRIPT: &str = include_str!("../web/dashboard.js");

// Streams open at once. Each one holds a connection.
const CLIENTS: usize = 32;
// Bytes read from a request before the headers end. A longer request is cut off.
const MAX_HEAD: u64 = 8 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
// Changes arriving within this window go out as one snapshot.
const COALESCE: Duration = Duration::from_millis(100);
// A stream with no change for this long gets a comment, so a dead client shows up on write.
const KEEPALIVE: Duration = Duration::from_secs(15);
// Every response: no framing, no type sniffing, no referrer that could carry the token.
const SECURITY: &str = "X-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; connect-src 'self'\r\n";

// The token travels in a URL, so it is letters and digits only.
#[must_use]
pub fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|b| b.is_ascii_alphanumeric())
}

pub async fn serve(app: Arc<App>, http: Arc<Http>, port: u16, token: String) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .context(format!("bind 0.0.0.0:{port}"))?;
    let initial: Arc<str> = app.act(&http, Action::Snapshot).await?.into();
    let (feed, _) = watch::channel(initial);
    tokio::spawn(pump(Arc::clone(&app), Arc::clone(&http), feed.clone()));

    let slots = Arc::new(Semaphore::new(CLIENTS));
    let token: Arc<str> = token.into();
    loop {
        let (stream, _) = listener.accept().await?;
        let slot = Arc::clone(&slots).try_acquire_owned().ok();
        let (app, http, token, feed) = (
            Arc::clone(&app),
            Arc::clone(&http),
            Arc::clone(&token),
            feed.clone(),
        );
        tokio::spawn(async move {
            if let Err(e) = handle(&app, &http, stream, &token, &feed, slot).await {
                eprintln!("dashboard: {e}");
            }
        });
    }
}

// Computes the snapshot once per desk change, and every stream shares the result.
async fn pump(app: Arc<App>, http: Arc<Http>, feed: watch::Sender<Arc<str>>) {
    loop {
        // enabled before the read, so a change made while reading still runs the next turn
        let woke = app.changed.notified();
        tokio::pin!(woke);
        woke.as_mut().enable();
        match app.act(&http, Action::Snapshot).await {
            Ok(snap) => {
                let snap: Arc<str> = snap.into();
                feed.send_if_modified(|cur| {
                    let same = **cur == *snap;
                    if !same {
                        *cur = Arc::clone(&snap);
                    }
                    !same
                });
            }
            Err(e) => eprintln!("dashboard snapshot: {e}"),
        }
        woke.await;
        sleep(COALESCE).await;
    }
}

async fn handle(
    app: &App,
    http: &Http,
    stream: TcpStream,
    token: &str,
    feed: &watch::Sender<Arc<str>>,
    slot: Option<OwnedSemaphorePermit>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let Some(_slot) = slot else {
        return respond(
            &mut write,
            "503 Service Unavailable",
            "text/plain; charset=utf-8",
            "busy",
        )
        .await;
    };
    // a client that is slow to send its request gets dropped
    let Ok(request) = timeout(HEAD_TIMEOUT, read_head(read)).await else {
        return Ok(());
    };
    let request = request?;

    let mut parts = request.split_whitespace();
    let (method, target) = (parts.next(), parts.next().unwrap_or_default());
    if method != Some("GET") {
        return respond(
            &mut write,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            "GET only",
        )
        .await;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        // the script holds no secret, so it loads without the token
        "/app.js" => {
            return respond(
                &mut write,
                "200 OK",
                "text/javascript; charset=utf-8",
                SCRIPT,
            )
            .await;
        }
        "/" | "/snapshot" | "/events" => {}
        _ => {
            return respond(
                &mut write,
                "404 Not Found",
                "text/plain; charset=utf-8",
                "not found",
            )
            .await;
        }
    }
    if !authorized(query, token) {
        return respond(
            &mut write,
            "401 Unauthorized",
            "text/plain; charset=utf-8",
            "open the dashboard with ?token=… from DASHBOARD_TOKEN",
        )
        .await;
    }
    match path {
        "/" => respond(&mut write, "200 OK", "text/html; charset=utf-8", PAGE).await,
        "/snapshot" => {
            let json = app.act(http, Action::Snapshot).await?;
            respond(&mut write, "200 OK", "application/json", &json).await
        }
        _ => events(feed, &mut write).await,
    }
}

// The request line. The headers are read and dropped, so the reply is never cut off by a socket closed with unread input.
async fn read_head(read: OwnedReadHalf) -> Result<String> {
    let mut lines = BufReader::new(read.take(MAX_HEAD)).lines();
    let request = lines.next_line().await?.unwrap_or_default();
    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            break;
        }
    }
    Ok(request)
}

// Compares every byte, so the reply does not show how much of the token matched.
fn authorized(query: &str, token: &str) -> bool {
    let given = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("token="))
        .unwrap_or_default();
    given.len() == token.len()
        && given
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

async fn respond(
    write: &mut (impl AsyncWriteExt + Unpin),
    status: &str,
    kind: &str,
    body: &str,
) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{SECURITY}Connection: close\r\n\r\n",
        body.len()
    );
    write.write_all(head.as_bytes()).await?;
    write.write_all(body.as_bytes()).await?;
    Ok(())
}

// The current snapshot now, then one each time it changes. A silent stream gets a comment line.
async fn events(
    feed: &watch::Sender<Arc<str>>,
    write: &mut (impl AsyncWriteExt + Unpin),
) -> Result<()> {
    write
        .write_all(
            format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n{SECURITY}Connection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut rx = feed.subscribe();
    let mut current = rx.borrow_and_update().clone();
    write
        .write_all(format!("data: {current}\n\n").as_bytes())
        .await?;
    loop {
        match timeout(KEEPALIVE, rx.changed()).await {
            Ok(Ok(())) => {
                current = rx.borrow_and_update().clone();
                write
                    .write_all(format!("data: {current}\n\n").as_bytes())
                    .await?;
            }
            Ok(Err(_)) => return Ok(()),
            Err(_) => write.write_all(b": keepalive\n\n").await?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_plain_letters_and_digits() {
        assert!(valid_token("zouille"));
        assert!(!valid_token(""));
        assert!(!valid_token("has space"));
        assert!(!valid_token("abc&def=1"));
    }

    #[test]
    fn the_query_must_carry_the_token() {
        let token = "abcdefghijklmnop";
        assert!(authorized("token=abcdefghijklmnop", token));
        assert!(authorized("x=1&token=abcdefghijklmnop", token));
        assert!(!authorized("", token));
        assert!(!authorized("token=abcdefghijklmnoq", token));
        assert!(!authorized("token=abcdefghijklmno", token));
        assert!(!authorized("tokenabcdefghijklmnop", token));
    }
}
