// herdr and shell access per machine. The host runs commands directly;
// other machines go through `herdr --machine <label>` and `ssh <label>`.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::LazyLock,
    time::Duration,
};

use serde::{Deserialize, Serialize, de::IgnoredAny};
use tokio::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Local,
    Ssh,
}

#[derive(Clone, Debug)]
pub struct Machine {
    pub name: String,
    pub reach: Reach,
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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{message}")]
    Herdr { code: Code, message: String },
    #[error("{0} timed out")]
    Timeout(String),
    #[error("repo and branch may only use letters, digits, # . _ / -")]
    Unsafe,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("unexpected herdr output: {0}")]
    Json(#[from] serde_json::Error),
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

// Clones and worktrees live here. Tilde form (`~/Developer`) expands in the shell.
fn root() -> &'static str {
    static ROOT: LazyLock<String> =
        LazyLock::new(|| std::env::var("REPOS_ROOT").unwrap_or_else(|_| "~/Developer".into()));
    ROOT.as_str()
}

/// Path to `bin/herdr-wt`: `HERDR_WT`, then PATH, then this repo next to the binary, then `~/.local/bin`.
#[must_use]
pub fn herdr_wt() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("HERDR_WT") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("herdr-wt");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        // target/{debug,release}/drover → ../../bin/herdr-wt
        if let Some(p) = exe.ancestors().nth(3).map(|repo| repo.join("bin/herdr-wt"))
            && p.is_file()
        {
            return Some(p);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = Path::new(&home).join(".local/bin/herdr-wt");
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

// Prefer PATH / HERDR_WT so remotes need only a stowed copy, not this repo checkout.
fn wt(args: &str) -> String {
    format!(
        r#"b="${{HERDR_WT:-}}"; [ -n "$b" ] || b="$(command -v herdr-wt 2>/dev/null || true)"; [ -n "$b" ] || b="$HOME/.local/bin/herdr-wt"; "$b" {args}"#
    )
}
const SHORT: Duration = Duration::from_secs(15);
const LIST: Duration = Duration::from_secs(20);
const LONG: Duration = Duration::from_secs(120);

#[must_use]
fn or<'a>(a: &'a str, b: &'a str) -> &'a str {
    if a.is_empty() { b } else { a }
}

struct Out {
    out: String,
    err: String,
    code: Option<i32>,
}

async fn run(prog: &str, args: &[&str], timeout: Duration) -> Result<Out> {
    let child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let o = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| Error::Timeout(prog.into()))??;
    Ok(Out {
        out: String::from_utf8_lossy(&o.stdout).into_owned(),
        err: String::from_utf8_lossy(&o.stderr).into_owned(),
        code: o.status.code(),
    })
}

async fn sh(m: &Machine, script: &str, timeout: Duration) -> Result<Out> {
    match m.reach {
        Reach::Ssh => run("ssh", &["-o", "BatchMode=yes", &m.name, script], timeout).await,
        Reach::Local => run("sh", &["-c", script], timeout).await,
    }
}

#[derive(Deserialize)]
struct Ok<T> {
    result: T,
}

#[derive(Deserialize)]
struct Failed {
    error: Failure,
}

#[derive(Deserialize)]
struct Failure {
    code: String,
    message: String,
}

fn parse<T: for<'de> Deserialize<'de>>(o: &Out) -> Result<T> {
    if o.code == Some(0) {
        return Ok(serde_json::from_str::<Ok<T>>(&o.out)?.result);
    }
    let text = or(&o.err, &o.out);
    if let Ok(Failed { error }) = serde_json::from_str(text) {
        return Err(Error::Herdr { code: error.code.into(), message: error.message });
    }
    Err(Error::failed(match (text.trim(), o.code) {
        ("", Some(c)) => format!("exit {c}"),
        ("", None) => "killed".into(),
        (t, _) => t.into(),
    }))
}

