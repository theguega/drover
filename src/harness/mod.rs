// One adapter per harness, behind one trait (C12). An adapter is the harness's protocol: its
// command line, how a prompt is written to it, and what its output and hooks mean as C9 events.
// Running the process, here or over ssh, is the same for every harness and lives below.

use std::{collections::HashMap, path::Path, process::Stdio};

use serde_json::Value;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::{
    herdr::{SessionId, quote},
    run::{HarnessName, Profile, Usage},
    socket::SSH_OPTS,
};

pub mod claude;

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
    End, // the session ended
}

#[derive(Clone, Debug)]
pub struct Decision {
    pub allow: bool,
    pub reason: Option<String>,
}

// Where an answer to a permission goes: back to the waiting hook, or onto the process's stdin.
pub enum Answer {
    Hook(Value),
    #[expect(dead_code, reason = "pi answers on stdin, a later adapter")]
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
        _ => None,
    }
}

pub struct Spawned {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
}

// Starts a headless process in `cwd`, here or on `ssh`'s machine, where the login shell sets PATH.
pub fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &[(&str, &str)],
    ssh: Option<&str>,
) -> std::io::Result<Spawned> {
    let mut cmd = match (ssh, argv.split_first()) {
        (Some(host), _) => {
            let exports: String = env
                .iter()
                .map(|(k, v)| format!("export {k}={} && ", quote(v)))
                .collect();
            let words: Vec<String> = argv.iter().map(|a| quote(a)).collect();
            let script = format!(
                "cd {} && {exports}exec {}",
                quote(&cwd.to_string_lossy()),
                words.join(" ")
            );
            let mut c = Command::new("ssh");
            c.args(SSH_OPTS).args(["-T", host, &script]);
            c
        }
        (None, Some((bin, args))) => {
            let mut c = Command::new(bin);
            c.args(args).current_dir(cwd).envs(env.iter().copied());
            c
        }
        (None, None) => return Err(std::io::Error::other("empty command line")),
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
