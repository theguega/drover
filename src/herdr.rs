// herdr and shell access per machine: herdr over its socket, git and transcripts over sh or ssh.

use std::{
    io::SeekFrom,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize, de::IgnoredAny};
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    process::Command,
    time::sleep,
};

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
        self.agent_session
            .as_ref()
            .and_then(|s| SessionId::parse(&s.value))
    }
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
// bytes read from the end of a transcript; a turn's reply sits well inside this
const TAIL: u64 = 8 << 20;
const LONG: Duration = Duration::from_secs(120);

struct Out {
    out: String,
    err: String,
    code: Option<i32>,
}

// Shell work herdr has no method for: git via drover-wt, transcripts. ssh runs the login shell, so the dotfiles' env applies.
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

// Resolves once the agent is ready or blocked at startup (folder trust and similar dialogs).
// A reopened workspace keeps its shell wherever it was left, so cd to the checkout first. `env`
// is exported in the pane's shell, so the agent's own drover calls carry it.
pub async fn start(
    m: &Host,
    name: &str,
    kind: &str,
    pane: &PaneId,
    path: &str,
    env: &[(&str, &str)],
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
        json!({"name": name, "kind": kind, "pane_id": pane.0, "args": [], "timeout_ms": 90_000});
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
            Err(e) if !e.is(&Code::AgentNotReady) => return Err(e),
            _ => return Ok(()),
        }
    }
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
    let path = Name::parse(&checkout_path(m, workspace).await?)?;
    let o = sh(m, &format!("drover-wt rm '{}'", path.as_str()), LONG).await?;
    if o.code != Some(0) {
        return Err(failure(&o));
    }
    Ok(o.out.trim().into())
}

// Quotes one argument for the shell, for a folder that came from herdr or drover-wt.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub async fn close_pane(m: &Host, pane: &PaneId) -> Result<()> {
    m.link
        .call::<IgnoredAny>("pane.close", json!({"pane_id": pane.0}), SHORT)
        .await
        .map(drop)
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
    subtype: Option<String>,
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
    Text(#[expect(dead_code, reason = "only the variant matters")] String), // must stay a String: untagged tries variants in order
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

    // Claude Code writes this once a turn is over, after the turn's last text.
    fn ends_turn(&self) -> bool {
        self.kind == "system" && self.subtype.as_deref() == Some("turn_duration")
    }

    fn texts(&self) -> impl Iterator<Item = &str> {
        let blocks = match self.content() {
            Some(Content::Blocks(b)) => b.as_slice(),
            _ => &[],
        };
        blocks
            .iter()
            .filter(|b| b.kind == "text")
            .filter_map(|b| b.text.as_deref())
            .map(str::trim)
    }
}

pub struct Reply {
    pub id: String,
    pub text: String,
}

// The newest turn of a Claude Code transcript: the turn after its latest real prompt.
#[derive(Default)]
pub struct Turn {
    pub reply: Option<Reply>, // its last assistant text
    // the agent wrote in it and its end line has not come yet, so a later text may still land
    pub open: bool,
}

pub async fn reply(m: &Host, session: &SessionId) -> Result<Turn> {
    let turn = read_back(m, session, |text| match last_reply(text) {
        Scan::Turn(t) => Some(t),
        Scan::NoPrompt => None,
    });
    Ok(turn.await?.unwrap_or_default())
}

