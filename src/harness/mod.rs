// One adapter per harness, behind one trait (C12). An adapter is the harness's protocol: its
// command line, how a prompt is written to it, and what its output and hooks mean as C9 events.
// Running the process, here or over ssh, is the same for every harness and lives below.

use std::{collections::HashMap, path::Path, process::Stdio, time::Duration};

use serde_json::{Value, json};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

use crate::{
    herdr::{SessionId, quote},
    run::{HarnessName, Profile, Usage},
    socket::SSH_OPTS,
};

pub mod acp;
pub mod claude;

// What a harness can do, declared and never guessed.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Caps {
    pub headless: bool,
    // a run in a herdr pane, observed by hooks; without, `attach` opens the TUI unobserved
    pub pane: bool,
    pub resume: bool,
    pub approvals: bool,
    pub usage: bool,
    pub hooks: bool,
}

pub struct StartSpec<'a> {
    pub headless: bool,
    pub profile: Profile,
    pub model: Option<&'a str>,
    // Claude's --permission-mode for an actor, when asked for; the others have no such thing
    pub permission_mode: Option<&'a str>,
    pub resume: Option<&'a SessionId>,
    // the run's cwd, on its machine
    pub cwd: &'a Path,
    // where the harness posts its hooks: one URL per run
    pub hooks: &'a str,
    // seconds a permission hook may wait for drover's answer
    pub hook_timeout: u64,
    // the names a remote process has the variables of `Harness::forward` drover sends it under
    pub forwarded: &'a [&'a str],
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
    // `call` is what `answer` needs to write the decision to stdin; null for a hook
    Permission {
        tool: String,
        input: Value,
        call: Value,
    },
    Reply {
        text: String,
        usage: Option<Usage>,
        error: bool,
    },
    Write(String), // a protocol line onto the process's stdin
    Fail(String),  // the process cannot serve the run
    Cancel,        // what the agent asked is over: the run's waiting calls are answered
    End,           // the session ended
    Rekey,         // the next hook brings a new session, in the same agent
}

#[derive(Clone, Debug)]
pub struct Decision {
    pub allow: bool,
    pub reason: Option<String>,
}

pub enum Abort {
    Kill,
    Stdin(Vec<String>),
}

// What an adapter remembers of one process: the reader of its stdout and the writer of prompts
// share it.
#[derive(Default)]
pub struct Parse {
    pub tools: HashMap<String, String>, // tool call id to tool name
    pub cost: f64,                      // the process's running cost, at the last turn's end
    pub acp: acp::Conn,
}

pub trait Harness: Sync {
    fn caps(&self) -> Caps;
    // The command line, binary first. A pane run starts the TUI with it.
    fn start(&self, spec: &StartSpec) -> Vec<String>;
    // Lines written first, once the process is up.
    fn open(&self, _spec: &StartSpec, _p: &mut Parse) -> Vec<String> {
        Vec::new()
    }
    // A prompt as stdin lines of a headless process: none while it is not ready for one.
    fn send(&self, text: &str, p: &mut Parse) -> Vec<String>;
    fn abort(&self, p: &mut Parse) -> Abort;
    // One stdout line of a headless process.
    fn events(&self, line: &str, p: &mut Parse) -> Vec<Out>;
    // The answer to a permission its stdout asked for: `Out::Write`s, and what follows.
    fn answer(&self, _d: &Decision, _call: &Value, _p: &mut Parse) -> Vec<Out> {
        Vec::new()
    }
    // One hook the harness posted, and the answer to a permission it asked for.
    fn hook(&self, _body: &Value) -> Vec<Out> {
        Vec::new()
    }
    fn respond(&self, _d: &Decision) -> Value {
        json!({})
    }
    // Variables of drover's environment a remote process gets on its stdin, never on a command
    // line: (drover's name, the name the process has it under).
    fn forward(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }
}

