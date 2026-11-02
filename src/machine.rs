// `drover machine`: the remote machines the service drives. It runs here, as you, and restarts the
// service when the set changes, since the config is read once at start.

use std::process::Command;

use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    config::{Config, Paths, set_remotes},
    control::Failure,
    run::Machine,
    socket::SSH_OPTS,
};

#[derive(Subcommand, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Op {
    /// The remote machines in the config
    Ls,
    /// Save it in herdr, install drover on it over ssh, add it to remotes, and restart the service
    Add { ssh: Machine },
    /// Remove it from herdr and from remotes, and restart the service
    Rm { ssh: Machine },
}

// Runs on the remote's login shell. A clone there is updated in place, a missing drover is installed.
const REMOTE_INSTALL: &str = r#"set -e
if command -v drover-wt >/dev/null 2>&1; then
  root=$(dirname "$(dirname "$(readlink -f "$(command -v drover-wt)")")")
  git -C "$root" pull --ff-only -q
  "$root/install.sh" --remote
else
  curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/bootstrap.sh | sh -s -- --remote
fi"#;

pub fn run(paths: &Paths, op: Op) -> Result<Value, Failure> {
    if std::env::var_os("DROVER_RUN").is_some_and(|r| !r.is_empty()) {
        return Err(Failure::new(
            "machine speaks as you, not from a run",
            "unset DROVER_RUN",
        ));
    }
    let config = Config::load(&paths.config)?;
    let mut remotes: Vec<&str> = config.remotes.iter().map(Machine::as_str).collect();
    match op {
        Op::Ls => Ok(json!({ "machines": remotes })),
        Op::Add { ssh } => {
            herdr_add(&ssh)?;
            install(&ssh)?;
            if !remotes.contains(&ssh.as_str()) {
                remotes.push(ssh.as_str());
            }
            set_remotes(&paths.config, &remotes)?;
            restart()?;
            Ok(json!({ "machine": ssh.as_str(), "remotes": remotes, "help": "drover doctor" }))
        }
        Op::Rm { ssh } => {
            for id in herdr_ids(&ssh)? {
                let ok = Command::new("herdr")
                    .args(["machine", "remove", &id])
                    .status()?
                    .success();
                if !ok {
                    return Err(Failure::new(
                        format!("herdr machine remove {id} failed"),
                        "herdr machine list",
                    ));
                }
            }
            remotes.retain(|r| *r != ssh.as_str());
            set_remotes(&paths.config, &remotes)?;
            restart()?;
            Ok(json!({ "machine": ssh.as_str(), "remotes": remotes }))
        }
    }
}

// herdr's saved machines: `id label host profile state`, tab-separated.
fn herdr_ids(ssh: &Machine) -> Result<Vec<String>, Failure> {
    let list = Command::new("herdr").args(["machine", "list"]).output()?;
    Ok(String::from_utf8_lossy(&list.stdout)
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let (id, label) = (f.next()?, f.next()?);
            (label == ssh.as_str()).then(|| id.to_owned())
        })
        .collect())
}

// herdr prepares the remote's server and saves the machine. Inherits this terminal, so it can ask for a login.
fn herdr_add(ssh: &Machine) -> Result<(), Failure> {
    if !herdr_ids(ssh)?.is_empty() {
        return Ok(());
    }
    let ok = Command::new("herdr")
        .args(["machine", "add", ssh.as_str()])
        .status()?
        .success();
    if ok {
        return Ok(());
    }
    Err(Failure::new(
        format!("herdr machine add {ssh} failed"),
        format!("herdr machine reconnect {ssh}"),
    ))
}

fn install(ssh: &Machine) -> Result<(), Failure> {
    let ok = Command::new("ssh")
        .args(SSH_OPTS)
        .args([ssh.as_str(), REMOTE_INSTALL])
        .status()?
        .success();
    if ok {
        return Ok(());
    }
    Err(Failure::new(
        format!("installing drover on {ssh} failed"),
        format!("ssh {ssh} and read the output above"),
    ))
}

// The config is read once at start, so the service has to start again to see the new set.
fn restart() -> Result<(), Failure> {
    let status = if cfg!(target_os = "macos") {
        Command::new("sh")
            .args(["-c", "launchctl kickstart -k \"gui/$(id -u)/dev.drover\""])
            .status()?
    } else {
        Command::new("systemctl")
            .args(["--user", "restart", "drover"])
            .status()?
    };
    if status.success() {
        return Ok(());
    }
    Err(Failure::new(
        "the service did not restart",
        "restart it: systemctl --user restart drover (macOS: launchctl kickstart -k gui/$(id -u)/dev.drover)",
    ))
}
