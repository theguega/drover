// One adapter per harness, behind one trait (C12). An adapter is the harness's protocol: its
// command line, how a prompt is written to it, and what its output and hooks mean as C9 events.
// Running the process, here or over ssh, is the same for every harness and lives below.

use std::{collections::HashMap, path::Path, process::Stdio, time::Duration};

use serde_json::Value;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::{
    herdr::{SessionId, quote},
    run::{HarnessName, Profile, Usage},
    socket::SSH_OPTS,
};

pub mod claude;
pub mod pi;

// What a harness can do, declared and never guessed.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Caps {
    pub headless: bool,
    pub resume: bool,
    pub approvals: bool,
    pub usage: bool,
    pub hooks: bool,
}

pub struct StartSpec<'a> {
    pub headless: bool,
    pub profile: Profile,
    pub model: Option<&'a str>,
    pub resume: Option<&'a SessionId>,
    // where the harness posts its hooks: one URL per run
    pub hooks: &'a str,
    // seconds a permission hook may wait for drover's answer
    pub hook_timeout: u64,
}

// A harness's output or hook, as drover reads it.
#[derive(Debug, PartialEq)]
pub enum Out {
    Session(SessionId),
    Prompt, // a turn began
    Text(String),
    ToolStart {
        tool: String,
    },
    ToolEnd {
        tool: String,
        ok: bool,
    },
    Permission {
        tool: String,
        input: Value,
    },
    Reply {
        text: String,
        usage: Option<Usage>,
        error: bool,
    },
    End,   // the session ended
    Rekey, // the next hook brings a new session, in the same agent
}

#[derive(Clone, Debug)]
pub struct Decision {
    pub allow: bool,
    pub reason: Option<String>,
}

// Where an answer to a permission goes: back to the waiting hook, or onto the process's stdin.
pub enum Answer {
    Hook(Value),
    #[expect(dead_code, reason = "a harness that answers on stdin, a later adapter")]
    Stdin(String),
}

pub enum Abort {
    Kill,
    #[expect(
        dead_code,
        reason = "a harness with an interrupt message, a later adapter"
    )]
    Stdin(String),
}

// What an adapter remembers while it reads one process's output.
#[derive(Default)]
pub struct Parse {
    pub tools: HashMap<String, String>, // tool call id to tool name
    pub cost: f64,                      // the process's running total
}

pub trait Harness: Sync {
    fn caps(&self) -> Caps;
    // The command line, binary first. A pane run starts the TUI with it.
    fn start(&self, spec: &StartSpec) -> Vec<String>;
    // A prompt as one stdin line of a headless process.
    fn send(&self, text: &str) -> String;
    fn respond(&self, d: &Decision) -> Answer;
    fn abort(&self) -> Abort;
    // One stdout line of a headless process.
    fn events(&self, line: &str, p: &mut Parse) -> Vec<Out>;
    // The settings that make the harness post its hooks to drover.
    fn hook_settings(&self, spec: &StartSpec) -> Value;
    // One hook the harness posted.
    fn hook(&self, body: &Value) -> Vec<Out>;
}

#[must_use]
pub fn get(name: HarnessName) -> Option<&'static dyn Harness> {
    match name {
        HarnessName::Claude => Some(&claude::Claude),
        HarnessName::Pi => Some(&pi::Pi),
        _ => None,
    }
}

pub struct Spawned {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

// The first stderr line of every headless process: its pid and start time, written by the shell
// that then execs the harness, so both stay the harness's own. A pid alone could be reused.
const IDENT: &str = r#"echo "drover-pid $$ $(ps -o lstart= -p $$)" >&2"#;

// Who a headless process is: enough to signal it after drover restarted, and never another one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ident {
    pub pid: u32,
    pub start: String,
}

impl Ident {
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        let (pid, start) = line.strip_prefix("drover-pid ")?.split_once(' ')?;
        let start = start.trim();
        let ok = !start.is_empty()
            && start
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || " :".contains(c));
        let pid = pid.parse().ok()?;
        ok.then(|| Self {
            pid,
            start: start.into(),
        })
    }
}

