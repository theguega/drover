// Terminal front end: clap turns argv into one Cmd, which goes to the service as it is. The reply
// prints as TOON, errors too, as `error:` and `help:`. Exit 0 ok, 1 error, 2 usage error (clap's).

use std::{io::Read, path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::Paths,
    control::{self, Failure},
    run::{HarnessName, Machine, NodeStatus, Phase, Profile, Review, Role, RunId},
    serve, toon,
};

#[derive(Parser)]
#[command(
    name = "drover",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("DROVER_GIT_SHA"), ")"),
    about = "Runs coding agents in git worktrees, on this machine or over ssh. With no command: the runs that need you."
)]
pub struct Cli {
    /// Where drover.db and drover.sock live [default: $XDG_STATE_HOME/drover or ~/.local/state/drover]
    #[arg(long, global = true, env = "DROVER_STATE_DIR", value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// The config file [default: ~/.config/drover/config.toml]
    #[arg(long, global = true, env = "DROVER_CONFIG", value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

// A command as the service runs it: what clap parsed is what goes over the socket.
#[derive(Subcommand, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Cmd {
    /// Start a run: a worktree, and the agent in a herdr pane or headless. With --parent it waits for your approve-run
    Run(Start),
    /// Runs, live ones unless --all or --phase
    Ls(Ls),
    /// One run: phase, owner, usage, last reply, children
    Show { run: RunId },
    /// The next prompt to a run you own; - reads it from stdin
    Send { run: RunId, text: String },
    /// Allow a tool call a run waits on (its `needs` perm). Late, it resumes the run with the approval
    Approve { run: RunId, perm: String },
    /// Deny a tool call a run waits on; the agent reads the reason
    Deny {
        run: RunId,
        perm: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Start a pending run
    ApproveRun { run: RunId },
    /// Remove a pending run
    DropRun { run: RunId },
    /// Block until the next reply, needs, done or failed of your crew, or of --parent's. Each call
    /// returns what came after the last one, so nothing is lost between two
    Wait {
        /// The first event from any run in scope; the only mode
        #[arg(long)]
        any: bool,
        /// A run's children [default: your own, for a run; every run, for you]
        #[arg(long)]
        parent: Option<RunId>,
        /// Seconds to wait, then return no events
        #[arg(long, default_value_t = 90)]
        timeout: u64,
    },
    /// Take a run over: you type into it, its owner can only read. A headless run reopens in a pane
    Attach { run: RunId },
    /// The end of a pane run's terminal
    Screen {
        run: RunId,
        #[arg(long, default_value_t = 40)]
        lines: u32,
    },
    /// Keys to a pane run: enter, esc, tab, up, down, ctrl+c, 1, 2, …
    Keys {
        run: RunId,
        #[arg(required = true)]
        keys: Vec<String>,
    },
    /// Interrupt the run's current turn; a headless run's process is stopped, and the next send resumes it
    Stop { run: RunId },
    /// Close a run: `brain log` its entry, close its pane, and with --remove its worktree if clean and merged
    Done {
        run: RunId,
        #[arg(long)]
        remove: bool,
        /// The journal entry for `brain log`; - reads it from stdin
        #[arg(long)]
        entry: Option<String>,
    },
    /// A mate's goal graph: stored with the run, streamed as `goal` events
    Goal {
        #[command(subcommand)]
        op: GoalOp,
    },
    /// A question from a mate to you: a `needs` event, and the run waits for your `send`; - reads it from stdin
    Escalate {
        #[arg(long)]
        text: String,
    },
    /// Events as JSON lines (C9), as they happen: started, text, tool_start, tool_end, needs, goal, reply, done, failed
    Watch {
        #[arg(long)]
        run: Option<RunId>,
        #[arg(long, value_enum)]
        role: Option<Role>,
        /// Events after this id first [default: only new ones]
        #[arg(long)]
        since: Option<i64>,
    },
    /// Open runs as JSON
    Snapshot {
        /// Done and failed runs too
        #[arg(long)]
        all: bool,
    },
    /// The service: the unix socket, HTTP on 127.0.0.1 for hooks and events, and one herdr watcher per machine
    Serve,
    /// A harness's hook command: its JSON on stdin to the run's hook at host:port/path, the answer on stdout. Gives up after secs
    #[command(hide = true)]
    Hook {
        at: String,
        secs: u64,
        /// Exit 2, the harness's deny, when there is no answer; else `{}`
        #[arg(long)]
        closed: bool,
    },
    #[command(hide = true)]
    Home,
}

#[derive(Subcommand, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GoalOp {
    /// Replace the graph with a goal.json; - reads it from stdin
    Set {
        #[arg(long)]
        file: String,
        // the file's text, read here: the service may be on another machine
        #[arg(skip)]
        graph: Option<String>,
    },
    /// Change one node: only the fields given
    Node {
        id: String,
        #[arg(long, value_enum)]
        status: Option<NodeStatus>,
        #[arg(long)]
        round: Option<u32>,
        /// The crew run on this node
        #[arg(long)]
        crew: Option<RunId>,
        #[arg(long, value_enum)]
        review: Option<Review>,
    },
}

