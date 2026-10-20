// What drover does, once. Discord and the terminal both build an Action; Discord posts the
// Reply as text, and the terminal prints it as TOON.

use std::{
    hash::{BuildHasher, Hasher, RandomState},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use serenity::all::{ChannelId, Http};
use tokio::time::sleep;

use super::*;

#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Action {
    New(Box<New>),
    Attach(Attach),
    Agents,
    Worktrees,
    Usage,
    Config,
    Snapshot,
    Watch,
    Screen(Look),
    Reply(Target),
    Mate(Mate),
    Scout(Scout),
    Brief(Note),
    Escalate(Note),
    ScoutClose(Target),
    Take(Target),
    Send(Send),
    Release(Target),
    Keys(Keys),
    Done(Done),
    Recall(Recall),
    JournalList(JournalList),
    JournalRemove(JournalRemove),
    Complete(Candidates),
}

// What the shell completion scripts ask for, one name per line.
#[derive(Serialize, Deserialize)]
pub enum Candidates {
    Threads,
    Machines,
    Mates,
    Panes(String), // the machine whose panes to list
    Journal,       // the newest entry ids
}

#[derive(Serialize, Deserialize)]
pub struct New {
    pub machine: String,
    pub repo: String,
    pub branch: String,
    pub prompt: String,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub scout: bool,
    // the mate that dispatches this crewmate, which then owns its pane
    pub mate: Option<MateName>,
    // the pane's label in herdr
    #[serde(default)]
    pub name: Option<String>,
    // a task by pane or thread: open a checkout at its commit, in place of machine, repo and branch
    #[serde(default)]
    pub at: Option<String>,
    // when the branch is open already: clear its agent's context and send the prompt there
    #[serde(default)]
    pub fresh: bool,
    #[serde(default)]
    pub discord: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Note {
    pub mate: MateName,
    pub text: String,
}

#[derive(Serialize, Deserialize)]
pub struct Scout {
    pub prompt: String,
    #[serde(default)]
    pub discord: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Mate {
    pub name: MateName,
    #[serde(default)]
    pub discord: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Attach {
    pub machine: Option<String>,
    pub pane: String,
    // set by link: the mate that the pane is linked to
    pub mate: Option<MateName>,
    #[serde(default)]
    pub discord: bool,
}

// A task is addressed by its herdr pane id or Discord thread id, as `agents` prints them.
#[derive(Serialize, Deserialize)]
pub struct Target {
    pub thread: String,
}

// A task's screen. Without `full` the terminal gets the end of it, a screenful of text.
#[derive(Serialize, Deserialize)]
pub struct Look {
    pub thread: String,
    #[serde(default)]
    pub full: bool,
    // a read that fails is text in the message, not an error
    #[serde(default)]
    pub discord: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Send {
    pub thread: String,
    pub text: String,
}

#[derive(Serialize, Deserialize)]
pub struct Keys {
    pub thread: String,
    pub keys: Vec<String>,
    // a read that fails is text in the message, not an error
    #[serde(default)]
    pub discord: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Done {
    pub thread: String,
    pub remove: bool,
    pub journal: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Recall {
    pub query: String,
    #[serde(default)]
    pub full: bool,
}

#[derive(Serialize, Deserialize)]
pub struct JournalList {
    // a task, by pane or thread id: list its repo's entries instead of all
    pub task: Option<String>,
    #[serde(default)]
    pub full: bool,
}

#[derive(Serialize, Deserialize)]
pub struct JournalRemove {
    pub id: i64,
}

// A scout's id: "scout-" and six hex digits, which the agent name rule allows.
fn scout_id() -> String {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("scout-{:06x}", hasher.finish() & 0xff_ffff)
}

// Deletes one scout's folder. Only a direct child of the scouts folder named scout-* goes.
fn remove_scout_dir(root: &Path, id: &str) -> Result<()> {
    let dir = root.join(id);
    if dir.parent() != Some(root) || !id.starts_with("scout-") {
        bail!("refusing to delete {}: not a scout folder", dir.display());
    }
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// What an action answers. `Text` is a command not converted yet: both sides print it as it is.
// The rest are data: the terminal prints them as TOON and Discord as `discord` renders them.
#[derive(Serialize)]
#[serde(untagged)]
pub enum Reply {
    Text(String),
    Agents(Agents),
    Config(Config),
    Worktrees(Worktrees),
    Usage(Usage),
    Screen(ScreenView),
    Journal(Entries),
    Recall(Entries),
    Change(Change),
}

impl Reply {
    #[must_use]
    pub fn discord(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Agents(a) => a.discord(),
            Self::Config(c) => c.discord(),
            Self::Worktrees(w) => w.discord.clone(),
            Self::Usage(u) => u.lines.join("\n"),
            Self::Screen(s) => fenced(&s.text, ""),
            Self::Journal(e) => e.discord("no entries", true),
            Self::Recall(e) => e.discord("nothing", false),
            Self::Change(c) => c.discord.clone(),
        }
    }
}

// What a command that changes the desk did, as the fields that feed the next command. Fields come out in
// the order they are set: the id, then `noop` when the desired state already held, then the rest.
#[derive(Serialize)]
pub struct Change {
    #[serde(flatten)]
    fields: Map<String, Value>,
    #[serde(skip)]
    discord: String,
}

impl Change {
    fn new(discord: impl Into<String>) -> Self {
        Self {
            fields: Map::new(),
            discord: discord.into(),
        }
    }

    // The task's pane and machine.
    fn at(t: &Task, noop: Option<&str>, discord: impl Into<String>) -> Self {
        Self::new(discord)
            .with("pane", &t.pane.0)
            .with("machine", &t.machine)
            .maybe("noop", noop)
    }

    // A task as the next command needs it.
    fn task(t: &Task, noop: Option<&str>, discord: impl Into<String>) -> Self {
        Self::at(t, noop, discord)
            .with("workspace", &t.workspace.0)
            .with("repo", &t.repo)
            .maybe("branch", t.branch.as_ref())
            .with("harness", &t.kind)
            .maybe("mate", t.role.mate())
            .maybe("thread", t.thread)
    }

    fn with(self, key: &str, v: impl Serialize) -> Self {
        self.maybe(key, Some(v))
    }

    fn maybe(mut self, key: &str, v: Option<impl Serialize>) -> Self {
        if let Some(v) = v {
            self.fields
                .insert(key.into(), serde_json::to_value(v).unwrap_or_default());
        }
        self
    }

    fn help(self, commands: Vec<String>) -> Self {
        match commands.is_empty() {
            true => self,
            false => self.with("help", commands),
        }
    }

    // What to do with a pane that was just opened or found.
    fn watch(self, pane: &PaneId) -> Self {
        self.help(vec![
            format!("drover screen --pane {}", pane.0),
            format!("drover send --pane {} --text <text>", pane.0),
        ])
    }
}

const AGENTS_HELP: [&str; 3] = [
    "drover screen --pane <pane>",
    "drover send --pane <pane> --text <text>",
    "drover attach --machine <machine> --pane <pane>",
];

#[derive(Serialize)]
pub struct Agents {
    pub agents: Vec<AgentRow>,
    pub machines: Vec<MachineReach>,
    pub help: [&'static str; 3],
}

#[derive(Serialize)]
pub struct MachineReach {
    pub name: String,
    pub reach: Reachability,
    // herdr's own words stay on Discord, where a person reads them
    #[serde(skip)]
    pub error: Option<String>,
}

#[derive(Serialize)]
// Fields in the order the terminal shows them: the id first, then the status.
pub struct AgentRow {
    pub pane: String,
    pub machine: String,
    pub status: Status,
    pub harness: String,
    pub cwd: String,
    #[serde(skip)]
    pub thread: Option<ChannelId>,
}

impl Agents {
    fn discord(&self) -> String {
        let mut lines = vec![];
        for m in &self.machines {
            lines.push(format!("**{}**", m.name));
            if let Some(e) = &m.error {
                lines.push(format!("unreachable: {e}"));
            }
            for a in self.agents.iter().filter(|a| a.machine == m.name) {
                let bound = a.thread.map(|b| format!(" <#{b}>")).unwrap_or_default();
                lines.push(format!(
                    "`{} {}` {} {}{bound}",
                    a.status.icon(),
                    a.pane,
                    a.harness,
                    a.cwd
                ));
            }
        }
        lines.join("\n")
    }
}

#[derive(Serialize)]
pub struct Config {
    pub host: String,
    pub remotes: Vec<String>,
    pub channel: String,
    pub repos: String,
    pub state: &'static str,
    pub journal: &'static str,
    pub socket: String,
}

impl Config {
    fn discord(&self) -> String {
        let remotes = match self.remotes.is_empty() {
            true => "none".into(),
            false => self.remotes.join(", "),
        };
        [
            format!("host: {}", self.host),
            format!("remotes: {remotes}"),
            format!("channel: <#{}>", self.channel),
            format!("repos: {}", self.repos),
            format!("state: {}", self.state),
            format!("journal: {}", self.journal),
            format!("socket: {}", self.socket),
        ]
        .join("\n")
    }
}

#[derive(Serialize)]
pub struct Worktrees {
    pub worktrees: Vec<WorktreeRow>,
    pub machines: Vec<MachineReach>,
    pub help: [&'static str; 1],
    #[serde(skip)]
    discord: String,
}

// A worktree's status is its agent's, or `open` for a workspace without a known agent state, or `closed`.
#[derive(Serialize, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Checkout {
    Working,
    Blocked,
    Done,
    Open,
    Closed,
}

#[derive(Serialize, PartialEq, Debug)]
pub struct WorktreeRow {
    pub machine: String,
    pub repo: String,
    pub branch: String,
    pub status: Checkout,
    // changed files; none when the checkout is gone
    pub changes: Option<u32>,
}

// The rows of `drover-wt ls`: a repo name, then one line per worktree: mark, branch, changes, upstream.
fn worktree_rows(machine: &str, text: &str) -> Vec<WorktreeRow> {
    let mut repo = "";
    let mut rows = vec![];
    for line in text.lines() {
        if !line.starts_with(' ') {
            repo = line.trim();
            continue;
        }
        let mut f = line.split_whitespace();
        let (Some(mark), Some(branch), Some(changes)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        rows.push(WorktreeRow {
            machine: machine.into(),
            repo: repo.into(),
            branch: branch.into(),
            status: match mark {
                "●" => Checkout::Working,
                "◆" => Checkout::Blocked,
                "✓" => Checkout::Done,
                "○" => Checkout::Open,
                _ => Checkout::Closed,
            },
            changes: match changes {
                "clean" => Some(0),
                n => n.parse().ok(),
            },
        });
    }
    rows
}

#[derive(Serialize)]
pub struct Usage {
    pub windows: Vec<Window>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub spend: Vec<Spend>,
    // herdr's own error text stays on Discord
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<Unavailable>,
    #[serde(skip)]
    pub lines: Vec<String>,
    #[serde(skip)]
    pub logins: Vec<Login>,
}

// A machine's claude account, and whether its token could be read there.
pub struct Login {
    pub machine: String,
    pub account: String,
    pub read: bool,
}

// A usage limit that resets: how much of it is used.
#[derive(Serialize)]
pub struct Window {
    pub machine: String,
    pub harness: &'static str,
    pub window: &'static str,
    pub percent: f64,
    pub resets_at: Option<i64>,
}

#[derive(Serialize)]
pub struct Spend {
    pub machine: String,
    pub harness: &'static str,
    pub pool: &'static str,
    pub used_usd: f64,
    pub limit_usd: f64,
}

#[derive(Serialize)]
pub struct Unavailable {
    pub machine: String,
    pub harness: &'static str,
}

// A task's terminal. `text` is cut to its last EXCERPT characters unless the action asked for `full`.
#[derive(Serialize)]
pub struct ScreenView {
    pub pane: String,
    pub machine: String,
    pub status: Status,
    pub chars: usize,
    pub text: String,
    pub help: Vec<String>,
}

#[derive(Serialize)]
pub struct Entries {
    // the empty state, as a line
    #[serde(skip_serializing_if = "Option::is_none")]
    pub journal: Option<String>,
    // `5 of 42`, when the list is capped
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<EntryRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub help: Vec<String>,
}

// An entry's text is cut to its first EXCERPT characters unless the action asked for `full`.
#[derive(Serialize)]
pub struct EntryRow {
    pub id: i64,
    pub repo: String,
    pub branch: String,
    pub day: String,
    pub chars: usize,
    pub text: String,
}

// Characters of long text the terminal shows without --full.
const EXCERPT: usize = 1000;
// Entries a journal list or a recall shows.
const ENTRIES: u32 = 5;

impl Entries {
    // `none` is the line for no entries; `full_cmd` is the command that shows them whole.
    fn new(
        rows: Vec<(i64, journal::Entry)>,
        total: u32,
        full: bool,
        none: String,
        full_cmd: String,
    ) -> Self {
        let cut = !full && rows.iter().any(|(_, e)| e.text.chars().count() > EXCERPT);
        let entries: Vec<EntryRow> = rows
            .into_iter()
            .map(|(id, e)| {
                let chars = e.text.chars().count();
                let text = match full || chars <= EXCERPT {
                    true => e.text,
                    false => format!("{}…", head(&e.text, EXCERPT)),
                };
                EntryRow {
                    id,
                    repo: e.repo,
                    branch: e.branch,
                    day: e.day,
                    chars,
                    text,
                }
            })
            .collect();
        let mut help = vec![];
        if cut {
            help.push(full_cmd);
        }
        if !entries.is_empty() {
            help.push("drover journal rm <id>".into());
        }
        Self {
            journal: entries.is_empty().then_some(none),
            count: (entries.len() < total as usize)
                .then(|| format!("{} of {total}", entries.len())),
            entries,
            help,
        }
    }

    fn discord(&self, none: &str, numbered: bool) -> String {
        if self.entries.is_empty() {
            return none.into();
        }
        self.entries
            .iter()
            .map(|e| {
                let id = if numbered {
                    format!("#{} ", e.id)
                } else {
                    String::new()
                };
                format!("{id}{} {} {}\n{}", e.day, e.repo, e.branch, e.text)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

// What became of a prompt.
pub enum Sent {
    Now,
    Queued,  // the agent works; it goes out on the next idle
    Blocked, // a dialog holds the agent; nothing was sent
}

// The --machine --repo --branch of a task, for a command that opens the same place again.
fn place_of(t: &Task) -> String {
    format!(
        "--machine {} --repo {} --branch {}",
        t.machine,
        t.repo,
        t.branch.as_deref().unwrap_or("<branch>")
    )
}

// Most mates a desk runs at once.
const MAX_MATES: usize = 3;

// What a mate is told before its first message: its instructions live in mates/AGENTS.md.
fn mate_role(name: &MateName) -> String {
    format!(
        "You are mate {name}. Your instructions are in AGENTS.md in this folder. Wait for the desk's first message."
    )
}

// What to do with a mate that runs.
fn mate_help(name: &MateName, machine: &str) -> Vec<String> {
    vec![
        format!("drover brief --mate {name} --text <text>"),
        format!(
            "drover new --machine {machine} --repo <repo> --branch <branch> --prompt <prompt> --mate {name}"
        ),
    ]
}

// A mate's name, checked once: it becomes an agent name and a folder under mates/state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MateName(String);

impl MateName {
    // Agent names take 32 characters, and "mate-" takes five of them.
    pub fn parse(s: &str) -> Result<Self> {
        let valid = s.len() <= 26
            && s.starts_with(|c: char| c.is_ascii_lowercase())
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if valid {
            Ok(Self(s.into()))
        } else {
            bail!(
                "a mate name is lowercase letters, digits, - or _, starting with a letter, up to 26 characters"
            )
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for MateName {
    type Error = Error;
    fn try_from(s: String) -> Result<Self> {
        Self::parse(&s)
    }
}

impl From<MateName> for String {
    fn from(n: MateName) -> Self {
        n.0
    }
}

impl std::fmt::Display for MateName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl App {
    pub async fn act(&self, http: &Http, action: Action) -> Result<Reply> {
        let text = match action {
            Action::New(n) => return Ok(Reply::Change(self.cmd_new(http, &n).await?)),
            Action::Attach(a) => return Ok(Reply::Change(self.cmd_attach(http, &a).await?)),
            Action::Mate(m) => return Ok(Reply::Change(self.cmd_mate(http, &m).await?)),
            Action::Scout(s) => return Ok(Reply::Change(self.cmd_scout(http, &s).await?)),
            Action::Brief(b) => return Ok(Reply::Change(self.cmd_brief(&b)?)),
            Action::Escalate(n) => return Ok(Reply::Change(self.cmd_escalate(http, &n).await?)),
            Action::ScoutClose(t) => {
                return Ok(Reply::Change(self.cmd_scout_close(http, &t.thread).await?));
            }
            Action::Send(s) => return Ok(Reply::Change(self.cmd_send(&s).await?)),
            Action::Take(t) => {
                return Ok(Reply::Change(self.set_owner(&t.thread, Owner::User).await?));
            }
            Action::Release(t) => {
                return Ok(Reply::Change(self.set_owner(&t.thread, Owner::Mate).await?));
            }
            Action::Keys(k) => return Ok(Reply::Screen(self.cmd_keys(&k).await?)),
            Action::Done(d) => return Ok(Reply::Change(self.cmd_done(http, &d).await?)),
            Action::JournalRemove(r) => {
                return Ok(Reply::Change(self.cmd_journal_remove(r.id).await?));
            }
            Action::Agents => return Ok(Reply::Agents(self.cmd_agents().await)),
            Action::Config => return Ok(Reply::Config(self.config())),
            Action::Worktrees => return Ok(Reply::Worktrees(self.cmd_worktrees().await?)),
            Action::Usage => return Ok(Reply::Usage(self.cmd_usage().await?)),
            Action::Screen(l) => return Ok(Reply::Screen(self.cmd_screen(&l).await?)),
            Action::Reply(t) => return Ok(Reply::Screen(self.cmd_reply(&t.thread).await?)),
            Action::Recall(r) => return Ok(Reply::Recall(self.cmd_recall(&r).await?)),
            Action::JournalList(l) => return Ok(Reply::Journal(self.cmd_journal_list(&l).await?)),
            Action::Snapshot => self.snapshot().await,
            Action::Watch => bail!("watch streams over the control socket"),
            Action::Complete(c) => self.cmd_complete(c).await,
        };
        text.map(Reply::Text)
    }

    async fn cmd_new(&self, http: &Http, n: &New) -> Result<Change> {
        let (machine, repo, r#ref) = match &n.at {
            Some(at) => {
                let (_, cell) = self.find_task(at).await?;
                let t = cell.lock().await;
                let branch = t
                    .branch
                    .clone()
                    .context(format!("{} is on no branch", t.pane.0))?;
                (t.machine.clone(), t.repo.clone(), branch)
            }
            None => (n.machine.clone(), n.repo.clone(), n.branch.clone()),
        };
        let m = self.machine(&machine)?;
        let repo = Name::parse(&repo)?;
        let r#ref = Name::parse(&r#ref)?;
        let kind = harness::find(n.agent.as_deref().unwrap_or(harness::DEFAULT))
            .context("unknown agent, see drover help")?
            .name;
        let model = n.model.as_deref().map(Name::parse).transpose()?;

        let memory = self.journal.context(repo.as_str(), &n.prompt, 2000).await?;
        let mut prompt = if n.scout {
            format!("{SCOUT}\n\n{}", n.prompt)
        } else {
            n.prompt.clone()
        };
        if n.mate.is_some() {
            prompt = format!("{prompt}\n\n{CREW}");
        }
        if !memory.is_empty() {
            prompt = format!(
                "{prompt}\n\nContext from earlier tasks (journal, may be stale):\n{memory}"
            );
        }
        let wt = herdr::worktree(m, &repo, &r#ref, n.at.is_some()).await?;
        if let Some((_, cell)) = self.live_cell(&m.name, &wt.pane_id).await {
            if n.fresh {
                return self.renew(&cell, n, &prompt).await;
            }
            let t = cell.lock().await;
            let at = t
                .thread
                .map_or_else(|| t.pane.0.clone(), |th| format!("<#{th}>"));
            return Ok(Change::task(
                &t,
                Some("already open, prompt not sent"),
                format!("already open in {at}; the prompt was not sent"),
            )
            .help(vec![
                format!("drover send --pane {} --text <text>", t.pane.0),
                format!("drover new --at {} --prompt <prompt>", t.pane.0),
                format!("drover new --fresh {} --prompt <prompt>", place_of(&t)),
            ]));
        }
        if herdr::agents(m)
            .await?
            .iter()
            .any(|a| a.pane_id == wt.pane_id)
        {
            return Err(cli::Failure::new(
                format!(
                    "an agent already runs in {} that drover does not follow",
                    wt.pane_id.0
                ),
                format!("drover attach --machine {} --pane {}", m.name, wt.pane_id.0),
            )
            .into());
        }

        let repo = repo.as_str();
        let (thread, prompt_msg) = if n.discord {
            let th = self
                .open_thread(http, &format!("{repo} · {} · {}", wt.branch, m.name))
                .await?;
            let head = th
                .say(
                    http,
                    format!(
                        "`{}`\n> {}",
                        wt.path,
                        tail(&n.prompt, 1500).replace('\n', "\n> ")
                    ),
                )
                .await?;
            th.create_reaction(http, head.id, emoji("👀")).await?;
            (Some(th), Some(head.id))
        } else {
            (None, None)
        };

        let task = Task {
            thread,
            machine: m.name.clone(),
            pane: wt.pane_id.clone(),
            workspace: wt.workspace_id,
            kind: kind.into(),
            repo: repo.into(),
            branch: Some(wt.branch.clone()),
            origin: match n.at {
                Some(_) => Origin::Detached,
                None => Origin::Opened,
            },
            role: n.mate.clone().map_or(Role::Task, |mate| Role::Crew {
                mate,
                owner: Owner::Mate,
            }),
            phase: Phase::Starting { prompt },
            status: None,
            session: None,
            last_reply: None,
            reply: None,
            prompt_msg,
            dialog_msg: None,
            queued: None,
            inbox: Vec::new(),
        };
        let shown = thread.map_or_else(|| wt.pane_id.0.clone(), |th| format!("<#{th}>"));
        let out = Change::task(&task, None, shown).watch(&wt.pane_id);
        self.insert(task);
        self.save().await;
        if let Some(name) = &n.mate {
            log::append(&self.mate_dir(name), log::Event::Linked, &wt.pane_id.0)?;
            self.write_owners(name, None).await?;
        }
        // a startup dialog shows up through the poller; the prompt goes out once the agent is idle
        herdr::start(
            m,
            &agent_name(&wt.branch),
            kind,
            &wt.pane_id,
            &wt.path,
            model.as_ref(),
        )
        .await?;
        if let Some(label) = &n.name {
            herdr::rename_pane(m, &wt.pane_id, label).await?;
        }
        Ok(out)
    }

    // A fresh conversation for the agent already in this pane: the context goes, the pane, its
    // worktree and its owner stay, and the prompt goes out as a first prompt would.
    async fn renew(&self, cell: &Cell, n: &New, prompt: &str) -> Result<Change> {
        let mut t = cell.lock().await;
        if t.role.crew_of_mate() != n.mate.as_ref() {
            bail!(
                "{} is not driven by {}",
                t.pane.0,
                n.mate.as_ref().map_or("the desk", MateName::as_str)
            );
        }
        let Some(clear) = harness::find(&t.kind).and_then(|h| h.clear) else {
            bail!(
                "{} cannot clear its context in place; close the task and start a new one",
                t.kind
            );
        };
        let m = self.machine(&t.machine)?;
        if let Err(e) = herdr::prompt(m, &t.pane, clear).await {
            return Err(match e.is_busy() || e.is(&herdr::Code::AgentBlocked) {
                true => cli::Failure::new(
                    "the agent is not idle; wait for its reply",
                    format!("drover screen --pane {}", t.pane.0),
                )
                .into(),
                false => e.into(),
            });
        }
        // the clear shows no status change to wait on, and the prompt must not land in the middle of it
        let mut sent = Err(herdr::Error::failed("not sent".into()));
        for _ in 0..20 {
            sleep(Duration::from_millis(500)).await;
            sent = herdr::prompt(m, &t.pane, prompt).await;
            if !sent.as_ref().is_err_and(herdr::Error::is_busy) {
                break;
            }
        }
        sent?;
        // the new conversation has a new transcript, learned from agent.list on the next push;
        // the old last reply stays, so the old transcript is never read as this turn's
        t.session = None;
        t.reply = None;
        t.queued = None;
        t.phase = Phase::Waiting;
        if let Some(label) = &n.name {
            herdr::rename_pane(m, &t.pane, label).await?;
        }
        let out = Change::task(&t, None, format!("fresh agent in {}", t.pane.0))
            .with("delivery", "sent")
            .watch(&t.pane);
        drop(t);
        self.save().await;
        Ok(out)
    }

    async fn cmd_attach(&self, http: &Http, at: &Attach) -> Result<Change> {
        let m = match at.machine.as_deref() {
            Some(name) => self.machine(name)?,
            None => self.machine_of_pane(&at.pane).await.unwrap_or(&self.host),
        };
        let pane = PaneId(at.pane.clone());
        if let Some((_, cell)) = self.live_cell(&m.name, &pane).await {
            let mut t = cell.lock().await;
            if let Some(name) = &at.mate {
                t.role = match &t.role {
                    Role::Task => Role::Crew {
                        mate: name.clone(),
                        owner: Owner::User,
                    },
                    Role::Crew { owner, .. } => Role::Crew {
                        mate: name.clone(),
                        owner: *owner,
                    },
                    Role::Mate(_) | Role::Scout(_) => bail!("a mate or a scout can't be linked"),
                };
            }
            if let Some(th) = t.thread {
                if at.mate.is_none() {
                    return Ok(Change::task(
                        &t,
                        Some("already open"),
                        format!("already open in <#{th}>"),
                    )
                    .watch(&pane));
                }
            } else if at.discord {
                let middle = t.branch.as_ref().unwrap_or(&t.kind);
                let th = self
                    .open_thread(http, &format!("{} · {middle} · {}", t.repo, m.name))
                    .await?;
                t.thread = Some(th);
                th.send_message(http, screen_msg(m, &pane).await).await?;
            } else if at.mate.is_none() {
                return Ok(Change::task(
                    &t,
                    Some("already open"),
                    format!("already open in {}", pane.0),
                )
                .watch(&pane));
            }
            let shown = t
                .thread
                .map_or_else(|| pane.0.clone(), |th| format!("<#{th}>"));
            let out = Change::task(&t, None, shown).watch(&pane);
            drop(t);
            self.save().await;
            if let Some(name) = &at.mate {
                log::append(&self.mate_dir(name), log::Event::Linked, &pane.0)?;
                self.write_owners(name, None).await?;
            }
            return Ok(out);
        }

        let a = herdr::agents(m)
            .await?
            .into_iter()
            .find(|x| x.pane_id == pane)
            .context(format!("no agent in {} on {}", pane.0, m.name))?;

        let repo = a.cwd.rsplit('/').next().unwrap_or_default().to_string();
        let thread = if at.discord {
            Some(
                self.open_thread(http, &format!("{repo} · {} · {}", a.agent, m.name))
                    .await?,
            )
        } else {
            None
        };
        let session = a.session();
        let last_reply = match &session {
            Some(s) => herdr::reply(m, s).await?.reply.map(|r| r.id),
            None => None,
        };
        let task = Task {
            thread,
            machine: m.name.clone(),
            pane: pane.clone(),
            workspace: a.workspace_id,
            kind: a.agent,
            repo,
            branch: None,
            origin: Origin::Attached,
            role: at.mate.clone().map_or(Role::Task, |mate| Role::Crew {
                mate,
                owner: Owner::User,
            }),
            phase: Phase::Idle,
            status: Some(a.agent_status),
            session,
            last_reply,
            reply: None,
            prompt_msg: None,
            dialog_msg: None,
            queued: None,
            inbox: Vec::new(),
        };
        let shown = thread.map_or_else(|| pane.0.clone(), |th| format!("<#{th}>"));
        let out = Change::task(&task, None, shown).watch(&pane);
        self.insert(task);
        self.save().await;
        if let Some(name) = &at.mate {
            log::append(&self.mate_dir(name), log::Event::Linked, &pane.0)?;
            self.write_owners(name, None).await?;
        }
        if let Some(th) = thread {
            th.send_message(http, screen_msg(m, &pane).await).await?;
        }
        Ok(out)
    }

    async fn cmd_agents(&self) -> Agents {
        let mut machines = vec![];
        let mut agents = vec![];
        for m in self.machines() {
            let list = match herdr::agents(m).await {
                Ok(list) => list,
                Err(e) => {
                    machines.push(MachineReach {
                        name: m.name.clone(),
                        reach: Reachability::Unreachable,
                        error: Some(e.to_string()),
                    });
                    continue;
                }
            };
            machines.push(MachineReach {
                name: m.name.clone(),
                reach: Reachability::Reachable,
                error: None,
            });
            for a in list {
                agents.push(AgentRow {
                    machine: m.name.clone(),
                    thread: self.thread_on(&m.name, &a.pane_id).await,
                    pane: a.pane_id.0,
                    harness: a.agent,
                    status: a.agent_status,
                    cwd: home(&a.cwd),
                });
            }
        }
        Agents {
            machines,
            agents,
            help: AGENTS_HELP,
        }
    }

    async fn cmd_complete(&self, what: Candidates) -> Result<String> {
        let names: Vec<String> = match what {
            Candidates::Threads => {
                let mut ids = vec![];
                for (id, cell) in self.cells() {
                    if cell.lock().await.phase != Phase::Gone {
                        ids.push(id);
                    }
                }
                ids
            }
            Candidates::Machines => self.machines().map(|m| m.name.clone()).collect(),
            Candidates::Mates => {
                let mut names = vec![];
                for (_, cell) in self.cells() {
                    let t = cell.lock().await;
                    if let Role::Mate(name) = &t.role
                        && t.phase != Phase::Gone
                    {
                        names.push(name.to_string());
                    }
                }
                names.sort();
                names.dedup();
                names
            }
            Candidates::Panes(machine) => {
                let m = self.machine(&machine)?;
                herdr::agents(m)
                    .await?
                    .into_iter()
                    .map(|a| a.pane_id.0)
                    .collect()
            }
            Candidates::Journal => self
                .journal
                .list(None, 25)
                .await?
                .into_iter()
                .map(|(id, _)| id.to_string())
                .collect(),
        };
        Ok(names.join("\n"))
    }

    // The repo of a task, by pane or thread id.
    async fn repo_of(&self, task: &str) -> Result<String> {
        let (_, cell) = self.find_task(task).await?;
        let repo = cell.lock().await.repo.clone();
        Ok(repo)
    }

    async fn cmd_journal_list(&self, l: &JournalList) -> Result<Entries> {
        let repo = match &l.task {
            Some(task) => Some(self.repo_of(task).await?),
            None => None,
        };
        let total = self.journal.count(repo.clone()).await?;
        let rows = self.journal.list(repo.clone(), ENTRIES).await?;
        let (none, full_cmd) = match (&repo, &l.task) {
            (Some(repo), Some(task)) => (
                format!("0 entries for {repo}"),
                format!("drover journal --pane {task} --full"),
            ),
            _ => ("0 entries".into(), "drover journal --full".into()),
        };
        Ok(Entries::new(rows, total, l.full, none, full_cmd))
    }

    async fn cmd_recall(&self, r: &Recall) -> Result<Entries> {
        let rows = self.journal.search(&r.query, ENTRIES).await?;
        let total = self.journal.matches(&r.query).await?;
        Ok(Entries::new(
            rows,
            total,
            r.full,
            format!("0 entries matching {}", r.query),
            format!("drover recall {} --full", r.query),
        ))
    }

    async fn cmd_screen(&self, l: &Look) -> Result<ScreenView> {
        let (_, cell) = self.find_task(&l.thread).await?;
        let (machine, pane, status) = {
            let t = cell.lock().await;
            (t.machine.clone(), t.pane.clone(), t.status)
        };
        let m = self.machine(&machine)?;
        let raw = match herdr::screen(m, &pane, 60).await {
            Ok(raw) => raw,
            Err(e) if l.discord => format!("read failed: {e}"),
            Err(e) => return Err(e.into()),
        };
        let text = last_lines(&raw);
        let chars = text.chars().count();
        let status = status.unwrap_or(Status::Unknown);
        let mut help = vec![];
        if !l.full && chars > EXCERPT {
            help.push(format!("drover screen --pane {} --full", pane.0));
        }
        help.push(format!("drover keys --pane {} --keys <keys>", pane.0));
        Ok(ScreenView {
            pane: pane.0,
            machine,
            status,
            chars,
            text: match l.full {
                true => text,
                false => tail(&text, EXCERPT),
            },
            help,
        })
    }

    // The agent's last reply in full, from the transcript a turn's end is read from.
    async fn cmd_reply(&self, target: &str) -> Result<ScreenView> {
        let (_, cell) = self.find_task(target).await?;
        let t = cell.lock().await.clone();
        let screen = format!("drover screen --pane {} --full", t.pane.0);
        if !harness::find(&t.kind).is_some_and(|h| h.transcript) {
            return Err(
                cli::Failure::new(format!("{} keeps no transcript", t.kind), screen).into(),
            );
        }
        let m = self.machine(&t.machine)?;
        let reply = match &t.session {
            Some(s) => herdr::reply(m, s).await?.reply,
            None => None,
        };
        let r = reply.ok_or_else(|| cli::Failure::new("no reply yet", screen))?;
        Ok(ScreenView {
            chars: r.text.chars().count(),
            text: r.text,
            help: vec![format!("drover send --pane {} --text <text>", t.pane.0)],
            pane: t.pane.0,
            machine: t.machine,
            status: t.status.unwrap_or(Status::Unknown),
        })
    }

    pub async fn cmd_usage(&self) -> Result<Usage> {
        let jobs: Vec<_> = self
            .machines()
            .cloned()
            .map(|m| tokio::spawn(async move { machine_usage(&m).await }))
            .collect();
        let mut all = Usage {
            windows: vec![],
            spend: vec![],
            unavailable: vec![],
            lines: vec![],
            logins: vec![],
        };
        for job in jobs {
            let u = job.await?;
            all.windows.extend(u.windows);
            all.spend.extend(u.spend);
            all.unavailable.extend(u.unavailable);
            all.lines.extend(u.lines);
            all.logins.extend(u.logins);
        }
        let read: Vec<_> = all
            .logins
            .iter()
            .filter(|l| l.read)
            .map(|l| &l.account)
            .collect();
        for l in all
            .logins
            .iter()
            .filter(|l| !l.read && !read.contains(&&l.account))
        {
            all.lines.push(format!(
                "**{}** *claude* unavailable: token not readable there",
                l.machine
            ));
            all.unavailable.push(Unavailable {
                machine: l.machine.clone(),
                harness: "claude",
            });
        }
        Ok(all)
    }

    // What this service runs with, so a terminal or an agent can see the setup without the .env.
    fn config(&self) -> Config {
        Config {
            host: self.host.name.clone(),
            remotes: self.remotes.iter().map(|m| m.name.clone()).collect(),
            channel: self.channel.to_string(),
            repos: std::env::var("REPOS_ROOT").unwrap_or_else(|_| "~/.drover-workspace".into()),
            state: STATE,
            journal: "journal.db",
            socket: control::path().map_or_else(|e| e.to_string(), |p| p.display().to_string()),
        }
    }

    // A prompt from the terminal: the same path as a Discord message in the thread.
    async fn cmd_send(&self, s: &Send) -> Result<Change> {
        let (_, cell) = self.find_task(&s.thread).await?;
        let (pane, machine) = {
            let t = cell.lock().await;
            (t.pane.clone(), t.machine.clone())
        };
        let (delivery, said) = match self.send_prompt(&cell, &s.text, None).await? {
            Sent::Now => ("sent", "sent"),
            Sent::Queued => ("queued", "queued, sent when the agent is idle"),
            Sent::Blocked => {
                return Err(cli::Failure::new(
                    "the agent waits on a dialog",
                    format!("drover screen --pane {}", pane.0),
                )
                .into());
            }
        };
        Ok(Change::new(said)
            .with("pane", &pane.0)
            .with("machine", machine)
            .with("delivery", delivery)
            .help(vec![format!("drover screen --pane {}", pane.0)]))
    }

    // Keys to the pane, then what its terminal shows.
    async fn cmd_keys(&self, k: &Keys) -> Result<ScreenView> {
        let (m, pane) = self.target(&k.thread).await?;
        let keys: Vec<&str> = k.keys.iter().map(String::as_str).collect();
        herdr::keys(m, &pane, &keys).await?;
        sleep(Duration::from_millis(700)).await;
        self.cmd_screen(&Look {
            thread: k.thread.clone(),
            full: k.discord,
            discord: k.discord,
        })
        .await
    }

    // A prompt from Discord or the terminal. `msg` is the Discord message that carries its reaction.
    pub(crate) async fn send_prompt(
        &self,
        cell: &Cell,
        text: &str,
        msg: Option<MessageId>,
    ) -> Result<Sent> {
        let sent = {
            let mut t = cell.lock().await;
            if t.phase == Phase::Gone {
                bail!("the agent has exited: attach a running one or start a new task");
            }
            let m = self.machine(&t.machine)?;
            match herdr::prompt(m, &t.pane, text).await {
                Ok(()) => {
                    // a pending first prompt or journal entry keeps its phase; its reply covers this one too
                    if t.phase == Phase::Idle {
                        t.phase = Phase::Waiting;
                    }
                    if msg.is_some() {
                        t.prompt_msg = msg;
                    }
                    Sent::Now
                }
                Err(e) if e.is(&herdr::Code::AgentBlocked) => return Ok(Sent::Blocked),
                Err(e) if e.is_busy() => {
                    if t.queued.is_some() {
                        bail!("one message is already queued; send again after it runs");
                    }
                    t.queued = Some(text.into());
                    Sent::Queued
                }
                Err(e) => return Err(e.into()),
            }
        };
        self.save().await;
        Ok(sent)
    }

    // The mate's own task.
    async fn mate_cell(&self, name: &MateName) -> Option<Cell> {
        for (_, cell) in self.cells() {
            let t = cell.lock().await;
            if matches!(&t.role, Role::Mate(n) if n == name) && t.phase != Phase::Gone {
                drop(t);
                return Some(cell);
            }
        }
        None
    }

    // The mate's pane and optional Discord thread.
    pub(crate) async fn mate_task(&self, name: &MateName) -> Option<(PaneId, Option<ChannelId>)> {
        let cell = self.mate_cell(name).await?;
        let t = cell.lock().await;
        Some((t.pane.clone(), t.thread))
    }

    // News from a crewmate: prompted to its mate now, or kept in its inbox until the mate's turn ends.
    pub(crate) async fn tell_mate(&self, name: &MateName, text: &str) -> Result<()> {
        let Some(cell) = self.mate_cell(name).await else {
            return Ok(());
        };
        // the event already says crew
        let head = text.lines().next().unwrap_or_default();
        let head = head.strip_prefix("crew ").unwrap_or(head);
        log::append(&self.mate_dir(name), log::Event::Crew, head)?;
        let mut t = cell.lock().await;
        let m = self.machine(&t.machine)?;
        // the phase stays as it is, so the mate's answer to this is not posted
        match herdr::prompt(m, &t.pane, text).await {
            Ok(()) => {}
            Err(e) if e.is_busy() || e.is(&herdr::Code::AgentBlocked) => t.inbox.push(text.into()),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }

    // A question for the desk from a mate: posted in its thread (if any) and kept in its log.
    async fn cmd_escalate(&self, http: &Http, n: &Note) -> Result<Change> {
        let name = &n.mate;
        let (pane, th) = self
            .mate_task(name)
            .await
            .context(format!("no running mate {name}"))?;
        log::append(&self.mate_dir(name), log::Event::Escalated, &n.text)?;
        let said = match th {
            Some(th) => {
                th.say(http, &n.text).await?;
                format!("escalated to <#{th}>")
            }
            None => format!("escalated for mate {name} ({})", pane.0),
        };
        Ok(Change::new(said)
            .with("mate", name)
            .with("pane", &pane.0)
            .with("machine", &self.host.name)
            .maybe("thread", th)
            .with("event", log::Event::Escalated.as_str()))
    }

    // A note for the mate, in its log, for it to read when it next looks.
    fn cmd_brief(&self, b: &Note) -> Result<Change> {
        let name = &b.mate;
        log::append(&self.mate_dir(name), log::Event::Brief, &b.text)?;
        Ok(Change::new(format!("noted for mate {name}"))
            .with("mate", name)
            .with("event", log::Event::Brief.as_str()))
    }

    // A scout: a short look with no repo, in its own folder and pane, with nothing to journal.
    async fn cmd_scout(&self, http: &Http, s: &Scout) -> Result<Change> {
        let id = scout_id();
        let dir = self.mates.join("state").join("scouts").join(&id);
        std::fs::create_dir_all(&dir)?;
        let cwd = dir.to_string_lossy().into_owned();
        let (workspace, pane) = herdr::tab_pane(&self.host, "scouts", &cwd).await?;
        let (thread, prompt_msg) = if s.discord {
            let th = self
                .open_thread(http, &format!("scout · {id} · {}", self.host.name))
                .await?;
            let head = th.say(http, format!("scout in `{cwd}`")).await?;
            (Some(th), Some(head.id))
        } else {
            (None, None)
        };
        let task = Task {
            thread,
            machine: self.host.name.clone(),
            pane: pane.clone(),
            workspace,
            kind: harness::DEFAULT.into(),
            repo: "scouts".into(),
            branch: None,
            origin: Origin::Attached,
            role: Role::Scout(id.clone()),
            phase: Phase::Starting {
                prompt: format!("{SCOUT}\n\n{}", s.prompt),
            },
            status: None,
            session: None,
            last_reply: None,
            reply: None,
            prompt_msg,
            dialog_msg: None,
            queued: None,
            inbox: Vec::new(),
        };
        let shown = thread.map_or_else(|| pane.0.clone(), |th| format!("<#{th}>"));
        let out = Change::task(&task, None, shown)
            .with("scout", &id)
            .help(vec![
                format!("drover screen --pane {}", pane.0),
                format!("drover scout close --pane {id}"),
            ]);
        self.insert(task);
        self.save().await;
        herdr::start(&self.host, &id, harness::DEFAULT, &pane, &cwd, None).await?;
        Ok(out)
    }

    async fn cmd_scout_close(&self, http: &Http, target: &str) -> Result<Change> {
        let (key, cell) = match self.find_task(target).await {
            Ok(found) => found,
            // a closed scout's task is gone, and its id is all that is left
            Err(_) if target.starts_with("scout-") => {
                return Ok(Change::new(format!("closed {target}"))
                    .with("scout", target)
                    .with("noop", "already closed"));
            }
            Err(e) => return Err(e),
        };
        let t = cell.lock().await.clone();
        let Role::Scout(scout) = t.role.clone() else {
            bail!("this task is not a scout");
        };
        self.remove(&key);
        self.save().await;
        self.teardown(http, &t, Teardown::Close).await?;
        remove_scout_dir(&self.mates.join("state").join("scouts"), &scout)?;
        Ok(Change::at(&t, None, format!("closed {scout}")).with("scout", &scout))
    }

    async fn cmd_mate(&self, http: &Http, m: &Mate) -> Result<Change> {
        let name = m.name.clone();
        let mut live = 0;
        for (_, cell) in self.cells() {
            let t = cell.lock().await;
            if t.phase == Phase::Gone {
                continue;
            }
            match &t.role {
                Role::Mate(n) if *n == name => {
                    let shown = t
                        .thread
                        .map_or_else(|| t.pane.0.clone(), |th| format!("<#{th}>"));
                    return Ok(Change::task(&t, Some("already running"), shown)
                        .help(mate_help(&name, &self.host.name)));
                }
                Role::Mate(_) => live += 1,
                _ => {}
            }
        }
        if live >= MAX_MATES {
            bail!("{MAX_MATES} mates already run, close one first");
        }

        let cwd = self.mates.to_string_lossy().into_owned();
        let (workspace, pane) = herdr::tab_pane(&self.host, "mates", &cwd).await?;
        let (thread, prompt_msg) = if m.discord {
            let th = self
                .open_thread(http, &format!("mate · {name} · {}", self.host.name))
                .await?;
            let head = th.say(http, format!("mate `{name}` in `{cwd}`")).await?;
            (Some(th), Some(head.id))
        } else {
            (None, None)
        };
        let task = Task {
            thread,
            machine: self.host.name.clone(),
            pane: pane.clone(),
            workspace,
            kind: harness::DEFAULT.into(),
            repo: "mates".into(),
            branch: None,
            origin: Origin::Attached,
            role: Role::Mate(name.clone()),
            phase: Phase::Starting {
                prompt: mate_role(&name),
            },
            status: None,
            session: None,
            last_reply: None,
            reply: None,
            prompt_msg,
            dialog_msg: None,
            queued: None,
            inbox: Vec::new(),
        };
        let shown = thread.map_or_else(|| pane.0.clone(), |th| format!("<#{th}>"));
        let out = Change::task(&task, None, shown).help(mate_help(&name, &self.host.name));
        self.insert(task);
        self.save().await;
        log::append(&self.mate_dir(&name), log::Event::Started, &pane.0)?;
        self.write_owners(&name, None).await?;
        herdr::start(
            &self.host,
            &format!("mate-{name}"),
            harness::DEFAULT,
            &pane,
            &cwd,
            None,
        )
        .await?;
        Ok(out)
    }

    // A new agent nobody started through drover: link it to a mate that works on the same repo and branch.
    // Same repo on another branch is asked about in the mate's thread. Anything else is left alone.
    pub(crate) async fn adopt(&self, http: &Http, machine: &str, pane: &PaneId) {
        if let Err(e) = self.adopt_pane(http, machine, pane).await {
            eprintln!("adopt {}: {e}", pane.0);
        }
    }

    async fn adopt_pane(&self, http: &Http, machine: &str, pane: &PaneId) -> Result<()> {
        let m = self.machine(machine)?;
        let Some(agent) = herdr::agents(m)
            .await?
            .into_iter()
            .find(|a| a.pane_id == *pane)
        else {
            return Ok(());
        };
        let Some((repo, branch)) = herdr::checkout(m, &agent.cwd).await? else {
            return Ok(());
        };

        let mut exact = None;
        let mut near = None;
        for (_, cell) in self.cells() {
            let t = cell.lock().await;
            let Role::Crew { mate: name, .. } = &t.role else {
                continue;
            };
            if t.repo != repo {
                continue;
            }
            if t.branch.as_deref() == Some(branch.as_str()) {
                exact = Some(name.clone());
            } else {
                near = Some(name.clone());
            }
        }

        if let Some(name) = exact {
            let at = Attach {
                machine: Some(machine.into()),
                pane: pane.0.clone(),
                mate: Some(name),
                discord: false,
            };
            self.cmd_attach(http, &at).await?;
        } else if let Some(name) = near
            && let Some((_, Some(th))) = self.mate_task(&name).await
        {
            let text = format!(
                "a new agent on {repo} {branch} at {} is not linked to you: `drover link --machine {machine} --pane {} --mate {name}` links it",
                pane.0, pane.0
            );
            th.say(http, text).await?;
        }
        Ok(())
    }

    // The panes linked to a mate and who may drive each one, for the mate to read before it sends input.
    // `skip` is the key of the closing task, whose cell the caller still holds: locking it would wait forever.
    pub(crate) async fn write_owners(&self, name: &MateName, skip: Option<&str>) -> Result<()> {
        let mut lines = Vec::new();
        for (key, cell) in self.cells() {
            if skip == Some(key.as_str()) {
                continue;
            }
            let t = cell.lock().await;
            if let Role::Crew { mate, owner } = &t.role
                && mate == name
                && t.phase != Phase::Gone
            {
                lines.push(format!("{} {}", t.pane.0, owner.as_str()));
            }
        }
        let dir = self.mate_dir(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("owners"), lines.join("\n") + "\n")?;
        Ok(())
    }

    async fn set_owner(&self, target: &str, owner: Owner) -> Result<Change> {
        let (_, cell) = self.find_task(target).await?;
        let (out, name, same) = {
            let mut t = cell.lock().await;
            let pane = t.pane.0.clone();
            let Role::Crew { mate, owner: o } = &mut t.role else {
                bail!("{pane} is not linked to a mate");
            };
            let same = *o == owner;
            *o = owner;
            let mate = mate.clone();
            let noop = match (same, owner) {
                (false, _) => None,
                (true, Owner::User) => Some("already taken"),
                (true, Owner::Mate) => Some("already released"),
            };
            let out = Change::at(&t, noop, format!("{pane} is {}", owner.as_str()))
                .with("mate", &mate)
                .with("owner", owner.as_str());
            (out, mate, same)
        };
        if same {
            return Ok(out);
        }
        log::append(&self.mate_dir(&name), log::Event::Owner, owner.as_str())?;
        self.write_owners(&name, None).await?;
        self.save().await;
        Ok(match owner {
            Owner::User => out.help(vec![format!(
                "drover send --pane {} --text <text>",
                cell.lock().await.pane.0
            )]),
            Owner::Mate => out,
        })
    }

    async fn cmd_worktrees(&self) -> Result<Worktrees> {
        let jobs: Vec<_> = self
            .machines()
            .cloned()
            .map(|m| {
                tokio::spawn(async move {
                    let listed = herdr::worktrees(&m).await;
                    (m.name, listed)
                })
            })
            .collect();
        let mut out = Worktrees {
            worktrees: vec![],
            machines: vec![],
            help: [
                "drover new --machine <machine> --repo <repo> --branch <branch> --prompt <prompt>",
            ],
            discord: String::new(),
        };
        let mut parts = Vec::new();
        for job in jobs {
            let (name, listed) = job.await?;
            let (s, reach, error) = match listed {
                Ok(s) if s.is_empty() => ("none".into(), Reachability::Reachable, None),
                Ok(s) => (s, Reachability::Reachable, None),
                Err(e) => (
                    format!("unreachable: {e}"),
                    Reachability::Unreachable,
                    Some(e.to_string()),
                ),
            };
            if error.is_none() {
                out.worktrees.extend(worktree_rows(&name, &s));
            }
            parts.push(format!("**{name}**\n```\n{s}\n```"));
            out.machines.push(MachineReach { name, reach, error });
        }
        out.discord = parts.join("\n");
        Ok(out)
    }

    async fn cmd_done(&self, http: &Http, d: &Done) -> Result<Change> {
        let (key, cell) = self.find_task(&d.thread).await?;
        let teardown = if d.remove || cell.lock().await.origin == Origin::Detached {
            Teardown::Remove
        } else {
            Teardown::Close
        };
        let (th, base) = {
            let mut t = cell.lock().await;
            if matches!(t.role, Role::Scout(_)) {
                bail!("a scout ends with scout-close");
            }
            // asking again would send a second journal prompt; --no-journal still ends the wait
            let noop = match (&t.phase, d.journal) {
                (Phase::Gone, _) => Some("already gone"),
                (Phase::Ending(_), true) => Some("already ending"),
                _ => None,
            };
            if let Some(why) = noop {
                return Ok(Change::at(&t, noop, why));
            }
            if teardown == Teardown::Remove && t.origin == Origin::Attached {
                bail!("drover didn't open this worktree, remove it by hand");
            }
            if d.journal {
                herdr::prompt(self.machine(&t.machine)?, &t.pane, journal::ENTRY_PROMPT).await?;
                t.phase = Phase::Ending(teardown);
            }
            (t.thread, Change::at(&t, None, ""))
        };
        if !d.journal {
            let t = cell.lock().await.clone();
            self.remove(&key);
            self.save().await;
            let removed = self.teardown(http, &t, teardown).await?;
            let mut out = Change::at(&t, None, "closed without a journal entry")
                .with("entry", "none")
                .with("closed", true)
                .with("teardown", teardown);
            if teardown == Teardown::Remove {
                out = out.with("checkout", if removed { "removed" } else { "kept" });
            }
            return Ok(out);
        }
        if let Some(th) = th {
            let msg = th.say(http, "writing the journal entry").await?;
            cell.lock().await.prompt_msg = Some(msg.id);
            self.save().await;
            th.create_reaction(http, msg.id, emoji("👀")).await?;
        } else {
            self.save().await;
        }
        let mut out = base;
        out.discord = "asked the agent for the journal entry".into();
        Ok(out
            .with("entry", "asked")
            .with("closed", false)
            .with("teardown", teardown)
            .help(vec!["drover journal".into()]))
    }

    async fn cmd_journal_remove(&self, id: i64) -> Result<Change> {
        let removed = self.journal.remove(id).await?;
        Ok(match removed {
            true => Change::new(format!("removed #{id}")).with("id", id),
            false => Change::new(format!("no journal entry #{id}"))
                .with("id", id)
                .with("noop", "already gone"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scout_folders_are_removed_only_when_they_are_scouts() -> Result<()> {
        let root = std::env::temp_dir().join(format!("drover-scouts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scout-abc123"))?;
        assert!(remove_scout_dir(&root, "").is_err());
        assert!(remove_scout_dir(&root, "../scout-abc123").is_err());
        assert!(remove_scout_dir(&root, "other").is_err());
        assert!(root.join("scout-abc123").exists());
        remove_scout_dir(&root, "scout-abc123")?;
        assert!(!root.join("scout-abc123").exists());
        remove_scout_dir(&root, "scout-abc123")?;
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    // The text the old String replies posted, for the same desk.
    #[test]
    fn agents_and_config_post_the_same_discord_text() {
        let row = |machine: &str, pane: &str, status, thread| AgentRow {
            machine: machine.into(),
            pane: pane.into(),
            harness: "claude".into(),
            status,
            cwd: "~/Developer/drover".into(),
            thread,
        };
        let agents = Agents {
            machines: vec![
                MachineReach {
                    name: "host".into(),
                    reach: Reachability::Reachable,
                    error: None,
                },
                MachineReach {
                    name: "m2".into(),
                    reach: Reachability::Unreachable,
                    error: Some("ssh: connect timed out".into()),
                },
                MachineReach {
                    name: "m3".into(),
                    reach: Reachability::Reachable,
                    error: None,
                },
            ],
            agents: vec![
                row("host", "w1:p1", Status::Idle, Some(ChannelId::new(42))),
                row("host", "w2:p1", Status::Blocked, None),
            ],
            help: AGENTS_HELP,
        };
        let agents = Reply::Agents(agents);
        // the terminal sees no thread and no herdr error text
        assert_eq!(
            serde_json::to_value(&agents).map(|v| toon::encode(&v)).ok(),
            Some(
                "agents[2]{pane,machine,status,harness,cwd}:\n  \"w1:p1\",host,idle,claude,~/Developer/drover\n  \
                 \"w2:p1\",host,blocked,claude,~/Developer/drover\nmachines[3]{name,reach}:\n  host,reachable\n  \
                 m2,unreachable\n  m3,reachable\nhelp[3]:\n  - drover screen --pane <pane>\n  - \
                 drover send --pane <pane> --text <text>\n  - drover attach --machine <machine> --pane <pane>\n"
                    .into()
            )
        );
        assert_eq!(
            agents.discord(),
            "**host**\n`○ w1:p1` claude ~/Developer/drover <#42>\n`◆ w2:p1` claude ~/Developer/drover\n\
             **m2**\nunreachable: ssh: connect timed out\n**m3**"
        );

        let config = |remotes: Vec<String>| Config {
            host: "host".into(),
            remotes,
            channel: "7".into(),
            repos: "~/.drover-workspace".into(),
            state: STATE,
            journal: "journal.db",
            socket: "/home/u/.local/state/drover/drover.sock".into(),
        };
        let text = |remotes: &str| {
            format!(
                "host: host\nremotes: {remotes}\nchannel: <#7>\nrepos: ~/.drover-workspace\n\
                 state: state.json\njournal: journal.db\nsocket: /home/u/.local/state/drover/drover.sock"
            )
        };
        assert_eq!(Reply::Config(config(vec![])).discord(), text("none"));
        assert_eq!(
            Reply::Config(config(vec!["m2".into(), "m3".into()])).discord(),
            text("m2, m3")
        );
        assert_eq!(Reply::Text("sent".into()).discord(), "sent");
    }

    fn toon_of(r: &Reply) -> String {
        serde_json::to_value(r)
            .map(|v| toon::encode(&v))
            .unwrap_or_default()
    }

    fn journal_entry(text: &str) -> journal::Entry {
        journal::Entry {
            day: "2026-10-18".into(),
            machine: "host".into(),
            repo: "drover".into(),
            branch: "theo/x".into(),
            text: text.into(),
        }
    }

    // The text the old String replies posted, for the same data.
    #[test]
    fn read_commands_post_the_same_discord_text() {
        let listing = "drover\n  ● theo/axi-read   3          ↓0 ↑2\n  · old   clean   no upstream\n  ○ gone   missing   prune\n";
        let rows = worktree_rows("host", listing);
        assert_eq!(rows.len(), 3);
        assert!(matches!(
            (rows.first(), rows.get(2)),
            (
                Some(WorktreeRow {
                    status: Checkout::Working,
                    changes: Some(3),
                    ..
                }),
                Some(WorktreeRow {
                    status: Checkout::Open,
                    changes: None,
                    ..
                })
            )
        ));
        let w = Reply::Worktrees(Worktrees {
            worktrees: rows,
            machines: vec![MachineReach {
                name: "host".into(),
                reach: Reachability::Reachable,
                error: None,
            }],
            help: [
                "drover new --machine <machine> --repo <repo> --branch <branch> --prompt <prompt>",
            ],
            discord: format!("**host**\n```\n{}\n```", listing.trim_end()),
        });
        assert_eq!(
            toon_of(&w),
            "worktrees[3]{machine,repo,branch,status,changes}:\n  host,drover,theo/axi-read,working,3\n  \
             host,drover,old,closed,0\n  host,drover,gone,open,null\nmachines[1]{name,reach}:\n  host,reachable\n\
             help[1]:\n  - drover new --machine <machine> --repo <repo> --branch <branch> --prompt <prompt>\n"
        );
        assert!(
            w.discord()
                .starts_with("**host**\n```\ndrover\n  ● theo/axi-read")
        );

        let u = Reply::Usage(Usage {
            windows: vec![Window {
                machine: "host".into(),
                harness: "claude",
                window: "5h session",
                percent: 37.0,
                resets_at: Some(1_781_600_000),
            }],
            spend: vec![],
            unavailable: vec![Unavailable {
                machine: "host".into(),
                harness: "cursor",
            }],
            lines: vec![
                "**host**".into(),
                "*claude* plan limits".into(),
                "*cursor* unavailable: x".into(),
            ],
            logins: vec![],
        });
        assert_eq!(
            toon_of(&u),
            "windows[1]{machine,harness,window,percent,resets_at}:\n  host,claude,5h session,37,1781600000\n\
             unavailable[1]{machine,harness}:\n  host,cursor\n"
        );
        assert_eq!(
            u.discord(),
            "**host**\n*claude* plan limits\n*cursor* unavailable: x"
        );

        let long = "x".repeat(2500);
        let screen = |text: &str, chars| {
            Reply::Screen(ScreenView {
                pane: "w1:p1".into(),
                machine: "host".into(),
                status: Status::Blocked,
                chars,
                text: text.into(),
                help: vec!["drover keys --pane w1:p1 --keys <keys>".into()],
            })
        };
        let s = screen("a\nb", 3);
        assert_eq!(
            toon_of(&s),
            "pane: \"w1:p1\"\nmachine: host\nstatus: blocked\nchars: 3\ntext: \"a\\nb\"\nhelp[1]:\n  - \"drover keys --pane w1:p1 --keys <keys>\"\n"
        );
        assert_eq!(s.discord(), "```\na\nb\n```");
        assert_eq!(last_lines("a\n\n  \nb\n"), "a\nb");

        let rows = || vec![(7, journal_entry("what: x")), (9, journal_entry(&long))];
        let list = Entries::new(
            rows(),
            42,
            false,
            "0 entries".into(),
            "drover journal --full".into(),
        );
        let t = toon_of(&Reply::Journal(list));
        assert!(t.starts_with("count: 2 of 42\nentries[2]{id,repo,branch,day,chars,text}:\n  7,drover,theo/x,2026-10-18,7,\"what: x\"\n"));
        assert!(
            t.contains(",2500,xxx")
                && t.contains(
                    "…\nhelp[2]:\n  - drover journal --full\n  - drover journal rm <id>\n"
                )
        );
        assert!(!t.contains(&"x".repeat(1002)));
        // Discord asks for everything and gets it
        let full = Reply::Journal(Entries::new(
            rows(),
            2,
            true,
            "0 entries".into(),
            String::new(),
        ));
        assert_eq!(
            full.discord(),
            format!("#7 2026-10-18 drover theo/x\nwhat: x\n\n#9 2026-10-18 drover theo/x\n{long}")
        );
        assert!(!toon_of(&full).contains("count:"));

        let none = |text: &str| Entries::new(vec![], 0, false, text.into(), String::new());
        assert_eq!(
            toon_of(&Reply::Journal(none("0 entries for drover"))),
            "journal: 0 entries for drover\n"
        );
        assert_eq!(Reply::Journal(none("")).discord(), "no entries");
        assert_eq!(Reply::Recall(none("")).discord(), "nothing");
        let hit = Reply::Recall(Entries::new(
            vec![(7, journal_entry("what: x"))],
            1,
            false,
            String::new(),
            String::new(),
        ));
        assert_eq!(hit.discord(), "2026-10-18 drover theo/x\nwhat: x");
    }

    // The text the old String replies posted, and the fields the terminal gets instead.
    #[test]
    fn write_commands_post_the_same_discord_text() -> Result<()> {
        let task = |role| Task {
            thread: None,
            machine: "host".into(),
            pane: PaneId("w1:p1".into()),
            workspace: WorkspaceId("w1".into()),
            kind: "claude".into(),
            repo: "drover".into(),
            branch: Some("theo/x".into()),
            origin: Origin::Opened,
            role,
            phase: Phase::Idle,
            status: None,
            session: None,
            last_reply: None,
            reply: None,
            prompt_msg: None,
            dialog_msg: None,
            queued: None,
            inbox: Vec::new(),
        };
        let crew = task(Role::Crew {
            mate: MateName::parse("docs")?,
            owner: Owner::Mate,
        });
        let opened = Reply::Change(Change::task(&crew, None, "w1:p1").watch(&crew.pane));
        assert_eq!(opened.discord(), "w1:p1");
        assert_eq!(
            toon_of(&opened),
            "pane: \"w1:p1\"\nmachine: host\nworkspace: w1\nrepo: drover\nbranch: theo/x\nharness: claude\nmate: docs\n\
             help[2]:\n  - \"drover screen --pane w1:p1\"\n  - \"drover send --pane w1:p1 --text <text>\"\n"
        );

        let noop = Reply::Change(
            Change::at(&crew, Some("already taken"), "w1:p1 is user").with("owner", "user"),
        );
        assert_eq!(noop.discord(), "w1:p1 is user");
        assert_eq!(
            toon_of(&noop),
            "pane: \"w1:p1\"\nmachine: host\nnoop: already taken\nowner: user\n"
        );

        let sent = Reply::Change(
            Change::new("queued, sent when the agent is idle")
                .with("pane", "w1:p1")
                .with("delivery", "queued"),
        );
        assert_eq!(sent.discord(), "queued, sent when the agent is idle");
        assert_eq!(toon_of(&sent), "pane: \"w1:p1\"\ndelivery: queued\n");

        let closing = Reply::Change(
            Change::at(&crew, None, "asked the agent for the journal entry")
                .with("entry", "asked")
                .with("closed", false)
                .with("teardown", Teardown::Remove),
        );
        assert_eq!(closing.discord(), "asked the agent for the journal entry");
        assert_eq!(
            toon_of(&closing),
            "pane: \"w1:p1\"\nmachine: host\nentry: asked\nclosed: false\nteardown: remove\n"
        );

        let gone = Reply::Change(
            Change::new("no journal entry #7")
                .with("id", 7)
                .with("noop", "already gone"),
        );
        assert_eq!(gone.discord(), "no journal entry #7");
        assert_eq!(toon_of(&gone), "id: 7\nnoop: already gone\n");

        // a thread id is text, so a program reads it back as it is
        let mut threaded = crew.clone();
        threaded.thread = Some(ChannelId::new(42));
        assert!(
            toon_of(&Reply::Change(Change::task(&threaded, None, "")))
                .ends_with("mate: docs\nthread: \"42\"\n")
        );
        Ok(())
    }

    #[test]
    fn mate_names_are_short_and_lowercase() {
        assert!(MateName::parse("reviewer-2").is_ok());
        assert!(MateName::parse("Reviewer").is_err());
        assert!(MateName::parse("2fast").is_err());
        assert!(MateName::parse(&"a".repeat(27)).is_err());
    }
}
