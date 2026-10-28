// HTTP/1.1 on 127.0.0.1, one request per connection: the harnesses' hooks, the event stream as
// SSE, and approvals for the dashboard's BFF.
//
//   POST /hooks/<run>                      a hook; the answer is the hook's response
//   GET  /events?run=&role=&since=         C9 events as SSE, `id:` is the row id
//   POST /runs/<run>/permissions/<perm>    {"allow": bool, "reason": "…"}, as you
//
// A browser page must not reach it: a request with an Origin header, or a Host that is not this
// address (DNS rebinding), is refused.

use std::{sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

use crate::{
    Result,
    control::Failure,
    run::{Role, RunId},
    serve::{App, Feed, Filter},
};

const MAX_HEAD: u64 = 16 << 10;
const MAX_BODY: usize = 4 << 20;
// A request's head and body arrive within this, or the connection closes.
const HEAD_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(5)
};
// An idle stream sends a comment this often, so a client that left is noticed.
const PING: Duration = Duration::from_secs(15);

pub async fn serve(app: Arc<App>, listener: TcpListener, port: u16) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            if let Err(e) = handle(&app, stream, port).await {
                eprintln!("http: {e}");
            }
        });
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

async fn handle(app: &App, stream: TcpStream, port: u16) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    // a client that never sends its request holds a socket; one waiting on an approval or
    // reading events is past this
    let Ok(req) = tokio::time::timeout(HEAD_TIMEOUT, request(&mut read, port)).await else {
        return Ok(());
    };
    let Some(req) = req? else {
        return reply(
            &mut write,
            "403 Forbidden",
            &json!({"error": "not from this machine"}),
        )
        .await;
    };
    let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    let (status, body) = match (req.method.as_str(), parts.as_slice()) {
        ("POST", ["hooks", run]) => match (
            run.parse::<RunId>(),
            serde_json::from_slice::<Value>(&req.body),
        ) {
            (Ok(run), Ok(body)) => ("200 OK", app.hook(&run, &body).await),
            _ => (
                "400 Bad Request",
                json!({"error": "a run id and a JSON body"}),
            ),
        },
        ("GET", ["events"]) => match filter(&req.query) {
            Some(f) => return events(app, f, write).await,
            None => (
                "400 Bad Request",
                json!({"error": "run=<run>&role=<role>&since=<id>"}),
            ),
        },
        ("POST", ["runs", run, "permissions", perm]) => {
            #[derive(Deserialize)]
            struct Answer {
                allow: bool,
                reason: Option<String>,
            }
            match (
                run.parse::<RunId>(),
                serde_json::from_slice::<Answer>(&req.body),
            ) {
                (Ok(run), Ok(a)) => match app.answer(None, &run, perm, a.allow, a.reason).await {
                    Ok(v) => ("200 OK", v),
                    Err(e) => ("409 Conflict", json!(Failure::from(e))),
                },
                _ => (
                    "400 Bad Request",
                    json!({"error": r#"{"allow": true|false, "reason": "…"}"#}),
                ),
            }
        }
        _ => ("404 Not Found", json!({"error": "no such route"})),
    };
    reply(&mut write, status, &body).await
}

// The request, or None when it comes from a browser page.
async fn request(
    read: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    port: u16,
) -> Result<Option<Request>> {
    let mut head = String::new();
    let mut limited = read.take(MAX_HEAD);
    loop {
        let n = limited.read_line(&mut head).await?;
        if n == 0 || head.ends_with("\r\n\r\n") || head.ends_with("\n\n") {
            break;
        }
    }
    let mut lines = head.lines();
    let first = lines.next().unwrap_or_default();
    let mut words = first.split(' ');
    let (method, target) = (
        words.next().unwrap_or_default(),
        words.next().unwrap_or_default(),
    );
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (mut len, mut local) = (0, false);
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "content-length" => len = v.parse().unwrap_or(0),
            "host" => local = v == format!("127.0.0.1:{port}") || v == format!("localhost:{port}"),
            "origin" => return Ok(None),
            _ => {}
        }
    }
    if !local {
        return Ok(None);
    }
    if len > MAX_BODY {
        bail!("a body of {len} bytes");
    }
    let mut body = vec![0; len];
    read.read_exact(&mut body).await?;
    Ok(Some(Request {
        method: method.into(),
        path: path.into(),
        query: query.into(),
        body,
    }))
}

fn filter(query: &str) -> Option<Filter> {
    let mut f = Filter::default();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=')?;
        match k {
            "run" => f.run = Some(v.parse().ok()?),
            "role" => f.role = Some(Role::parse(v)?),
            "since" => f.since = Some(v.parse().ok()?),
            _ => return None,
        }
    }
    Some(f)
}

async fn reply(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    status: &str,
    body: &Value,
) -> Result<()> {
    let body = body.to_string();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    write.write_all(head.as_bytes()).await?;
    write.write_all(body.as_bytes()).await?;
    Ok(())
}

async fn events(app: &App, f: Filter, mut write: tokio::net::tcp::OwnedWriteHalf) -> Result<()> {
    let mut feed = Feed::new(app, f)?;
    write
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n")
        .await?;
    loop {
        let chunk = match tokio::time::timeout(PING, feed.next(app)).await {
            Ok(events) => events?
                .iter()
                .map(|e| {
                    let id = e.get("id").and_then(Value::as_i64).unwrap_or_default();
                    let kind = e.get("kind").and_then(Value::as_str).unwrap_or_default();
                    format!("id: {id}\nevent: {kind}\ndata: {e}\n\n")
                })
                .collect(),
            Err(_) => ": ping\n\n".to_string(),
        };
        // a client that left fails the write
        write.write_all(chunk.as_bytes()).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A client that sends nothing is closed after HEAD_TIMEOUT; one that sends its request is answered.
    #[tokio::test]
    async fn a_silent_client_is_closed() -> Result<()> {
        use tokio::io::AsyncReadExt;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let app = crate::serve::tests::app()?;
        tokio::spawn(serve(app, listener, port));
        let mut silent = TcpStream::connect(("127.0.0.1", port)).await?;
        let mut buf = [0; 64];
        let n = tokio::time::timeout(HEAD_TIMEOUT * 5, silent.read(&mut buf)).await??;
        assert_eq!(n, 0, "closed");

        let mut hook = TcpStream::connect(("127.0.0.1", port)).await?;
        let body = "{}";
        let req = format!(
            "POST /hooks/r-00000a HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        hook.write_all(req.as_bytes()).await?;
        let mut answer = String::new();
        hook.read_to_string(&mut answer).await?;
        assert!(
            answer.starts_with("HTTP/1.1 200 OK") && answer.ends_with("{}"),
            "{answer}"
        );
        Ok(())
    }

    #[test]
    fn filters_parse() {
        let f = filter("run=r-00000a&role=crew&since=7");
        assert!(
            f.is_some_and(|f| f.run.is_some() && f.role == Some(Role::Crew) && f.since == Some(7))
        );
        assert!(filter("").is_some());
        assert!(filter("run=x").is_none() && filter("color=red").is_none());
    }
}