#[derive(Args, Serialize, Deserialize)]
pub struct Start {
    /// A machine from the config [default: this one]
    #[arg(long)]
    pub machine: Option<Machine>,
    /// A repo under the repos root [required for an actor]
    #[arg(long, requires = "branch")]
    pub repo: Option<String>,
    /// A new name, an existing branch, or #<pr>
    #[arg(long, requires = "repo")]
    pub branch: Option<String>,
    /// A temp dir and no repo [not implemented]
    #[arg(long, conflicts_with_all = ["repo", "branch"])]
    pub scratch: bool,
    /// In a herdr pane [default]
    #[arg(long, conflicts_with = "headless")]
    pub pane: bool,
    /// Headless, over the harness's stream: the process stays warm for the next send
    #[arg(long)]
    pub headless: bool,
    #[arg(long, value_enum, default_value_t = HarnessName::Claude)]
    pub harness: HarnessName,
    /// actor: a worktree and your allowlists. reader: brain reads and Read only, headless, in the state dir's reader/
    #[arg(long, value_enum, default_value_t = Profile::Actor)]
    pub profile: Profile,
    /// The harness's model [default: the harness's own]
    #[arg(long)]
    pub model: Option<String>,
    /// The run that asks for this one, which then owns it and waits for your approve-run. A run (with $DROVER_RUN) is always the parent of the runs it asks for
    #[arg(long)]
    pub parent: Option<RunId>,
    /// task [default], or mate: a dispatcher that follows the mates skill. Only you start a mate
    #[arg(long, value_enum)]
    pub role: Option<Role>,
    /// The first prompt; - reads it from stdin
    #[arg(long)]
    pub prompt: String,
}

#[derive(Args, Serialize, Deserialize)]
pub struct Ls {
    #[arg(long, value_enum)]
    pub role: Option<Role>,
    #[arg(long)]
    pub machine: Option<Machine>,
    #[arg(long, value_enum)]
    pub phase: Option<Phase>,
    /// Done and failed runs too
    #[arg(long)]
    pub all: bool,
}

impl Cli {
    // No command is the home view.
    #[must_use]
    pub fn into_cmd(self) -> Cmd {
        self.cmd.unwrap_or(Cmd::Home)
    }
}

// What the service gets: the command, and the run it comes from ($DROVER_RUN), none for you.
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub from: Option<RunId>,
    pub cmd: Cmd,
}

pub async fn main(cli: Cli) -> i32 {
    if let Some(Cmd::Hook { at, secs, closed }) = &cli.cmd {
        return hook(at, *secs, *closed);
    }
    let reply = run(cli).await;
    match reply {
        Ok(Value::Null) => 0,
        Ok(v) => {
            print!("{}", toon::encode(&v));
            0
        }
        Err(f) => {
            print!(
                "{}",
                toon::encode(&serde_json::to_value(&f).unwrap_or_default())
            );
            1
        }
    }
}

async fn run(cli: Cli) -> Result<Value, Failure> {
    let paths = Paths::new(cli.state_dir.clone(), cli.config.clone())?;
    let mut cmd = cli.into_cmd();
    match &mut cmd {
        Cmd::Serve => {
            return serve::run(&paths)
                .await
                .map(|()| Value::Null)
                .map_err(Failure::from);
        }
        Cmd::Run(Start { prompt: text, .. })
        | Cmd::Send { text, .. }
        | Cmd::Deny {
            reason: Some(text), ..
        }
        | Cmd::Done {
            entry: Some(text), ..
        }
        | Cmd::Escalate { text } => stdin_dash(text)?,
        Cmd::Goal {
            op: GoalOp::Set { file, graph },
        } => {
            let mut text = file.clone();
            if text == "-" {
                stdin_dash(&mut text)?;
            } else {
                text = std::fs::read_to_string(&*file).map_err(|e| {
                    Failure::new(format!("{file}: {e}"), "drover goal set --file <goal.json>")
                })?;
            }
            *graph = Some(text);
        }
        _ => {}
    }
    let from = match std::env::var("DROVER_RUN") {
        Ok(r) if !r.is_empty() => Some(
            r.parse::<RunId>()
                .map_err(|e| Failure::new(format!("DROVER_RUN: {e}"), "unset DROVER_RUN"))?,
        ),
        _ => None,
    };
    let watch = matches!(cmd, Cmd::Watch { .. });
    let snapshot = matches!(cmd, Cmd::Snapshot { .. });
    let request = Request { from, cmd };
    if watch {
        return control::watch(&paths, &request).await.map(|()| Value::Null);
    }
    let reply = control::send(&paths, &request).await?;
    // JSON for programs, one line
    if snapshot {
        println!("{reply}");
        return Ok(Value::Null);
    }
    Ok(reply)
}