#[must_use]
pub fn get(name: HarnessName) -> Option<&'static dyn Harness> {
    match name {
        HarnessName::Claude => Some(&claude::Claude),
        HarnessName::Cursor => Some(&acp::CURSOR),
        HarnessName::Pi => Some(&acp::PI),
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
// On ssh, the variables named in `read` come first on stdin, one line each, so their values are
// on no command line: the caller writes them before anything else.
pub fn spawn(
    argv: &[String],
    cwd: &Path,
    env: &[(&str, &str)],
    ssh: Option<&str>,
    read: &[&str],
) -> std::io::Result<Spawned> {
    if argv.is_empty() {
        return Err(std::io::Error::other("empty command line"));
    }
    let mut cmd = match ssh {
        Some(host) => {
            let exports: String = env
                .iter()
                .map(|(k, v)| format!("export {k}={} && ", quote(v)))
                .chain(
                    read.iter()
                        .map(|k| format!("IFS= read -r {k} && export {k} && ")),
                )
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
                .envs(env.iter().copied())
                // a group of its own, which `terminate` signals with everything below it
                .process_group(0);
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

// Ends the process `ident` names and everything it started: SIGTERM, then SIGKILL for what is
// left after `grace`. A harness's tools run in sessions and groups of their own, so they are found
// by their parents, which a SIGKILL of the harness alone would orphan, and by the harness's group.
// One script, here or over ssh, so the tree is read before anything dies. Nothing to do when the
// process is gone or its pid is another process now; exit 1 when it outlived the SIGKILL.
// macOS pads lstart with spaces, which `Ident::parse` trims: so is it trimmed here.
const TERMINATE: &str = r#"
pid=PID; start=START; grace=GRACE
[ "$(ps -o lstart= -p $pid 2>/dev/null | sed 's/^ *//; s/ *$//')" = "$start" ] || exit 0
ids="$pid $(ps -e -o pid=,pgid= | awk -v g=$pid '$2 == g { print $1 }') $(ps -e -o pid=,ppid= | awk -v r=$pid '{ p[$1] = $2 } END { for (i in p) { x = i; while (x in p) { if (p[x] == r) { print i; break }; x = p[x] } } }')"
# a zombie has ended: only its parent's wait is left
alive() { for i in $ids; do case "$(ps -o stat= -p $i 2>/dev/null)" in "" | Z*) ;; *) return 0 ;; esac; done; return 1; }
kill -TERM $ids 2>/dev/null
n=0
while alive && [ $n -lt $((grace * 5)) ]; do sleep 0.2; n=$((n + 1)); done
alive && kill -KILL $ids 2>/dev/null
n=0
while alive && [ $n -lt 10 ]; do sleep 0.2; n=$((n + 1)); done
alive && exit 1
exit 0
"#;

pub async fn terminate(ssh: Option<&str>, ident: &Ident, grace: Duration) -> std::io::Result<()> {
    let script = TERMINATE
        .replace("PID", &ident.pid.to_string())
        .replace("START", &quote(&ident.start))
        .replace("GRACE", &grace.as_secs().max(1).to_string());
    let mut cmd = match ssh {
        // under sh: a login shell such as zsh does not split `$ids` into words
        Some(host) => {
            let mut c = Command::new("ssh");
            c.args(SSH_OPTS)
                .args([host, &format!("exec sh -c {}", quote(&script))]);
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
    let status = tokio::time::timeout(grace + Duration::from_secs(20), status)
        .await
        .map_err(|_| std::io::Error::other("terminate timed out"))??;
    // the caller keeps its record of the process until it is gone
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "pid {} is still running",
            ident.pid
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    async fn started(argv: &[&str]) -> Result<(Spawned, Ident), Box<dyn std::error::Error>> {
        let argv: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
        let mut sp = spawn(&argv, Path::new("/"), &[], None, &[])?;
        let mut line = String::new();
        BufReader::new(&mut sp.stderr).read_line(&mut line).await?;
        let ident = Ident::parse(&line).ok_or("no ident line")?;
        Ok((sp, ident))
    }

    fn running(pid: u32) -> bool {
        let stat = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        // a zombie has ended
        !stat.is_empty() && !stat.starts_with('Z')
    }

    fn pgrep(pattern: &str) -> Vec<u32> {
        let out = std::process::Command::new("pgrep")
            .args(["-f", pattern])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        out.split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect()
    }

    // The ident is the exec'd process's own, and terminate ends only the process it names.
    #[tokio::test]
    async fn terminate_ends_the_process_it_names() -> Result<(), Box<dyn std::error::Error>> {
        let (sp, ident) = started(&["sleep", "30"]).await?;
        assert_eq!(sp.child.id(), Some(ident.pid));
        let other = Ident {
            start: "Thu Jan  1 00:00:00 1970".into(),
            ..ident.clone()
        };
        terminate(None, &other, Duration::from_secs(1)).await?;
        assert!(running(ident.pid));
        terminate(None, &ident, Duration::from_secs(5)).await?;
        assert!(!running(ident.pid));
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
        assert!(!running(ident.pid));
        Ok(())
    }

    // A tool in a session of its own, and one the process left running as it ended, go with it.
    #[tokio::test]
    async fn terminate_takes_the_tools_with_it() -> Result<(), Box<dyn std::error::Error>> {
        let (_sp, ident) = started(&[
            "sh",
            "-c",
            "setsid sleep 30417 & (trap '' TERM; sleep 30418) & wait",
        ])
        .await?;
        for _ in 0..40 {
            if !pgrep("^sleep 30417$").is_empty() && !pgrep("^sleep 30418$").is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!pgrep("^sleep 30417$").is_empty());
        terminate(None, &ident, Duration::from_secs(1)).await?;
        assert!(pgrep("^sleep 30417$").is_empty() && pgrep("^sleep 30418$").is_empty());
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