async fn herdr(m: &Machine, args: &[&str], timeout: Duration) -> Result<Out> {
    match m.reach {
        Reach::Ssh => run("herdr", &[&["--machine", &m.name], args].concat(), timeout).await,
        Reach::Local => run("herdr", args, timeout).await,
    }
}

async fn call<T: for<'de> Deserialize<'de>>(m: &Machine, args: &[&str], timeout: Duration) -> Result<T> {
    parse(&herdr(m, args, timeout).await?)
}

pub async fn agents(m: &Machine) -> Result<Vec<Agent>> {
    #[derive(Deserialize)]
    struct List {
        agents: Vec<Agent>,
    }
    Ok(call::<List>(m, &["agent", "list"], LIST).await?.agents)
}

// Same entry point as the ⌃b ⇧g popup: a new name, an existing branch, or #pr.
pub async fn worktree(m: &Machine, repo: &Name, r#ref: &Name) -> Result<Worktree> {
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

    let script = format!("cd {}/{} && {}", root(), repo.as_str(), wt(&format!("'{}' --no-focus", r#ref.as_str())));
    let c: Created = parse(&sh(m, &script, LONG).await?)?;
    Ok(Worktree {
        workspace_id: c.workspace.workspace_id,
        pane_id: c.root_pane.pane_id,
        path: c.worktree.path,
        branch: c.worktree.branch,
    })
}

// Resolves once the agent is ready or blocked at startup (folder trust and similar dialogs).
pub async fn start(m: &Machine, name: &str, kind: &str, pane: &PaneId) -> Result<()> {
    let args = ["agent", "start", name, "--kind", kind, "--pane", &pane.0, "--timeout", "90000"];
    match call::<IgnoredAny>(m, &args, Duration::from_secs(100)).await {
        Err(e) if !e.is(&Code::AgentNotReady) => Err(e),
        _ => Ok(()),
    }
}

pub async fn prompt(m: &Machine, pane: &PaneId, text: &str) -> Result<()> {
    call::<IgnoredAny>(m, &["agent", "prompt", &pane.0, text], LONG).await.map(drop)
}

pub async fn keys(m: &Machine, pane: &PaneId, keys: &[&str]) -> Result<()> {
    call::<IgnoredAny>(m, &[&["agent", "send-keys", &pane.0], keys].concat(), LONG).await.map(drop)
}

pub async fn screen(m: &Machine, pane: &PaneId, lines: u32) -> Result<String> {
    let lines = lines.to_string();
    let args = ["agent", "read", &pane.0, "--source", "recent-unwrapped", "--lines", &lines];
    let o = herdr(m, &args, LONG).await?;
    if o.code != Some(0) {
        return Err(Error::failed(or(&o.err, &o.out).trim().into()));
    }
    Ok(o.out)
}

pub async fn close(m: &Machine, workspace: &WorkspaceId) -> Result<()> {
    call::<IgnoredAny>(m, &["workspace", "close", &workspace.0], LONG).await.map(drop)
}

pub async fn remove(m: &Machine, workspace: &WorkspaceId) -> Result<()> {
    call::<IgnoredAny>(m, &["worktree", "remove", "--workspace", &workspace.0, "--force"], LONG).await.map(drop)
}

// Same view as `wt ls` at the desk: every repo's worktrees with their herdr state.
pub async fn worktrees(m: &Machine) -> Result<String> {
    let o = sh(m, &format!("cd ~ && {}", wt("ls")), Duration::from_secs(30)).await?;
    Ok(or(&o.out, &o.err).trim_end().into())
}

// Main clones only: a worktree has a .git file, a clone a .git directory.
pub async fn repos(m: &Machine) -> Result<Vec<String>> {
    let o = sh(m, &format!(r#"for d in {}/*/; do [ -d "$d.git" ] && basename "$d"; done"#, root()), SHORT).await?;
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
    let o = sh(m, &format!("cat ~/.claude/projects/*/{}.jsonl", session.0), LIST).await.ok()?;
    if o.code != Some(0) {
        return None;
    }
    last_reply(&o.out)
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