// Blocks until drover answers, for at most `secs`: the harness's own timeout is longer, and one
// that fires lets the call run. A permission hook is `closed`: no answer, an error or a refusal
// is exit 2, the harness's deny.
fn hook(at: &str, secs: u64, closed: bool) -> i32 {
    let at = at.to_string();
    let answer = within(Duration::from_secs(secs), move || {
        let mut body = String::new();
        std::io::stdin().read_to_string(&mut body)?;
        post(&at, &body)
    });
    match answer {
        Ok(answer) => {
            println!("{answer}");
            0
        }
        Err(e) => {
            eprintln!("drover hook: {e}");
            if closed {
                return 2;
            }
            println!("{{}}");
            0
        }
    }
}

// `f` on a thread, given up on after `limit`.
fn within<T: Send + 'static>(
    limit: Duration,
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> std::io::Result<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(f()));
    rx.recv_timeout(limit)
        .map_err(|_| std::io::Error::other("no answer in time"))?
}

fn post(at: &str, body: &str) -> std::io::Result<String> {
    use std::io::Write;

    let (addr, path) = at
        .split_once('/')
        .ok_or_else(|| std::io::Error::other("not a host:port/path"))?;
    let mut sock = std::net::TcpStream::connect(addr)?;
    write!(
        sock,
        "POST /{path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut reply = String::new();
    sock.read_to_string(&mut reply)?;
    match reply.split_once("\r\n\r\n") {
        Some((head, answer)) if head.contains(" 200 ") => Ok(answer.into()),
        _ => Err(std::io::Error::other("drover refused the hook")),
    }
}

// `-` stands for stdin, so long text skips the shell's quoting.
fn stdin_dash(text: &mut String) -> Result<(), Failure> {
    if text == "-" {
        text.clear();
        std::io::stdin().read_to_string(text)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(line: &str) -> Result<Cmd, clap::Error> {
        Cli::try_parse_from(std::iter::once("drover").chain(line.split_whitespace()))
            .map(|c| c.cmd.unwrap_or(Cmd::Home))
    }

    #[test]
    fn the_cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn commands_cross_the_socket_as_parsed() -> Result<(), Box<dyn std::error::Error>> {
        let cmd = parse("run --repo drover --branch theo/x --parent r-00aa11 --prompt go")?;
        let wire = serde_json::to_string(&Request { from: None, cmd })?;
        let back: Request = serde_json::from_str(&wire)?;
        assert!(
            matches!(back.cmd, Cmd::Run(s) if s.repo.as_deref() == Some("drover")
            && s.parent.as_ref().is_some_and(|p| p.as_str() == "r-00aa11")
            && s.harness == HarnessName::Claude && s.profile == Profile::Actor && !s.headless)
        );
        assert!(wire.contains(r#""cmd":"run""#));
        let back: Request = serde_json::from_str(&serde_json::to_string(&Request {
            from: Some("r-00aa11".parse()?),
            cmd: parse("approve-run r-00aa22")?,
        })?)?;
        assert!(matches!(back.cmd, Cmd::ApproveRun { run } if run.as_str() == "r-00aa22"));
        Ok(())
    }

    // A server that takes the request and never answers (a stopped service) is given up on at
    // the deadline, and so is one that answers with an error.
    #[test]
    fn a_hook_gives_up_at_its_deadline() -> std::io::Result<()> {
        let silent = std::net::TcpListener::bind("127.0.0.1:0")?;
        let at = format!("{}/hooks/r-00000a", silent.local_addr()?);
        let t = std::time::Instant::now();
        let r = within(Duration::from_millis(500), move || post(&at, "{}"));
        assert!(r.is_err());
        assert!(t.elapsed() < Duration::from_secs(2));

        let refusing = std::net::TcpListener::bind("127.0.0.1:0")?;
        let at = format!("{}/hooks/r-00000a", refusing.local_addr()?);
        std::thread::spawn(move || {
            use std::io::Write;
            if let Ok((mut s, _)) = refusing.accept() {
                let _ = s.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n{}");
            }
        });
        assert!(post(&at, "{}").is_err());
        Ok(())
    }

    #[test]
    fn bad_input_is_a_usage_error() {
        for line in [
            "run --repo x --prompt go",
            "run --scratch --repo x --branch y --prompt go",
            "run --pane --headless --scratch --prompt go",
            "send r-1 hi",
            "keys r-00aa11",
            "ls --phase gone",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
        assert!(parse("ls --role crew --phase pending").is_ok());
        assert!(matches!(parse(""), Ok(Cmd::Home)));
    }
}
