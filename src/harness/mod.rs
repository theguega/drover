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
pub mod cursor;
pub mod pi;
pub mod swap;

// What a harness can do, declared and never guessed.
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct Caps {
    pub headless: bool,
    pub resume: bool,
    pub approvals: bool,
    pub usage: bool,
    pub hooks: bool,
    // one process serves every turn; otherwise each turn is a process, resumed by session
    pub warm: bool,
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
    // this binary, for a harness whose hooks run a command
    pub drover: &'a str,
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
    pub last: String,                   // the final message of the turn so far
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
    // The settings that make the harness post its hooks to drover: --settings, or the file below.
    fn hook_settings(&self, spec: &StartSpec) -> Value;
    // Where a harness reads them from in the run's cwd, relative, instead of from its command line.
    fn hook_file(&self, _profile: Profile) -> Option<&'static str> {
        None
    }
    // One hook the harness posted.
    fn hook(&self, body: &Value) -> Vec<Out>;
}

#[must_use]
pub fn get(name: HarnessName) -> Option<&'static dyn Harness> {
    match name {
        HarnessName::Claude => Some(&claude::Claude),
        HarnessName::Pi => Some(&pi::Pi),
        HarnessName::Cursor => Some(&cursor::Cursor),
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
// process is gone or its pid is another process now.
const TERMINATE: &str = r#"
pid=PID; start=START; grace=GRACE
[ "$(ps -o lstart= -p $pid 2>/dev/null)" = "$start" ] || exit 0
ids="$pid $(ps -e -o pid=,pgid= | awk -v g=$pid '$2 == g { print $1 }') $(ps -e -o pid=,ppid= | awk -v r=$pid '{ p[$1] = $2 } END { for (i in p) { x = i; while (x in p) { if (p[x] == r) { print i; break }; x = p[x] } } }')"
# a zombie has ended: only its parent's wait is left
alive() { for i in $ids; do case "$(ps -o stat= -p $i 2>/dev/null)" in "" | Z*) ;; *) return 0 ;; esac; done; return 1; }
kill -TERM $ids 2>/dev/null
n=0
while alive && [ $n -lt $((grace * 5)) ]; do sleep 0.2; n=$((n + 1)); done
alive && kill -KILL $ids 2>/dev/null
exit 0
"#;

pub async fn terminate(ssh: Option<&str>, ident: &Ident, grace: Duration) -> std::io::Result<()> {
    let script = TERMINATE
        .replace("PID", &ident.pid.to_string())
        .replace("START", &quote(&ident.start))
        .replace("GRACE", &grace.as_secs().max(1).to_string());
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
    tokio::time::timeout(grace + Duration::from_secs(20), status)
        .await
        .map_err(|_| std::io::Error::other("terminate timed out"))??;
    Ok(())
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
