// Terminal front end: clap turns argv into one Cmd, which goes to the service as it is. The reply
// prints as TOON, errors too, as `error:` and `help:`. Exit 0 ok, 1 error, 2 usage error (clap's).

use std::{io::Read, path::PathBuf};

use clap::{Args, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::Paths,
    control::{self, Failure},
    run::{HarnessName, Machine, Phase, Profile, Role, RunId},
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
    /// Start a run: a worktree, and the agent in a herdr pane. With --parent it waits for your approve-run
    Run(Start),
    /// Runs, live ones unless --all or --phase
    Ls(Ls),
    /// One run: phase, owner, usage, last reply, children
    Show { run: RunId },
    /// The next prompt to a run you own; - reads it from stdin
    Send { run: RunId, text: String },
    /// Allow a tool call a run waits on [not implemented]
    Approve { run: RunId, perm: String },
    /// Deny a tool call a run waits on [not implemented]
    Deny { run: RunId, perm: String },
    /// Start a pending run
    ApproveRun { run: RunId },
    /// Remove a pending run
    DropRun { run: RunId },
    /// Block until the next reply, needs, done or failed [not implemented]
    Wait {
        #[arg(long)]
        any: bool,
        #[arg(long)]
        parent: Option<RunId>,
    },
    /// Take a run over: you type into it, its owner can only read
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
    /// Interrupt the run's current turn
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
    /// A mate's goal graph [not implemented]
    Goal {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// A question from a mate to you [not implemented]
    Escalate {
        #[arg(long)]
        text: String,
    },
    /// Open runs as JSON, then a line per change
    Watch {
        /// Done and failed runs too
        #[arg(long)]
        all: bool,
    },
    /// Open runs as JSON
    Snapshot {
        /// Done and failed runs too
        #[arg(long)]
        all: bool,
    },
    /// The service: the unix socket and one event watcher per machine
    Serve,
    #[command(hide = true)]
    Home,
}

#[derive(Args, Serialize, Deserialize)]
pub struct Start {
    /// A machine from the config [default: this one]
    #[arg(long)]
    pub machine: Option<Machine>,
    /// A repo under the repos root
    #[arg(long, required_unless_present = "scratch")]
    pub repo: Option<String>,
    /// A new name, an existing branch, or #<pr>
    #[arg(long, required_unless_present = "scratch")]
    pub branch: Option<String>,
    /// A temp dir and no repo [not implemented]
    #[arg(long, conflicts_with_all = ["repo", "branch"])]
    pub scratch: bool,
    /// In a herdr pane [default]
    #[arg(long, conflicts_with = "headless")]
    pub pane: bool,
    /// Headless, over the harness's stream [not implemented]
    #[arg(long)]
    pub headless: bool,
    #[arg(long, value_enum, default_value_t = HarnessName::Claude)]
    pub harness: HarnessName,
    #[arg(long, value_enum, default_value_t = Profile::Actor)]
    pub profile: Profile,
    /// The run that asks for this one, which then owns it and waits for your approve-run. A run (with $DROVER_RUN) is always the parent of the runs it asks for
    #[arg(long)]
    pub parent: Option<RunId>,
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
        | Cmd::Done {
            entry: Some(text), ..
        } => stdin_dash(text)?,
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

    #[test]
    fn bad_input_is_a_usage_error() {
        for line in [
            "run --prompt go",
            "run --scratch --repo x --prompt go",
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
