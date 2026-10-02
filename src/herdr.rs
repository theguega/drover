// herdr and shell access per machine: herdr over its socket, git and transcripts over sh or ssh.

use std::{process::Stdio, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize, de::IgnoredAny};
use serde_json::json;
use tokio::process::Command;

use crate::socket::Link;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Local,
    Ssh,
}

#[derive(Clone)]
pub struct Machine {
    pub name: String,
    pub reach: Reach,
    pub link: Arc<Link>,
}

impl Machine {
    #[must_use]
    pub fn new(name: String, reach: Reach) -> Self {
        let host = (reach == Reach::Ssh).then(|| name.clone());
        Self { name, reach, link: Arc::new(Link::new(host)) }
    }

    #[must_use]
    pub fn ssh(&self) -> Option<&str> {
        (self.reach == Reach::Ssh).then_some(self.name.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    #[serde(other)]
    Unknown,
}

impl Status {
    #[must_use]
    pub fn ready(self) -> bool {
        matches!(self, Self::Idle | Self::Done)
    }

    #[must_use]
    pub fn icon(self) -> &'static str {
        match self {
            Self::Working => "●",
            Self::Blocked => "◆",
            Self::Done => "✓",
            Self::Idle => "○",
            Self::Unknown => "?",
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct PaneId(pub String);

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
        let ok = !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "#_./-".contains(c));
        if ok { Ok(Self(s.into())) } else { Err(Error::Unsafe) }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Agent {
    pub pane_id: PaneId,
    pub workspace_id: WorkspaceId,
    pub agent: String,
    pub agent_status: Status,
    pub cwd: String,
    #[serde(default)]
    agent_session: Option<Session>,
}

#[derive(Clone, Debug, Deserialize)]
struct Session {
    value: String,
}

impl Agent {
    #[must_use]
    pub fn session(&self) -> Option<SessionId> {
        self.agent_session.as_ref().and_then(|s| SessionId::parse(&s.value))
    }
}

pub struct Worktree {
    pub workspace_id: WorkspaceId,
    pub pane_id: PaneId,
    pub path: String,
    pub branch: String,
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

impl Code {
    #[must_use]
    pub fn busy(&self) -> bool {
        matches!(self, Self::AgentNotReady)
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

    #[must_use]
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Herdr { code, .. } if code.busy())
    }

    pub(crate) fn failed(message: String) -> Self {
        Self::Herdr { code: Code::Other("failed".into()), message }
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

// Clones and `repo.branch` worktrees. Set per machine in the dotfiles, expanded by that machine's shell.
const ROOT: &str = r#""${REPOS_ROOT:-$HOME/Developer}""#;

const SHORT: Duration = Duration::from_secs(15);
const LONG: Duration = Duration::from_secs(120);

struct Out {
    out: String,
    err: String,
    code: Option<i32>,
}

// Shell work herdr has no method for: git via herdr-wt, transcripts. ssh runs the login shell, so the dotfiles' env applies.
async fn sh(m: &Machine, script: &str, timeout: Duration) -> Result<Out> {
    let mut cmd = match m.reach {
        Reach::Ssh => {
            let mut c = Command::new("ssh");
            c.args(["-o", "BatchMode=yes", &m.name, script]);
            c
        }
        Reach::Local => {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            c
        }
    };
    let child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
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
    let text = if o.err.trim().is_empty() { &o.out } else { &o.err };
    Error::failed(match (text.trim(), o.code) {
        ("", Some(c)) => format!("exit {c}"),
        ("", None) => "killed".into(),
        (t, _) => t.into(),
    })
}

pub async fn agents(m: &Machine) -> Result<Vec<Agent>> {
    #[derive(Deserialize)]
    struct List {
        agents: Vec<Agent>,
    }
    Ok(m.link.call::<List>("agent.list", json!({}), SHORT).await?.agents)
}

pub async fn panes(m: &Machine) -> Result<Vec<PaneId>> {
    #[derive(Deserialize)]
    struct List {
        panes: Vec<Pane>,
    }
    #[derive(Deserialize)]
    struct Pane {
        pane_id: PaneId,
    }
    Ok(m.link.call::<List>("pane.list", json!({}), SHORT).await?.panes.into_iter().map(|p| p.pane_id).collect())
}

// Same entry point as the desk popup: a new name, an existing branch, or #pr.
pub async fn worktree(m: &Machine, repo: &Name, r#ref: &Name) -> Result<Worktree> {
    #[derive(Deserialize)]
    struct Done {
        result: Created,
    }
    #[derive(Deserialize)]
    struct Created {
        workspace: Workspace,
        root_pane: Pane,
        worktree: Tree,
    }
    #[derive(Deserialize)]
    struct Workspace {
        workspace_id: WorkspaceId,
    }
    #[derive(Deserialize)]
    struct Pane {
        pane_id: PaneId,
    }
    #[derive(Deserialize)]
    struct Tree {
        path: String,
        branch: String,
    }

    let script = format!("cd {ROOT}/{} && herdr-wt '{}' --no-focus", repo.as_str(), r#ref.as_str());
    let o = sh(m, &script, LONG).await?;
    if o.code != Some(0) {
        return Err(failure(&o));
    }
    let c = serde_json::from_str::<Done>(&o.out)?.result;
    Ok(Worktree {
        workspace_id: c.workspace.workspace_id,
        pane_id: c.root_pane.pane_id,
        path: c.worktree.path,
        branch: c.worktree.branch,
    })
}

// Resolves once the agent is ready or blocked at startup (folder trust and similar dialogs).
// A reopened workspace keeps its shell wherever it was left, so cd to the checkout first.
pub async fn start(m: &Machine, name: &str, kind: &str, pane: &PaneId, path: &str) -> Result<()> {
    let cd = json!({"pane_id": pane.0, "text": format!("cd '{path}'"), "keys": ["Enter"]});
    m.link.call::<IgnoredAny>("pane.send_input", cd, SHORT).await?;
    let params = json!({"name": name, "kind": kind, "pane_id": pane.0, "timeout_ms": 90_000});
    match m.link.call::<IgnoredAny>("agent.start", params, Duration::from_secs(100)).await {
        Err(e) if !e.is(&Code::AgentNotReady) => Err(e),
        _ => Ok(()),
    }
}

pub async fn prompt(m: &Machine, pane: &PaneId, text: &str) -> Result<()> {
    m.link.call::<IgnoredAny>("agent.prompt", json!({"target": pane.0, "text": text}), LONG).await.map(drop)
}

pub async fn keys(m: &Machine, pane: &PaneId, keys: &[&str]) -> Result<()> {
    m.link.call::<IgnoredAny>("agent.send_keys", json!({"target": pane.0, "keys": keys}), SHORT).await.map(drop)
}

pub async fn screen(m: &Machine, pane: &PaneId, lines: u32) -> Result<String> {
    #[derive(Deserialize)]
    struct Read {
        read: Text,
    }
    #[derive(Deserialize)]
    struct Text {
        text: String,
    }
    let params = json!({"target": pane.0, "source": "recent_unwrapped", "lines": lines});
    Ok(m.link.call::<Read>("agent.read", params, SHORT).await?.read.text)
}

pub async fn close(m: &Machine, workspace: &WorkspaceId) -> Result<()> {
    m.link.call::<IgnoredAny>("workspace.close", json!({"workspace_id": workspace.0}), LONG).await.map(drop)
}

// Through herdr-wt rm, like the desk: it keeps a dirty checkout and an unmerged branch.
pub async fn remove(m: &Machine, workspace: &WorkspaceId) -> Result<String> {
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
    let path = m
        .link
        .call::<List>("workspace.list", json!({}), SHORT)
        .await?
        .workspaces
        .into_iter()
        .find(|w| &w.workspace_id == workspace)
        .and_then(|w| w.worktree)
        .ok_or_else(|| Error::failed("workspace has no worktree".into()))?
        .checkout_path;
    let path = Name::parse(&path)?;
    let o = sh(m, &format!("herdr-wt rm '{}'", path.as_str()), LONG).await?;
    if o.code != Some(0) {
        return Err(failure(&o));
    }
    Ok(o.out.trim().into())
}

// Same view as `wt ls` at the desk: every repo's worktrees with their herdr state.
pub async fn worktrees(m: &Machine) -> Result<String> {
    let o = sh(m, "cd ~ && herdr-wt ls", Duration::from_secs(30)).await?;
    let text = if o.out.trim().is_empty() { o.err } else { o.out };
    Ok(text.trim_end().into())
}

// Main clones only: a worktree has a .git file, a clone a .git directory.
pub async fn repos(m: &Machine) -> Result<Vec<String>> {
    let o = sh(m, &format!(r#"for d in {ROOT}/*/; do [ -d "$d.git" ] && basename "$d"; done"#), SHORT).await?;
    Ok(o.out.lines().filter(|l| !l.is_empty()).map(String::from).collect())
}

// ── transcript ──────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    is_meta: bool,
    #[serde(default)]
    is_compact_summary: bool,
    #[serde(default)]
    uuid: String,
    message: Option<Msg>,
}

#[derive(Deserialize)]
struct Msg {
    content: Option<Content>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(#[allow(dead_code, reason = "only the variant matters")] String), // must stay a String: untagged tries variants in order
    Blocks(Vec<Block>),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

impl Line {
    fn content(&self) -> Option<&Content> {
        self.message.as_ref()?.content.as_ref()
    }

    fn is_prompt(&self) -> bool {
        self.kind == "user"
            && !self.is_meta
            && !self.is_compact_summary
            && match self.content() {
                Some(Content::Text(_)) => true,
                Some(Content::Blocks(b)) => b.iter().any(|b| b.kind == "text"),
                _ => false,
            }
    }

    fn texts(&self) -> impl Iterator<Item = &str> {
        let blocks = match self.content() {
            Some(Content::Blocks(b)) => b.as_slice(),
            _ => &[],
        };
        blocks.iter().filter(|b| b.kind == "text").filter_map(|b| b.text.as_deref()).map(str::trim)
    }
}

pub struct Reply {
    pub id: String,
    pub text: String,
}

// Last assistant text after the latest real prompt in a Claude Code transcript.
pub async fn reply(m: &Machine, session: &SessionId) -> Option<Reply> {
    let file = format!("{}.jsonl", session.0);
    if m.reach == Reach::Ssh {
        let o = sh(m, &format!("cat ~/.claude/projects/*/{file}"), SHORT).await.ok()?;
        return if o.code == Some(0) { last_reply(&o.out) } else { None };
    }
    let projects = std::path::Path::new(&std::env::var("HOME").ok()?).join(".claude/projects");
    let mut dirs = tokio::fs::read_dir(projects).await.ok()?;
    while let Ok(Some(d)) = dirs.next_entry().await {
        if let Ok(text) = tokio::fs::read_to_string(d.path().join(&file)).await {
            return last_reply(&text);
        }
    }
    None
}

#[must_use]
fn last_reply(jsonl: &str) -> Option<Reply> {
    let lines: Vec<Line> = jsonl.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let from = lines.iter().rposition(Line::is_prompt).unwrap_or(0);
    lines
        .iter()
        .skip(from)
        .filter(|e| e.kind == "assistant")
        .flat_map(|e| e.texts().filter(|t| !t.is_empty()).map(move |t| (e, t)))
        .last()
        .map(|(e, t)| Reply { id: e.uuid.clone(), text: t.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_after_latest_prompt() {
        let jsonl = [
            r#"{"type":"user","message":{"content":"first"}}"#,
            r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"text","text":"old"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second"}]}}"#,
            r#"{"type":"assistant","uuid":"a2","message":{"content":[{"type":"text","text":" new "},{"type":"tool_use","id":"t"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
            r#"{"type":"assistant","uuid":"a3","message":{"content":[{"type":"text","text":"  "}]}}"#,
            "not json",
        ]
        .join("\n");
        let r = last_reply(&jsonl);
        assert_eq!(r.as_ref().map(|r| (r.id.as_str(), r.text.as_str())), Some(("a2", "new")));

        let only_prompt = r#"{"type":"user","message":{"content":"hi"}}"#;
        assert!(last_reply(only_prompt).is_none());
    }

    #[test]
    fn names_and_sessions() {
        assert!(Name::parse("theo/fix-1").is_ok());
        assert!(Name::parse("#42").is_ok());
        assert!(Name::parse("x'; rm -rf ~").is_err());
        assert!(SessionId::parse("0b1d1900-aaaa-bbbb-cccc-0123456789ab").is_some());
        assert!(SessionId::parse("../../etc/passwd").is_none());
    }
}
