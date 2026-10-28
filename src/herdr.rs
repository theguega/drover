// herdr and shell access per machine: herdr over its socket, git over sh or ssh.

use std::{
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize, de::IgnoredAny};
use serde_json::json;
use tokio::{process::Command, time::sleep};

use crate::socket::{Link, SSH_OPTS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Local,
    Ssh,
}

// A machine as drover reaches it. Only the host's own shell gets `repos_root`; a remote's comes from its dotfiles.
#[derive(Clone)]
pub struct Host {
    pub name: String,
    pub reach: Reach,
    pub link: Arc<Link>,
    pub repos_root: Option<PathBuf>,
}

impl Host {
    #[must_use]
    pub fn new(name: String, reach: Reach, repos_root: Option<PathBuf>) -> Self {
        let host = (reach == Reach::Ssh).then(|| name.clone());
        Self {
            name,
            reach,
            link: Arc::new(Link::new(host)),
            repos_root: repos_root.filter(|_| reach == Reach::Local),
        }
    }

    #[must_use]
    pub fn ssh(&self) -> Option<&str> {
        (self.reach == Reach::Ssh).then_some(self.name.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct PaneId(pub String);

impl PaneId {
    // herdr names a pane after its workspace: `wB4:p5` is in `wB4`.
    #[must_use]
    pub fn workspace(&self) -> Option<WorkspaceId> {
        let (ws, _) = self.0.split_once(':')?;
        Some(WorkspaceId(ws.into()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct WorkspaceId(pub String);

/// A Claude Code session uuid, checked once so it is safe to splice into a shell path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
#[repr(transparent)]
pub struct SessionId(String);

impl SessionId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let ok = s.len() == 36 && s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f' | '-'));
        ok.then(|| Self(s.into()))
    }
}

impl TryFrom<String> for SessionId {
    type Error = &'static str;
    fn try_from(s: String) -> std::result::Result<Self, Self::Error> {
        Self::parse(&s).ok_or("not a session id")
    }
}

impl From<SessionId> for String {
    fn from(s: SessionId) -> Self {
        s.0
    }
}

/// A repo or branch name, checked once so it is safe to splice into a shell command.
#[derive(Clone, Debug)]
#[repr(transparent)]
pub struct Name(String);

impl Name {
    pub fn parse(s: &str) -> Result<Self> {
        // a leading dash would reach drover-wt as an option
        let ok = !s.is_empty()
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "#_./-".contains(c));
        if ok {
            Ok(Self(s.into()))
        } else {
            Err(Error::Unsafe)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Agent {
    pub pane_id: PaneId,
}

pub struct Worktree {
    pub pane_id: PaneId,
    pub path: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Code {
    AgentNotReady,
    AgentBlocked,
    Other(String),
}

impl From<String> for Code {
    fn from(s: String) -> Self {
        match s.as_str() {
            "agent_not_ready" | "agent_not_idle" => Self::AgentNotReady,
            "agent_blocked" => Self::AgentBlocked,
            _ => Self::Other(s),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Herdr { code: Code, message: String },
    Timeout(String),
    Unsafe,
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Herdr { message, .. } => f.write_str(message),
            Self::Timeout(what) => write!(f, "{what} timed out"),
            Self::Unsafe => f.write_str("repo and branch may only use letters, digits, # . _ / -"),
            Self::Io(e) => e.fmt(f),
            Self::Json(e) => write!(f, "unexpected herdr output: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl Error {
    #[must_use]
    pub fn is(&self, code: &Code) -> bool {
        matches!(self, Self::Herdr { code: c, .. } if c == code)
    }

    pub(crate) fn failed(message: String) -> Self {
        Self::Herdr {
            code: Code::Other("failed".into()),
            message,
        }
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

// Clones and `repo.branch` worktrees. Set per machine in the dotfiles, expanded by that machine's shell.
const ROOT: &str = r#""${REPOS_ROOT:-$HOME/.drover-workspace}""#;

const SHORT: Duration = Duration::from_secs(15);
const LONG: Duration = Duration::from_secs(120);

struct Out {
    out: String,
    err: String,
    code: Option<i32>,
}

// Shell work herdr has no method for: git via drover-wt. ssh runs the login shell, so the dotfiles' env applies.
async fn sh(m: &Host, script: &str, timeout: Duration) -> Result<Out> {
    let mut cmd = match m.reach {
        Reach::Ssh => {
            let mut c = Command::new("ssh");
            c.args(SSH_OPTS).args([&m.name, script]);
            c
        }
        Reach::Local => {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            if let Some(root) = &m.repos_root {
                c.env("REPOS_ROOT", root);
            }
            c
        }
    };
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let o = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| Error::Timeout(m.name.clone()))??;
    Ok(Out {
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
        code: o.status.code(),
    })
}

fn failure(o: &Out) -> Error {
    let text = if o.err.trim().is_empty() {
        &o.out
    } else {
        &o.err
    };
    Error::failed(match (text.trim(), o.code) {
        ("", Some(c)) => format!("exit {c}"),
        ("", None) => "killed".into(),
        (t, _) => t.into(),
    })
}

pub async fn agents(m: &Host) -> Result<Vec<Agent>> {
    #[derive(Deserialize)]
    struct List {
        agents: Vec<serde_json::Value>,
    }
    // a pane herdr has not detected an agent in yet is listed without one; it is not an agent yet
    Ok(m.link
        .call::<List>("agent.list", json!({}), SHORT)
        .await?
        .agents
        .into_iter()
        .filter_map(|a| serde_json::from_value(a).ok())
        .collect())
}

pub async fn panes(m: &Host) -> Result<Vec<PaneId>> {
    #[derive(Deserialize)]
    struct List {
        panes: Vec<Pane>,
    }
    #[derive(Deserialize)]
    struct Pane {
        pane_id: PaneId,
    }
    Ok(m.link
        .call::<List>("pane.list", json!({}), SHORT)
        .await?
        .panes
        .into_iter()
        .map(|p| p.pane_id)
        .collect())
}

// Same entry point as the desk popup: a new name, an existing branch, or #pr.
pub async fn worktree(m: &Host, repo: &Name, branch: &Name) -> Result<Worktree> {
    #[derive(Deserialize)]
    struct Done {
        result: Created,
    }
    #[derive(Deserialize)]
    struct Created {
        root_pane: Pane,
        worktree: Tree,
    }
    #[derive(Deserialize)]
    struct Pane {
        pane_id: PaneId,
    }
    #[derive(Deserialize)]
    struct Tree {
        path: String,
    }

    let script = format!(
        "cd {ROOT}/{} && drover-wt '{}' --no-focus",
        repo.as_str(),
        branch.as_str()
    );
    let o = sh(m, &script, LONG).await?;
    if o.code != Some(0) {
        return Err(failure(&o));
    }
    let c = serde_json::from_str::<Done>(&o.out)?.result;
    Ok(Worktree {
        pane_id: c.root_pane.pane_id,
        path: c.worktree.path,
    })
}

// Resolves once herdr sees the agent, which may still be blocked at startup (folder trust and
// similar dialogs; then agent.start fails with AgentNotReady). A reopened workspace keeps its shell wherever it was left,
// so cd to the checkout first. `env` is exported in the pane's shell, so the agent's own drover
// calls carry it; `args` follow the agent's binary.
pub async fn start(
    m: &Host,
    name: &str,
    kind: &str,
    pane: &PaneId,
    path: &str,
    env: &[(&str, &str)],
    args: &[String],
) -> Result<()> {
    let exports: String = env
        .iter()
        .map(|(k, v)| format!(" && export {k}={}", quote(v)))
        .collect();
    let cd = json!({"pane_id": pane.0, "text": format!("cd {}{exports}", quote(path)), "keys": ["Enter"]});
    m.link
        .call::<IgnoredAny>("pane.send_input", cd, SHORT)
        .await?;
    let params =
        json!({"name": name, "kind": kind, "pane_id": pane.0, "args": args, "timeout_ms": 90_000});
    // A new pane's shell may still be starting, and agent.start refuses until it prompts.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match m
            .link
            .call::<IgnoredAny>("agent.start", params.clone(), Duration::from_secs(100))
            .await
        {
            Err(e) if !e.is(&Code::AgentNotReady) && Instant::now() < deadline => {
                sleep(Duration::from_millis(500)).await;
            }
            r => return r.map(drop),
        }
    }
}

// Until the pane's agent is in one of `until` (idle, blocked, …), and which one.
pub async fn wait(m: &Host, pane: &PaneId, until: &[&str]) -> Result<String> {
    #[derive(Deserialize)]
    struct Waited {
        agent: State,
    }
    #[derive(Deserialize)]
    struct State {
        agent_status: String,
    }
    let params = json!({"target": pane.0, "until": until});
    Ok(m.link
        .call::<Waited>("agent.wait", params, Duration::from_secs(86_400))
        .await?
        .agent
        .agent_status)
}

pub async fn prompt(m: &Host, pane: &PaneId, text: &str) -> Result<()> {
    m.link
        .call::<IgnoredAny>(
            "agent.prompt",
            json!({"target": pane.0, "text": text}),
            LONG,
        )
        .await
        .map(drop)
}

pub async fn keys(m: &Host, pane: &PaneId, keys: &[&str]) -> Result<()> {
    m.link
        .call::<IgnoredAny>(
            "agent.send_keys",
            json!({"target": pane.0, "keys": keys}),
            SHORT,
        )
        .await
        .map(drop)
}

pub async fn screen(m: &Host, pane: &PaneId, lines: u32) -> Result<String> {
    #[derive(Deserialize)]
    struct Read {
        read: Text,
    }
    #[derive(Deserialize)]
    struct Text {
        text: String,
    }
    let params = json!({"target": pane.0, "source": "recent_unwrapped", "lines": lines});
    Ok(m.link
        .call::<Read>("agent.read", params, SHORT)
        .await?
        .read
        .text)
}

pub async fn close(m: &Host, workspace: &WorkspaceId) -> Result<()> {
    m.link
        .call::<IgnoredAny>(
            "workspace.close",
            json!({"workspace_id": workspace.0}),
            LONG,
        )
        .await
        .map(drop)
}

// The checkout a worktree workspace was opened on.
pub async fn checkout_path(m: &Host, workspace: &WorkspaceId) -> Result<String> {
    #[derive(Deserialize)]
    struct List {
        workspaces: Vec<Ws>,
    }
    #[derive(Deserialize)]
    struct Ws {
        workspace_id: WorkspaceId,
        worktree: Option<Tree>,
    }
    #[derive(Deserialize)]
    struct Tree {
        checkout_path: String,
    }
    Ok(m.link
        .call::<List>("workspace.list", json!({}), SHORT)
        .await?
        .workspaces
        .into_iter()
        .find(|w| &w.workspace_id == workspace)
        .and_then(|w| w.worktree)
        .ok_or_else(|| Error::failed("workspace has no worktree".into()))?
        .checkout_path)
}

// Through drover-wt rm, like the desk: it keeps a dirty checkout and an unmerged branch.
pub async fn remove(m: &Host, workspace: &WorkspaceId) -> Result<String> {
    remove_path(m, &checkout_path(m, workspace).await?).await
}

pub async fn remove_path(m: &Host, path: &str) -> Result<String> {
    let path = Name::parse(path)?;
    let o = sh(m, &format!("drover-wt rm '{}'", path.as_str()), LONG).await?;
    if o.code != Some(0) {
        return Err(failure(&o));
    }
    Ok(o.out.trim().into())
}

// Quotes one argument for the shell, for a folder that came from herdr or drover-wt.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub async fn close_pane(m: &Host, pane: &PaneId) -> Result<()> {
    m.link
        .call::<IgnoredAny>("pane.close", json!({"pane_id": pane.0}), SHORT)
        .await
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_keeps_one_argument() {
        assert_eq!(quote("/a b/c"), "'/a b/c'");
        assert_eq!(quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn names_and_sessions() {
        assert!(Name::parse("feature/fix-1").is_ok());
        assert!(Name::parse("#42").is_ok());
        assert!(Name::parse("x'; rm -rf ~").is_err());
        assert!(SessionId::parse("0b1d1900-aaaa-bbbb-cccc-0123456789ab").is_some());
        assert!(SessionId::parse("../../etc/passwd").is_none());
    }
}