// Starts a headless process in `cwd`, here or on `ssh`'s machine, where the login shell sets PATH.
pub fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &[(&str, &str)],
    ssh: Option<&str>,
) -> std::io::Result<Spawned> {
    if argv.is_empty() {
        return Err(std::io::Error::other("empty command line"));
    }
    let mut cmd = match ssh {
        Some(host) => {
            let exports: String = env
                .iter()
                .map(|(k, v)| format!("export {k}={} && ", quote(v)))
                .collect();
            let words: Vec<String> = argv.iter().map(|a| quote(a)).collect();
            let script = format!(
                "cd {} && {exports}{IDENT} && exec {}",
                quote(&cwd.to_string_lossy()),
                words.join(" ")
            );
            let mut c = Command::new("ssh");
            c.args(SSH_OPTS).args(["-T", host, &script]);
            c
        }
        None => {
            let mut c = Command::new("sh");
            c.args(["-c", &format!("{IDENT} && exec \"$@\""), "sh"])
                .args(argv)
                .current_dir(cwd)
                .envs(env.iter().copied());
            c
        }
    };
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let gone = || std::io::Error::other("no pipe");
    Ok(Spawned {
        stdin: child.stdin.take().ok_or_else(gone)?,
        stdout: child.stdout.take().ok_or_else(gone)?,
        stderr: child.stderr.take().ok_or_else(gone)?,
        child,
    })
}

// Sends `sig` (TERM, KILL, or 0 to look) to the process, here or over ssh, if it is still the one
// `ident` names and has not ended. False when it is gone.
pub async fn signal(ssh: Option<&str>, ident: &Ident, sig: &str) -> std::io::Result<bool> {
    let script = format!(
        // a zombie has ended: only its parent's wait is left
        r#"[ "$(ps -o lstart= -p {pid} 2>/dev/null)" = {start} ] && case "$(ps -o stat= -p {pid})" in Z*) false ;; *) kill -{sig} {pid} ;; esac"#,
        pid = ident.pid,
        start = quote(&ident.start)
    );
    let mut cmd = match ssh {
        Some(host) => {
            let mut c = Command::new("ssh");
            c.args(SSH_OPTS).args([host, &script]);
            c
        }
        None => {
            let mut c = Command::new("sh");
            c.args(["-c", &script]);
            c
        }
    };
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status();
    let status = tokio::time::timeout(Duration::from_secs(20), status)
        .await
        .map_err(|_| std::io::Error::other("signal timed out"))??;
    Ok(status.success())
}

// SIGTERM first: claude ends the tools it started, which run in sessions of their own and would
// outlive a SIGKILL. SIGKILL after `grace`.
pub async fn terminate(ssh: Option<&str>, ident: &Ident, grace: Duration) -> std::io::Result<()> {
    if !signal(ssh, ident, "TERM").await? {
        return Ok(());
    }
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if !signal(ssh, ident, "0").await? {
            return Ok(());
        }
    }
    signal(ssh, ident, "KILL").await.map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    async fn started(argv: &[&str]) -> Result<(Spawned, Ident), Box<dyn std::error::Error>> {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        let mut sp = spawn(&argv, Path::new("/"), &[], None)?;
        let mut line = String::new();
        BufReader::new(&mut sp.stderr).read_line(&mut line).await?;
        let ident = Ident::parse(&line).ok_or("no ident line")?;
        Ok((sp, ident))
    }

    // The ident is the exec'd process's own, and a signal reaches only that process.
    #[tokio::test]
    async fn terminate_ends_the_process_it_names() -> Result<(), Box<dyn std::error::Error>> {
        let (sp, ident) = started(&["sleep", "30"]).await?;
        assert_eq!(sp.child.id(), Some(ident.pid));
        let other = Ident {
            start: "Thu Jan  1 00:00:00 1970".into(),
            ..ident.clone()
        };
        assert!(!signal(None, &other, "TERM").await?);
        assert!(signal(None, &ident, "0").await?);
        terminate(None, &ident, Duration::from_secs(5)).await?;
        assert!(!signal(None, &ident, "0").await?);
        Ok(())
    }

    // One that ignores SIGTERM gets SIGKILL after the grace.
    #[tokio::test]
    async fn terminate_kills_after_the_grace() -> Result<(), Box<dyn std::error::Error>> {
        let (_sp, ident) =
            started(&["sh", "-c", "trap '' TERM; while :; do sleep 0.1; done"]).await?;
        let t = std::time::Instant::now();
        terminate(None, &ident, Duration::from_secs(1)).await?;
        assert!(t.elapsed() >= Duration::from_secs(1));
        assert!(!signal(None, &ident, "0").await?);
        Ok(())
    }

    #[test]
    fn idents_parse() {
        let i = Ident::parse("drover-pid 42 Wed Oct 28 10:01:02 2026\n");
        assert_eq!(
            i,
            Some(Ident {
                pid: 42,
                start: "Wed Oct 28 10:01:02 2026".into()
            })
        );
        assert!(Ident::parse("drover-pid 42 $(rm -rf ~)").is_none());
        assert!(Ident::parse("hello").is_none());
    }
}