// Reads the transcript from the end and doubles the window until `scan` finds what it looks for,
// so a long session crosses ssh only as far as it needs. None when the file has none.
async fn read_back<T>(
    m: &Host,
    session: &SessionId,
    scan: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>> {
    let file = format!("{}.jsonl", session.0);
    let local = match m.reach {
        Reach::Ssh => None,
        Reach::Local => match find_local(&file).await? {
            Some(path) => Some(path),
            None => return Ok(None),
        },
    };
    let mut window = TAIL;
    loop {
        let (text, whole) = match &local {
            Some(path) => tail(path, window).await?,
            None => match ssh_tail(m, &file, window).await? {
                Some(t) => t,
                None => return Ok(None),
            },
        };
        match scan(&text) {
            Some(found) => return Ok(Some(found)),
            None if whole => return Ok(None),
            None => window *= 2,
        }
    }
}

async fn find_local(file: &str) -> Result<Option<PathBuf>> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"))?;
    let mut dirs = match tokio::fs::read_dir(Path::new(&home).join(".claude/projects")).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    while let Some(d) = dirs.next_entry().await? {
        let path = d.path().join(file);
        if tokio::fs::try_exists(&path).await? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

// The last `window` bytes, and whether they reach the start of the file.
async fn tail(path: &Path, window: u64) -> Result<(String, bool)> {
    let mut f = tokio::fs::File::open(path).await?;
    let len = f.metadata().await?.len();
    let from = len.saturating_sub(window);
    f.seek(SeekFrom::Start(from)).await?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).await?;
    // the first line may be cut mid-way; it fails to parse and is skipped
    Ok((String::from_utf8_lossy(&buf).into_owned(), from == 0))
}

// None when the file is not there.
async fn ssh_tail(m: &Host, file: &str, window: u64) -> Result<Option<(String, bool)>> {
    let o = sh(
        m,
        &format!("tail -c {window} ~/.claude/projects/*/{file}"),
        SHORT,
    )
    .await?;
    if o.code != Some(0) {
        return Ok(None);
    }
    // tail returns fewer bytes than asked only when the file is shorter
    let whole = (o.out.len() as u64) < window;
    Ok(Some((o.out, whole)))
}

enum Scan {
    NoPrompt,
    Turn(Turn),
}

// Walks back from the end, so only the latest turn is parsed. An interrupt or a local command
// such as /compact is a prompt with nothing after it, so its turn is not open.
#[must_use]
fn last_reply(jsonl: &str) -> Scan {
    let mut reply = None;
    // the newest line that tells: an assistant line opens the turn, the end line closes it
    let mut open = None;
    for line in jsonl.lines().rev() {
        let Ok(e) = serde_json::from_str::<Line>(line) else {
            continue;
        };
        if e.is_prompt() {
            return Scan::Turn(Turn {
                reply,
                open: open.unwrap_or(false),
            });
        }
        if e.ends_turn() {
            open.get_or_insert(false);
        }
        if e.kind != "assistant" {
            continue;
        }
        open.get_or_insert(true);
        if reply.is_none()
            && let Some(text) = e.texts().filter(|t| !t.is_empty()).last()
        {
            reply = Some(Reply {
                id: e.uuid.clone(),
                text: text.into(),
            });
        }
    }
    Scan::NoPrompt
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
        assert!(matches!(
            last_reply(&jsonl),
            Scan::Turn(Turn { reply: Some(r), .. }) if r.id == "a2" && r.text == "new"
        ));

        let only_prompt = r#"{"type":"user","message":{"content":"hi"}}"#;
        assert!(matches!(
            last_reply(only_prompt),
            Scan::Turn(Turn { reply: None, .. })
        ));
        let no_prompt = r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"text","text":"x"}]}}"#;
        assert!(matches!(last_reply(no_prompt), Scan::NoPrompt));
    }

    // Lines as Claude Code 2.1 writes them.
    const PROMPT: &str = r#"{"type":"user","message":{"content":"go"}}"#;
    const MID: &str = r#"{"type":"assistant","uuid":"m","message":{"content":[{"type":"text","text":"13 pass, then the full checks:"}]}}"#;
    const TOOL: &str =
        r#"{"type":"assistant","uuid":"t","message":{"content":[{"type":"tool_use","id":"x"}]}}"#;
    const RESULT: &str =
        r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#;
    const LAST: &str = r#"{"type":"assistant","uuid":"l","message":{"content":[{"type":"text","text":"DONE: 2cde9f6"}]}}"#;
    const END: &str = r#"{"type":"system","subtype":"turn_duration","isMeta":false}"#;
    const NOTIFY: &str = r#"{"type":"user","origin":{"kind":"task-notification"},"message":{"content":"<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>"}}"#;
    const INTERRUPT: &str = r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#;
    const COMPACT: &str =
        r#"{"type":"user","message":{"content":"<command-name>/compact</command-name>"}}"#;

    // The reply's id, and whether the turn is still open.
    fn turn(lines: &[&str]) -> Option<(Option<String>, bool)> {
        match last_reply(&lines.join("\n")) {
            Scan::Turn(t) => Some((t.reply.map(|r| r.id), t.open)),
            Scan::NoPrompt => None,
        }
    }

    // The status flips before the turn's last text is written, and the newest text is then one
    // from the middle of the turn. The turn stays open until its end line.
    #[test]
    fn a_turn_is_open_until_its_end_line() {
        let open = turn(&[PROMPT, MID, TOOL, RESULT]);
        assert_eq!(open, Some((Some("m".into()), true)));
        let ended = turn(&[PROMPT, MID, TOOL, RESULT, LAST, END]);
        assert_eq!(ended, Some((Some("l".into()), false)));
    }

    // Claude Code resumes on its own when a background task ends, with a notification prompt.
    #[test]
    fn a_task_notification_starts_a_turn_of_its_own() {
        // the mid-turn text ended its turn; the notification's turn has written no text yet
        let resuming = turn(&[PROMPT, MID, END, NOTIFY, TOOL, RESULT]);
        assert_eq!(resuming, Some((None, true)));
        let resumed = turn(&[PROMPT, MID, END, NOTIFY, TOOL, RESULT, LAST, END]);
        assert_eq!(resumed, Some((Some("l".into()), false)));
    }

    // Turns that never get an end line: there is nothing to wait for.
    #[test]
    fn an_interrupt_or_a_local_command_is_not_open() {
        assert_eq!(
            turn(&[PROMPT, MID, TOOL, RESULT, INTERRUPT]),
            Some((None, false))
        );
        assert_eq!(turn(&[PROMPT, LAST, END, COMPACT]), Some((None, false)));
        assert_eq!(turn(&[PROMPT, LAST, END, NOTIFY]), Some((None, false)));
    }

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
