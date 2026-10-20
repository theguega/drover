mod format;
mod harness;
mod herdr;
mod journal;
mod log;
mod socket;
mod toon;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    hash::{BuildHasher, Hasher, RandomState},
    num::NonZeroU64,
    path::PathBuf,
    sync::{
        Arc, Mutex as StdMutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serenity::all::{
    AutoArchiveDuration, ButtonStyle, Channel, ChannelId, ChannelType, Client, CommandInteraction,
    CommandOptionType, ComponentInteraction, Context, CreateActionRow, CreateAttachment,
    CreateAutocompleteResponse, CreateButton, CreateCommand, CreateCommandOption,
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateThread,
    EditInteractionResponse, EditThread, EventHandler, GatewayIntents, GuildId, Http, Interaction,
    Message, MessageId, ReactionType, Ready, UserId, async_trait,
};
use tokio::{
    sync::{Mutex, Notify},
    time::sleep,
};

use action::{
    Action, Attach, Done, JournalList, JournalRemove, Keys, Login, Look, Mate, MateName, New,
    Recall, Scout, Sent, Spend, Target, Unavailable, Usage, Window,
};
use herdr::{Agent, Machine, Name, PaneId, Reach, SessionId, Status, WorkspaceId};
use journal::Journal;

// ── errors ─────────────────────────────────────────────────

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T, E = Error> = std::result::Result<T, E>;

macro_rules! err {
    ($($t:tt)*) => { Error::from(format!($($t)*)) };
}

macro_rules! bail {
    ($($t:tt)*) => { return Err(err!($($t)*)) };
}

trait WithContext<T> {
    fn context(self, what: impl std::fmt::Display) -> Result<T>;
}

impl<T, E: std::fmt::Display> WithContext<T> for Result<T, E> {
    fn context(self, what: impl std::fmt::Display) -> Result<T> {
        self.map_err(|e| err!("{what}: {e}"))
    }
}

impl<T> WithContext<T> for Option<T> {
    fn context(self, what: impl std::fmt::Display) -> Result<T> {
        self.ok_or_else(|| err!("{what}"))
    }
}

mod action;
mod cli;
mod control;
mod snapshot;

// ── config ─────────────────────────────────────────────────

fn env(k: &str, d: Option<&str>) -> Result<String> {
    std::env::var(k)
        .ok()
        .or(d.map(String::from))
        .context(format!("missing env {k}"))
}

fn id<T: From<NonZeroU64>>(s: &str) -> Result<T> {
    Ok(s.trim()
        .parse::<NonZeroU64>()
        .context(format!("{s:?} is not a discord id"))?
        .into())
}

const STATE: &str = "state.json";
const SCOUT: &str = "Scout task: investigate and report back. Do not edit files, commit, or push.";
// The end of each turn goes to the mate, so a crewmate asks there instead of guessing.
const CREW: &str = "You are a crewmate. Your mate reads the end of each of your turns. The brief above is your only source of facts: do not assume what it does not say. When blocked or unsure, stop and end your turn with a line starting QUESTION:. When the check in the brief passes, end your turn with DONE: and its output. Do not change the check. Do not end your turn while a check or background job you started still runs: wait for it, or end with a line starting RUNNING: and what runs.";

// ── state: one thread per herdr pane ───────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Origin {
    Opened,   // worktree opened by drover, so /done may remove it
    Attached, // started at the desk, drover only follows it
    Detached, // a checkout at another task's commit, opened by drover for a second look; /done removes it
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Teardown {
    Close,  // close the herdr workspace, keep the checkout
    Remove, // also delete the checkout
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Starting { prompt: String }, // first prompt, sent once the agent is ready
    Restarting { prompt: String }, // the old agent exits; pushes wait until its replacement starts
    Idle,
    Waiting,          // a prompt from Discord is waiting for its reply
    Ending(Teardown), // /done sent, next reply is the journal entry
    Gone,             // the agent exited
}

impl Phase {
    #[must_use]
    fn waiting(&self) -> bool {
        matches!(self, Self::Waiting | Self::Ending(_))
    }
}

// Who may send input to a pane: the desk user, or the mate that dispatched it.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Owner {
    User,
    Mate,
}

impl Owner {
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Mate => "mate",
        }
    }
}

// What a pane is to drover. Only a crewmate has an owner: the desk takes it from its mate or gives it back.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Role {
    Task,                                  // a task of the desk's own
    Crew { mate: MateName, owner: Owner }, // linked to a mate, in its own worktree
    Mate(MateName),                        // a mate's pane, in the drover workspace
    Scout(String),                         // the scout's folder name, while the scout lives
}

impl Role {
    // The mate whose log and owner list this pane belongs in.
    #[must_use]
    fn mate(&self) -> Option<&MateName> {
        match self {
            Self::Crew { mate, .. } | Self::Mate(mate) => Some(mate),
            Self::Task | Self::Scout(_) => None,
        }
    }

    // A crewmate its mate drives.
    #[must_use]
    fn crew_of_mate(&self) -> Option<&MateName> {
        match self {
            Self::Crew {
                mate,
                owner: Owner::Mate,
            } => Some(mate),
            _ => None,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "Stored")]
struct Task {
    #[serde(skip_serializing_if = "Option::is_none")]
    thread: Option<ChannelId>,
    machine: String,
    pane: PaneId,
    workspace: WorkspaceId,
    kind: String,
    repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>, // none for an agent drover attached to, a mate, or a scout
    origin: Origin,
    role: Role,
    phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<Status>, // none until the poller has seen the agent
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_reply: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reply: Option<Excerpt>, // the end of the last turn, for the snapshot
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_msg: Option<MessageId>, // message that carries the state reaction
    #[serde(skip_serializing_if = "Option::is_none")]
    dialog_msg: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queued: Option<String>, // one Discord prompt waiting while the agent works
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    inbox: Vec<String>, // crewmates' news waiting for their mate's turn to end
}

// The start of a reply and how long it was, in characters.
#[derive(Clone, Serialize, Deserialize)]
struct Excerpt {
    text: String,
    chars: usize,
}

// Characters of a reply the snapshot keeps.
const REPLY_EXCERPT: usize = 300;
// Between reads of the usage endpoints for the snapshot.
const USAGE_REFRESH: Duration = Duration::from_secs(300);

impl Excerpt {
    fn of(reply: &str) -> Self {
        let reply = reply.trim();
        Self {
            text: head(reply, REPLY_EXCERPT),
            chars: reply.chars().count(),
        }
    }
}

// What state.json may hold: this format, or the flags of the TypeScript drover.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    thread: Option<ChannelId>,
    machine: String,
    pane: PaneId,
    workspace: WorkspaceId,
    kind: String,
    repo: String,
    #[serde(default)]
    branch: Option<String>,
    origin: Option<Origin>,
    role: Option<Role>,
    owner: Option<Owner>,
    mate: Option<MateName>,
    scout: Option<String>,
    phase: Option<Phase>,
    status: Option<Status>,
    session: Option<String>,
    last_reply: Option<String>,
    #[serde(default)]
    reply: Option<Excerpt>,
    prompt_msg: Option<MessageId>,
    dialog_msg: Option<MessageId>,
    queued: Option<String>,
    #[serde(default)]
    inbox: Vec<String>,
    #[serde(default)]
    owned: bool,
    #[serde(default)]
    expecting: bool,
    pending: Option<String>,
    ending: Option<LegacyEnding>,
    #[serde(default)]
    gone: bool,
}

#[derive(Deserialize)]
struct LegacyEnding {
    remove: bool,
}

impl From<Stored> for Task {
    fn from(s: Stored) -> Self {
        let legacy = || match (s.gone, &s.ending, &s.pending) {
            (true, ..) => Phase::Gone,
            (_, Some(e), _) => Phase::Ending(if e.remove {
                Teardown::Remove
            } else {
                Teardown::Close
            }),
            (_, _, Some(p)) => Phase::Starting { prompt: p.clone() },
            _ if s.expecting => Phase::Waiting,
            _ => Phase::Idle,
        };
        let phase = s.phase.clone().unwrap_or_else(legacy);
        let origin = s.origin.unwrap_or(if s.owned {
            Origin::Opened
        } else {
            Origin::Attached
        });
        // before roles, the mate's own pane was the one in the mates folder
        let role = s.role.unwrap_or_else(|| match (s.scout, s.mate) {
            (Some(id), _) => Role::Scout(id),
            (None, Some(mate)) if s.repo == "mates" => Role::Mate(mate),
            (None, Some(mate)) => Role::Crew {
                mate,
                owner: s.owner.unwrap_or(Owner::User),
            },
            (None, None) => Role::Task,
        });
        Self {
            thread: s.thread,
            session: s.session.as_deref().and_then(SessionId::parse),
            machine: s.machine,
            pane: s.pane,
            workspace: s.workspace,
            kind: s.kind,
            repo: s.repo,
            branch: s.branch.filter(|b| !b.is_empty()),
            origin,
            role,
            phase,
            status: s.status,
            last_reply: s.last_reply,
            reply: s.reply,
            prompt_msg: s.prompt_msg,
            dialog_msg: s.dialog_msg,
            queued: s.queued,
            inbox: s.inbox,
        }
    }
}

#[must_use]
fn task_key(machine: &str, pane: &PaneId) -> String {
    format!("{machine}:{}", pane.0)
}

// Loads either pane-keyed tasks or legacy thread-keyed tasks where the map key was the ChannelId.
fn parse_state(s: &str) -> Result<BTreeMap<String, Task>> {
    let raw: BTreeMap<String, Task> = serde_json::from_str(s)?;
    let mut out: BTreeMap<String, Task> = BTreeMap::new();
    for (k, mut t) in raw {
        if t.thread.is_none() {
            t.thread = id(&k).ok();
        }
        let key = task_key(&t.machine, &t.pane);
        if out.get(&key).is_some_and(|prev| prev.phase != Phase::Gone) && t.phase == Phase::Gone {
            continue;
        }
        out.insert(key, t);
    }
    Ok(out)
}

// Each task has its own lock so the poller and a command on the same pane
// take turns, while other tasks go on.
type Cell = Arc<Mutex<Task>>;
type Tasks = HashMap<String, Cell>;

/// Whether a machine's event watcher could subscribe, as the snapshot shows it.
#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Reachability {
    Reachable,
    Unreachable,
}

struct App {
    token: String,
    guild: GuildId,
    channel: ChannelId,
    allowed: HashSet<UserId>,
    host: Machine,
    remotes: Vec<Machine>,
    tasks: StdMutex<Tasks>,
    journal: Journal,
    /// Where mates and their logs live: the mates folder of this checkout.
    mates: PathBuf,
    started: AtomicBool,
    /// Watchers resubscribe when the set of panes for a machine changes.
    wake: Notify,
    /// Every save notifies this, so `drover watch` streams a new snapshot.
    changed: Notify,
    /// Per machine, from its watcher. A machine never tried is absent.
    reachability: StdMutex<HashMap<String, Reachability>>,
    /// The last `/usage` read of every machine, refreshed in the background so a snapshot never waits on it.
    usage: StdMutex<Option<Usage>>,
    /// Saves share one temp file, so they take turns.
    saving: Mutex<()>,
}

impl App {
    fn load() -> Result<Self> {
        let allowed = env("ALLOWED_USER_IDS", None)?
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(id)
            .collect::<Result<HashSet<_>>>()?;
        // an empty list would silently ignore every message
        if allowed.is_empty() {
            bail!("ALLOWED_USER_IDS is empty: list the Discord user IDs allowed to drive agents");
        }
        let remotes = env("REMOTES", Some(""))?
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|name| Machine::new(name.into(), Reach::Ssh))
            .collect();

        let tasks = match std::fs::read_to_string(STATE) {
            Ok(s) => parse_state(&s).context(format!("{STATE} is not valid, fix or remove it"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e).context(STATE),
        };

        Ok(Self {
            token: env("DISCORD_TOKEN", None)?,
            guild: id(&env("DISCORD_GUILD_ID", None)?)?,
            channel: id(&env("DISCORD_CHANNEL_ID", None)?)?,
            allowed,
            host: Machine::new(env("HOST_NAME", Some("host"))?, Reach::Local),
            remotes,
            tasks: StdMutex::new(
                tasks
                    .into_iter()
                    .map(|(k, t)| (k, Arc::new(Mutex::new(t))))
                    .collect(),
            ),
            journal: Journal::open("journal.db")?,
            mates: std::env::current_dir()?.join("mates"),
            started: AtomicBool::new(false),
            wake: Notify::new(),
            changed: Notify::new(),
            reachability: StdMutex::new(HashMap::new()),
            usage: StdMutex::new(None),
            saving: Mutex::new(()),
        })
    }

    fn set_reachability(&self, machine: &str, now: Reachability) {
        let before = self
            .reachability
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(machine.into(), now);
        if before != Some(now) {
            self.changed.notify_waiters();
        }
    }

    fn machines(&self) -> impl Iterator<Item = &Machine> {
        std::iter::once(&self.host).chain(&self.remotes)
    }

    fn machine(&self, name: &str) -> Result<&Machine> {
        self.machines().find(|m| m.name == name).ok_or_else(|| {
            cli::Failure::new(format!("unknown machine {name}"), "drover config").into()
        })
    }

    // A mate's log, owner list and goal.
    fn mate_dir(&self, name: &MateName) -> PathBuf {
        self.mates.join("state").join(name.as_str())
    }

    // A poisoned lock only means a handler panicked; the map itself is fine.
    fn map(&self) -> MutexGuard<'_, Tasks> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cells(&self) -> Vec<(String, Cell)> {
        self.map()
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect()
    }

    fn insert(&self, t: Task) {
        let key = task_key(&t.machine, &t.pane);
        self.map().insert(key, Arc::new(Mutex::new(t)));
        self.wake.notify_waiters();
    }

    fn remove(&self, key: &str) {
        self.map().remove(key);
        self.wake.notify_waiters();
    }

    async fn task_in(&self, channel: ChannelId) -> Option<Cell> {
        for (_, cell) in self.cells() {
            if cell.lock().await.thread == Some(channel) {
                return Some(cell);
            }
        }
        None
    }

    async fn find_task(&self, target: &str) -> Result<(String, Cell)> {
        let raw = target.trim();
        let stripped = raw
            .strip_prefix("<#")
            .and_then(|s| s.strip_suffix('>'))
            .unwrap_or(raw);
        let thread = id::<ChannelId>(stripped).ok();

        if let Some(cell) = self.map().get(raw).cloned() {
            return Ok((raw.into(), cell));
        }

        let mut found = None;
        for (k, cell) in self.cells() {
            let (matches, live) = {
                let t = cell.lock().await;
                let matches = (thread.is_some() && t.thread == thread)
                    || t.pane.0 == raw
                    || matches!(&t.role, Role::Scout(s) if s == raw);
                (matches, t.phase != Phase::Gone)
            };
            if matches {
                if live {
                    return Ok((k, cell));
                }
                found = Some((k, cell));
            }
        }
        found.ok_or_else(|| cli::Failure::new(format!("no task {raw}"), "drover agents").into())
    }

    async fn panes_on(&self, machine: &str) -> Vec<PaneId> {
        let mut panes = Vec::new();
        for (_, cell) in self.cells() {
            let t = cell.lock().await;
            if t.machine == machine && t.phase != Phase::Gone {
                panes.push(t.pane.clone());
            }
        }
        panes.sort_by(|a, b| a.0.cmp(&b.0));
        panes.dedup();
        panes
    }

    // Never call while holding a task lock.
    async fn save(&self) {
        let _turn = self.saving.lock().await;
        let mut all = BTreeMap::new();
        for (k, cell) in self.cells() {
            all.insert(k, cell.lock().await.clone());
        }
        // rename is atomic, so a crash mid-write keeps the previous state file
        let tmp = format!("{STATE}.tmp");
        let written = match serde_json::to_string_pretty(&all) {
            Ok(json) => match tokio::fs::write(&tmp, json).await {
                Ok(()) => tokio::fs::rename(&tmp, STATE).await.map_err(Error::from),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e.into()),
        };
        if let Err(e) = written {
            eprintln!("save {STATE}: {e}");
        }
        self.changed.notify_waiters();
    }

    async fn live_cell(&self, machine: &str, pane: &PaneId) -> Option<(String, Cell)> {
        let key = task_key(machine, pane);
        let cell = self.map().get(&key).cloned()?;
        let live = cell.lock().await.phase != Phase::Gone;
        live.then_some((key, cell))
    }

    async fn thread_on(&self, machine: &str, pane: &PaneId) -> Option<ChannelId> {
        let (_, cell) = self.live_cell(machine, pane).await?;
        cell.lock().await.thread
    }

    async fn machine_of_pane(&self, pane: &str) -> Option<&Machine> {
        for (_, cell) in self.cells() {
            let t = cell.lock().await;
            if t.pane.0 == pane && t.phase != Phase::Gone {
                return self.machine(&t.machine).ok();
            }
        }
        None
    }
}

// ── discord ui ─────────────────────────────────────────────

#[must_use]
fn tail(s: &str, n: usize) -> String {
    let len = s.chars().count();
    if len > n {
        format!("…{}", s.chars().skip(len - n).collect::<String>())
    } else {
        s.into()
    }
}

#[must_use]
fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[must_use]
fn emoji(s: &str) -> ReactionType {
    ReactionType::Unicode(s.into())
}

#[must_use]
fn not_found(e: &serenity::Error) -> bool {
    matches!(e, serenity::Error::Http(h) if h.status_code().map(|s| s.as_u16()) == Some(404))
}

async fn post(http: &Http, th: ChannelId, text: &str) -> Result<()> {
    let parts = format::chunks(&format::discord(text), 1900);
    if text.chars().count() > 6000 {
        let preview = format::chunks(parts.first().map_or("", String::as_str), 1500);
        let msg = CreateMessage::new()
            .content(format!("{}\n…", preview.first().map_or("", String::as_str)))
            .add_file(CreateAttachment::bytes(
                text.as_bytes().to_vec(),
                "reply.md",
            ));
        th.send_message(http, msg).await?;
        return Ok(());
    }
    for part in parts {
        th.say(http, part).await?;
    }
    Ok(())
}

const KEYS: [&[&str]; 2] = [
    &["1", "2", "3", "up", "down"],
    &["enter", "esc", "tab", "ctrl+c", "refresh"],
];

#[must_use]
fn key_rows() -> Vec<CreateActionRow> {
    let button = |k: &str| {
        let style = if k == "enter" {
            ButtonStyle::Primary
        } else {
            ButtonStyle::Secondary
        };
        CreateButton::new(format!("key:{k}")).label(k).style(style)
    };
    KEYS.iter()
        .map(|row| CreateActionRow::Buttons(row.iter().map(|k| button(k)).collect()))
        .collect()
}

// The last 30 lines with text.
fn last_lines(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .skip(lines.len().saturating_sub(30))
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

fn fenced(text: &str, note: &str) -> String {
    format!(
        "{note}```\n{}\n```",
        tail(&text.replace("```", "ˋˋˋ"), 1800)
    )
}

async fn screen(m: &Machine, pane: &PaneId, note: &str) -> String {
    let raw = herdr::screen(m, pane, 60)
        .await
        .unwrap_or_else(|e| format!("read failed: {e}"));
    fenced(&last_lines(&raw), note)
}

async fn screen_msg(m: &Machine, pane: &PaneId) -> CreateMessage {
    CreateMessage::new()
        .content(screen(m, pane, "").await)
        .components(key_rows())
}

// State lives in one reaction on the latest prompt: 👀 working, ⏸ needs you, ✅ done.
async fn mark(http: &Http, th: ChannelId, t: &Task, e: &str) {
    let Some(id) = t.prompt_msg else { return };
    let Ok(msg) = th.message(http, id).await else {
        return;
    };
    let want = emoji(e);
    for r in msg
        .reactions
        .iter()
        .filter(|r| r.me && r.reaction_type != want)
    {
        let _ = th
            .delete_reaction(http, id, None, r.reaction_type.clone())
            .await;
    }
    let _ = th.create_reaction(http, id, want).await;
}

// ── commands ───────────────────────────────────────────────

fn string(name: &str, description: &str) -> CreateCommandOption {
    CreateCommandOption::new(CommandOptionType::String, name, description)
}

#[must_use]
fn opt<'a>(i: &'a CommandInteraction, name: &str) -> Option<&'a str> {
    i.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| o.value.as_str())
}

fn flag(i: &CommandInteraction, name: &str) -> Option<bool> {
    i.data
        .options
        .iter()
        .find(|o| o.name == name)
        .and_then(|o| o.value.as_bool())
}

fn req<'a>(i: &'a CommandInteraction, name: &str) -> Result<&'a str> {
    opt(i, name).context(format!("missing {name}"))
}

#[must_use]
fn ephemeral(content: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(content)
            .ephemeral(true),
    )
}

fn edit(content: impl Into<String>) -> EditInteractionResponse {
    EditInteractionResponse::new().content(content)
}

#[must_use]
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut bad = false;
    for c in s.to_lowercase().chars() {
        let keep = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-';
        if keep || !bad {
            out.push(if keep { c } else { '-' });
        }
        bad = !keep;
    }
    out.trim_matches('-').into()
}

#[must_use]
fn agent_name(s: &str) -> String {
    let mut n = RandomState::new().build_hasher().finish();
    let suffix: String = (0..4)
        .map(|_| {
            let d = (n % 36) as u32;
            n /= 36;
            // INVARIANT: d < 36, the radix.
            #[expect(clippy::expect_used, reason = "d is below the radix")]
            char::from_digit(d, 36).expect("digit below radix")
        })
        .collect();
    format!("d-{}-{suffix}", head(&slug(s), 24))
}

// UTC yyyy-mm-dd, from days since the epoch (Howard Hinnant's civil_from_days).
#[must_use]
fn today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let z = i64::try_from(secs / 86_400).unwrap_or_default() + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

// Seconds since the epoch for a UTC "yyyy-mm-ddThh:mm:ss…" (Howard Hinnant's days_from_civil).
#[must_use]
fn unix(ts: &str) -> Option<i64> {
    let n = |a: usize, b: usize| ts.get(a..b)?.parse::<i64>().ok();
    let (y, m, d) = (n(0, 4)? - i64::from(n(5, 7)? <= 2), n(5, 7)?, n(8, 10)?);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + n(11, 13)? * 3600 + n(14, 16)? * 60 + n(17, 19)?)
}

#[must_use]
fn bar(name: &str, pct: f64, reset: Option<i64>) -> String {
    let filled = ((pct / 10.0).round().clamp(0.0, 10.0)) as usize;
    let reset = reset
        .map(|t| format!(" resets <t:{t}:R>"))
        .unwrap_or_default();
    format!(
        "`{name:<10} {}{} {pct:>3.0}%`{reset}",
        "█".repeat(filled),
        "░".repeat(10 - filled)
    )
}

// One machine's plans, as rows and as the lines Discord posts; an agent that isn't logged in there is left out.
async fn machine_usage(m: &Machine) -> Usage {
    let mut out = Usage {
        windows: vec![],
        spend: vec![],
        unavailable: vec![],
        lines: vec![format!("**{}**", m.name)],
        logins: vec![],
    };
    let machine = &m.name;
    let window = |out: &mut Usage, harness, name, pct: f64, reset: Option<i64>| {
        out.lines.push(bar(name, pct, reset));
        out.windows.push(Window {
            machine: machine.clone(),
            harness,
            window: name,
            percent: pct.round(),
            resets_at: reset,
        });
    };
    let spend = |out: &mut Usage, harness, pool, used: f64, limit: f64| {
        out.spend.push(Spend {
            machine: machine.clone(),
            harness,
            pool,
            used_usd: used / 100.0,
            limit_usd: limit / 100.0,
        });
    };
    let failed = |out: &mut Usage, harness, e: &herdr::Error| {
        out.lines.push(format!(
            "*{harness}* unavailable: {}",
            head(&e.to_string(), 200)
        ));
        out.unavailable.push(Unavailable {
            machine: machine.clone(),
            harness,
        });
    };
    let (claude, cursor) = tokio::join!(herdr::usage(m), herdr::cursor_usage(m));
    if let Ok(Some(c)) = &claude {
        out.logins.push(Login {
            machine: machine.clone(),
            account: c.account.clone(),
            read: c.usage.is_some(),
        });
    }
    match claude.map(|c| c.and_then(|c| c.usage)) {
        Ok(None) => {}
        Ok(Some(u)) => {
            out.lines.push("*claude* plan limits".into());
            let windows = [
                ("5h session", &u.five_hour),
                ("7d all", &u.seven_day),
                ("7d opus", &u.seven_day_opus),
                ("7d sonnet", &u.seven_day_sonnet),
            ];
            for (n, w) in windows {
                if let Some(w) = w {
                    let reset = w.resets_at.as_deref().and_then(unix);
                    window(&mut out, "claude", n, w.utilization, reset);
                }
            }
            if let Some(x) = u.extra_usage.filter(|x| x.is_enabled) {
                out.lines.push(format!(
                    "extra usage ${:.2} of ${:.2} this month",
                    x.used_credits / 100.0,
                    x.monthly_limit / 100.0
                ));
                spend(&mut out, "claude", "extra", x.used_credits, x.monthly_limit);
            }
        }
        Err(e) => failed(&mut out, "claude", &e),
    }
    match cursor {
        Ok(None) => {}
        Ok(Some(c)) => {
            out.lines.push("*cursor* billing cycle".into());
            let reset = c.billing_cycle_end.map(|ms| (ms.0 / 1000.0) as i64);
            if let Some(p) = c.plan_usage {
                let windows = [
                    ("plan total", p.total_percent_used, reset),
                    ("plan auto", p.auto_percent_used, None),
                    ("plan api", p.api_percent_used, None),
                ];
                for (n, pct, r) in windows {
                    if let Some(pct) = pct {
                        window(&mut out, "cursor", n, pct.0, r);
                    }
                }
                if let (Some(spent), Some(limit)) = (p.total_spend, p.limit) {
                    let bonus = p
                        .bonus_spend
                        .filter(|b| b.0 > 0.0)
                        .map(|b| format!(" + ${:.2} free bonus", b.0 / 100.0))
                        .unwrap_or_default();
                    out.lines.push(format!(
                        "you: ${:.2} spent (${:.2} included{bonus})",
                        spent.0 / 100.0,
                        limit.0 / 100.0
                    ));
                    spend(&mut out, "cursor", "plan", spent.0, limit.0);
                }
            }
            if let Some(o) = c.spend_limit_usage {
                // a team pool is everyone's on-demand spend; Cursor sends no per-member share
                let (who, pool, used, limit) = match o.limit_type.as_deref() {
                    Some("team") => (
                        "team pool, all members",
                        "on-demand-team",
                        o.pooled_used,
                        o.pooled_limit,
                    ),
                    _ => ("you", "on-demand", o.individual_used, o.individual_limit),
                };
                if let (Some(used), Some(limit)) = (used, limit) {
                    out.lines.push(format!(
                        "on-demand ({who}): ${:.2} of ${:.2}",
                        used.0 / 100.0,
                        limit.0 / 100.0
                    ));
                    spend(&mut out, "cursor", pool, used.0, limit.0);
                }
            }
        }
        Err(e) => failed(&mut out, "cursor", &e),
    }
    out
}

#[must_use]
fn home(cwd: &str) -> String {
    let user = ["/Users/", "/home/"]
        .iter()
        .find_map(|p| cwd.strip_prefix(p))
        .filter(|r| !r.is_empty() && !r.starts_with('/'));
    match user.map(|r| r.split_once('/')) {
        Some(Some((_, rest))) => format!("~/{rest}"),
        Some(None) => "~".into(),
        None => cwd.into(),
    }
}

impl App {
    fn commands(&self) -> Vec<CreateCommand> {
        let machine = || {
            self.machines()
                .fold(string("machine", "where it runs").required(true), |o, m| {
                    o.add_string_choice(&m.name, &m.name)
                })
        };
        vec![
            CreateCommand::new("new")
                .description("Worktree + agent in a new thread")
                .add_option(machine())
                .add_option(
                    string("repo", "repo under REPOS_ROOT")
                        .required(true)
                        .set_autocomplete(true),
                )
                .add_option(string("branch", "new name, existing branch, or #pr").required(true))
                .add_option(string("prompt", "first instruction").required(true))
                .add_option(
                    harness::HARNESSES
                        .iter()
                        .fold(string("agent", "default claude"), |o, h| {
                            o.add_string_choice(h.name, h.name)
                        }),
                )
                .add_option(string("model", "default if empty").set_autocomplete(true))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Boolean,
                    "scout",
                    "report only, no changes",
                )),
            CreateCommand::new("attach")
                .description("Follow an agent already running in herdr")
                .add_option(machine())
                .add_option(
                    string("agent", "running agent")
                        .required(true)
                        .set_autocomplete(true),
                ),
            CreateCommand::new("agents").description("Agents on every machine"),
            CreateCommand::new("worktrees").description("Worktrees on every machine"),
            CreateCommand::new("usage").description("Claude and Cursor usage on every machine"),
            CreateCommand::new("scout")
                .description("Start a scout: a short look with no repo, deleted when closed")
                .add_option(string("prompt", "what to look at").required(true)),
            CreateCommand::new("scout-close").description("End this scout and delete its folder"),
            CreateCommand::new("mate")
                .description("Start a mate: a dispatcher pane in the drover workspace")
                .add_option(string("name", "lowercase, up to 26 characters").required(true)),
            CreateCommand::new("take").description("Give this pane back to you"),
            CreateCommand::new("release").description("Let the mate drive this pane"),
            CreateCommand::new("screen").description("This thread's terminal"),
            CreateCommand::new("keys")
                .description("Send keys to this thread's agent")
                .add_option(string("keys", "e.g. esc, shift+tab enter").required(true)),
            CreateCommand::new("done")
                .description("Write the journal entry and close the workspace")
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Boolean,
                    "remove",
                    "also delete the checkout, and the branch if merged",
                ))
                .add_option(CreateCommandOption::new(
                    CommandOptionType::Boolean,
                    "journal",
                    "false: close without an entry",
                )),
            CreateCommand::new("recall")
                .description("Search the journal")
                .add_option(string("query", "words").required(true)),
            CreateCommand::new("journal")
                .description("Latest journal entries, of this task's repo inside a task thread"),
            CreateCommand::new("journal-rm")
                .description("Delete a journal entry")
                .add_option(
                    string("entry", "entry")
                        .required(true)
                        .set_autocomplete(true),
                ),
        ]
    }

    async fn open_thread(&self, http: &Http, name: &str) -> Result<ChannelId> {
        match self.channel.to_channel(http).await? {
            Channel::Guild(c) if c.kind == ChannelType::Text => {}
            _ => bail!("DISCORD_CHANNEL_ID is not a text channel"),
        }
        let create = CreateThread::new(head(name, 100))
            .kind(ChannelType::PublicThread)
            .auto_archive_duration(AutoArchiveDuration::OneWeek);
        Ok(self.channel.create_thread(http, create).await?.id)
    }

    // The machine and pane of a task, for commands that only look at its terminal.
    async fn target(&self, target: &str) -> Result<(&Machine, PaneId)> {
        let (_, cell) = self.find_task(target).await?;
        let t = cell.lock().await;
        Ok((self.machine(&t.machine)?, t.pane.clone()))
    }

    async fn finish(&self, http: &Http, t: &Task, teardown: Teardown, entry: &str) -> Result<()> {
        self.journal
            .add(journal::Entry {
                day: today(),
                machine: t.machine.clone(),
                repo: t.repo.clone(),
                branch: t.branch.clone().unwrap_or_default(),
                text: entry.into(),
            })
            .await?;
        if let Some(th) = t.thread {
            th.say(http, format!("```\n{}\n```", entry.replace("```", "")))
                .await?;
        }
        self.teardown(http, t, teardown).await?;
        Ok(())
    }

    // Whether the checkout was removed too.
    async fn teardown(&self, http: &Http, t: &Task, teardown: Teardown) -> Result<bool> {
        let m = self.machine(&t.machine)?;
        // drover-wt rm closes the workspace too; anything it kept is only closed
        let (removed, mut note) = match teardown {
            Teardown::Remove => match herdr::remove(m, &t.workspace).await {
                Ok(said) => (true, said),
                Err(e) => (false, format!("kept the checkout: {e}")),
            },
            Teardown::Close => (false, String::new()),
        };
        if !removed {
            // mates and scouts share the drover workspace, so only their pane goes
            let closed = if matches!(t.role, Role::Mate(_) | Role::Scout(_)) {
                herdr::close_pane(m, &t.pane).await
            } else {
                herdr::close(m, &t.workspace).await
            };
            if let Err(e) = closed {
                note = format!("{note}\nclose failed: {e}");
            }
        }
        if let Some(th) = t.thread
            && !note.trim().is_empty()
        {
            let _ = th.say(http, note.trim()).await;
        }
        if let Some(name) = t.role.mate() {
            // a mate's log records the close, and its owner list drops the pane
            let closed = log::append(&self.mate_dir(name), log::Event::Closed, &t.pane.0);
            let owners = self
                .write_owners(name, Some(&task_key(&t.machine, &t.pane)))
                .await;
            if let Err(e) = closed.and(owners) {
                eprintln!("mate {name}: {e}");
            }
        }
        if let Some(th) = t.thread {
            let _ = th.edit_thread(http, EditThread::new().archived(true)).await;
        }
        Ok(removed)
    }

    // Discord turns an interaction into an Action; the text it gets back is what the thread sees.
    async fn on_command(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        let thread = i.channel_id.to_string();
        let action = match i.data.name.as_str() {
            "new" => Action::New(Box::new(New {
                mate: None,
                name: None,
                at: None,
                fresh: false,
                machine: req(i, "machine")?.into(),
                repo: req(i, "repo")?.into(),
                branch: req(i, "branch")?.into(),
                prompt: req(i, "prompt")?.into(),
                agent: opt(i, "agent").map(String::from),
                model: opt(i, "model").map(String::from),
                scout: flag(i, "scout") == Some(true),
                discord: true,
            })),
            "attach" => Action::Attach(Attach {
                mate: None,
                machine: Some(req(i, "machine")?.into()),
                pane: req(i, "agent")?.into(),
                discord: true,
            }),
            "agents" => Action::Agents,
            "worktrees" => Action::Worktrees,
            "usage" => Action::Usage,
            "scout" => Action::Scout(Scout {
                prompt: req(i, "prompt")?.into(),
                discord: true,
            }),
            "scout-close" => Action::ScoutClose(Target { thread }),
            "mate" => Action::Mate(Mate {
                name: MateName::parse(req(i, "name")?)?,
                discord: true,
            }),
            "take" => Action::Take(Target { thread }),
            "release" => Action::Release(Target { thread }),
            "screen" => Action::Screen(Look {
                thread,
                full: true,
                discord: true,
            }),
            "keys" => Action::Keys(Keys {
                thread,
                keys: req(i, "keys")?
                    .split_whitespace()
                    .map(String::from)
                    .collect(),
                discord: true,
            }),
            "done" => Action::Done(Done {
                thread,
                remove: flag(i, "remove") == Some(true),
                journal: flag(i, "journal") != Some(false),
            }),
            "recall" => Action::Recall(Recall {
                query: req(i, "query")?.into(),
                full: true,
            }),
            "journal" => Action::JournalList(JournalList {
                task: self
                    .find_task(&thread)
                    .await
                    .is_ok()
                    .then(|| thread.clone()),
                full: true,
            }),
            "journal-rm" => Action::JournalRemove(JournalRemove {
                id: req(i, "entry")?.parse::<i64>()?,
            }),
            _ => return Ok(()),
        };
        let terminal = matches!(action, Action::Screen(_) | Action::Keys(_));
        i.defer(http).await?;
        let text = self.act(http, action).await?.discord();
        let mut reply = edit(tail(&text, 1990));
        if terminal {
            reply = reply.components(key_rows());
        }
        i.edit_response(http, reply).await?;
        Ok(())
    }

    async fn on_autocomplete(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        let m = match opt(i, "machine") {
            Some(name) => self.machine(name)?,
            None => &self.host,
        };
        let q = i
            .data
            .autocomplete()
            .map(|a| a.value.to_lowercase())
            .unwrap_or_default();
        let agent = |a: Agent| {
            let mut dir: Vec<&str> = a.cwd.rsplit('/').take(2).collect();
            dir.reverse();
            let name = format!(
                "{} {} {} {}",
                a.pane_id.0,
                a.agent,
                a.agent_status.as_str(),
                dir.join("/")
            );
            (head(&name, 100), a.pane_id.0)
        };
        let focused = i.data.autocomplete().map(|a| a.name);
        let choices: Vec<(String, String)> = match focused {
            Some("repo") => herdr::repos(m)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.clone(), r))
                .collect(),
            Some("model") => {
                let kind = opt(i, "agent").unwrap_or(harness::DEFAULT);
                herdr::models(m, kind)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|x| (x.clone(), x))
                    .collect()
            }
            Some("entry") => {
                // inside a task thread, only that task's repo
                let repo = match self.find_task(&i.channel_id.to_string()).await {
                    Ok((_, cell)) => {
                        let repo = cell.lock().await.repo.clone();
                        Some(repo)
                    }
                    Err(_) => None,
                };
                self.journal
                    .list(repo, 25)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(id, e)| {
                        let line = e.text.lines().next().unwrap_or_default();
                        let name = format!("#{id} {} {} {line}", e.day, e.repo);
                        (head(&name, 100), id.to_string())
                    })
                    .collect()
            }
            _ => herdr::agents(m)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(agent)
                .collect(),
        };
        let resp = choices
            .into_iter()
            .filter(|(name, _)| name.to_lowercase().contains(&q))
            .take(25)
            .fold(CreateAutocompleteResponse::new(), |r, (name, value)| {
                r.add_string_choice(name, value)
            });
        let _ = i
            .create_response(http, CreateInteractionResponse::Autocomplete(resp))
            .await;
        Ok(())
    }

    async fn on_button(&self, http: &Http, i: &ComponentInteraction) -> Result<()> {
        let Some(k) = i.data.custom_id.strip_prefix("key:") else {
            return Ok(());
        };
        let thread = i.channel_id.to_string();
        let action = match k {
            "refresh" => Action::Screen(Look {
                thread,
                full: true,
                discord: true,
            }),
            key => Action::Keys(Keys {
                thread,
                keys: vec![key.into()],
                discord: true,
            }),
        };
        i.defer(http).await?;
        let text = self.act(http, action).await?.discord();
        i.edit_response(http, edit(tail(&text, 1990)).components(key_rows()))
            .await?;
        Ok(())
    }

    // A plain message in a task thread is the next prompt.
    async fn on_message(&self, http: &Http, msg: &Message) -> Result<()> {
        if msg.author.bot || !self.allowed.contains(&msg.author.id) {
            return Ok(());
        }
        let Some(cell) = self.task_in(msg.channel_id).await else {
            return Ok(());
        };
        let text = std::iter::once(msg.content.clone())
            .chain(
                msg.attachments
                    .iter()
                    .map(|a| format!("attachment: {}", a.url)),
            )
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        match self.send_prompt(&cell, text, Some(msg.id)).await? {
            Sent::Now => {
                msg.react(http, emoji("👀")).await?;
            }
            Sent::Queued => {
                msg.react(http, emoji("👀")).await?;
                msg.reply(http, "queued — sends when the agent is idle")
                    .await?;
            }
            Sent::Blocked => {
                let (m, pane) = {
                    let t = cell.lock().await;
                    (self.machine(&t.machine)?, t.pane.clone())
                };
                let out = CreateMessage::new()
                    .content(screen(m, &pane, "answer the dialog first, then resend\n").await)
                    .components(key_rows())
                    .reference_message(msg);
                msg.channel_id.send_message(http, out).await?;
            }
        }
        Ok(())
    }

    // ── events: herdr pushes status per pane; agent.list only on (re)connect ──

    async fn reconcile(&self, http: &Http, machine: &str) {
        let Ok(m) = self.machine(machine) else { return };
        let Ok(list) = herdr::agents(m).await else {
            return;
        }; // unreachable, the watcher retries
        for (key, cell) in self.cells() {
            let seen = {
                let t = cell.lock().await;
                if t.machine != machine || t.phase == Phase::Gone {
                    continue;
                }
                match list.iter().find(|a| a.pane_id == t.pane) {
                    Some(a) => Seen::Agent(a.agent_status, a.session()),
                    None if t.status.is_some() => Seen::Gone,
                    None => continue, // still starting
                }
            };
            self.run(http, &key, &cell, seen).await;
        }
        self.save().await;
    }

    async fn on_push(&self, http: &Http, machine: &str, push: socket::Push) {
        let detected = matches!(push, socket::Push::Detected(_));
        let (pane, status) = match push {
            socket::Push::Status(pane, s) if s != Status::Unknown => (pane, Some(s)),
            socket::Push::Gone(pane) => (pane, None),
            socket::Push::Status(pane, _) | socket::Push::Detected(pane) => {
                if self.live_cell(machine, &pane).await.is_some() {
                    self.reconcile(http, machine).await; // the agent may have left or restarted
                } else if detected {
                    self.adopt(http, machine, &pane).await;
                }
                return;
            }
            socket::Push::Reconcile => return self.reconcile(http, machine).await,
            socket::Push::Lost => return,
        };
        let Some((key, cell)) = self.live_cell(machine, &pane).await else {
            return;
        };
        if status.is_some() && cell.lock().await.session.is_none() {
            return self.reconcile(http, machine).await; // learn the session from agent.list
        }
        self.run(
            http,
            &key,
            &cell,
            status.map_or(Seen::Gone, |s| Seen::Agent(s, None)),
        )
        .await;
        self.save().await;
    }

    async fn run(&self, http: &Http, key: &str, cell: &Cell, seen: Seen) {
        match self.step(http, cell, seen).await {
            Ok(Step::Keep) => {}
            Ok(Step::Drop) => self.remove(key),
            Ok(Step::Tell { mate, text }) => {
                if let Err(e) = self.tell_mate(&mate, &text).await {
                    eprintln!("mate {mate}: {e}");
                }
            }
            Err(e) => eprintln!("task {key}: {e}"),
        }
        // a crew pane that went gone leaves its mate's owner list; step has let go of the cell by now
        let gone_crew = {
            let t = cell.lock().await;
            match &t.role {
                Role::Crew { mate, .. } if t.phase == Phase::Gone => Some(mate.clone()),
                _ => None,
            }
        };
        if let Some(mate) = gone_crew
            && let Err(e) = self.write_owners(&mate, None).await
        {
            eprintln!("mate {mate}: {e}");
        }
    }

    // One task's turn.
    async fn step(&self, http: &Http, cell: &Cell, seen: Seen) -> Result<Step> {
        let mut t = cell.lock().await;
        let Ok(m) = self.machine(&t.machine) else {
            return Ok(Step::Keep);
        }; // a machine this host doesn't drive
        // the old agent's exit is no news, and its replacement is not started yet
        if matches!(t.phase, Phase::Gone | Phase::Restarting { .. }) {
            return Ok(Step::Keep);
        }
        let th = t.thread;
        if let Some(th) = th
            && let Err(e) = th.to_channel(http).await
        {
            return if not_found(&e) {
                Ok(Step::Drop)
            } else {
                Err(e.into())
            };
        }
        // a crewmate's mate hears of its replies, dialogs and exit, since it has no thread to read
        let crew = t.role.crew_of_mate().cloned();
        let tell = |text: String| {
            crew.clone()
                .map_or(Step::Keep, |mate| Step::Tell { mate, text })
        };
        let who = format!(
            "crew {} on {} ({} {})",
            t.pane.0,
            t.machine,
            t.repo,
            t.branch.as_deref().unwrap_or("-")
        );
        let Seen::Agent(now, session) = seen else {
            t.phase = Phase::Gone;
            if let Some(th) = th {
                th.say(http, "agent exited").await?;
            }
            return Ok(tell(format!("{who} exited")));
        };

        let prev = t.status.replace(now);
        if let Some(s) = session {
            t.session = Some(s);
        }

        let blocked = Some(Status::Blocked);
        let dialog = now == Status::Blocked && prev != blocked;
        if let Some(th) = th {
            if dialog {
                t.dialog_msg = Some(
                    th.send_message(http, screen_msg(m, &t.pane).await)
                        .await?
                        .id,
                );
                mark(http, th, &t, "⏸️").await;
            } else if prev == blocked && now != Status::Blocked {
                if let Some(d) = t.dialog_msg.take() {
                    let _ = th.delete_message(http, d).await;
                }
                mark(http, th, &t, "👀").await;
            }
        }

        if dialog {
            return Ok(tell(format!(
                "{who} waits on a dialog: `drover screen --pane {}`",
                t.pane.0
            )));
        }
        if !now.ready() {
            return Ok(Step::Keep);
        }
        if let Phase::Starting { prompt } = &t.phase {
            let prompt = prompt.clone();
            if let Some(s) = &t.session {
                t.last_reply = herdr::reply(m, s).await?.reply.map(|r| r.id);
            }
            t.phase = match herdr::prompt(m, &t.pane, &prompt).await {
                Ok(()) => Phase::Waiting,
                Err(e) => {
                    if let Some(th) = th {
                        th.say(http, format!("first prompt failed: {e}")).await?;
                    } else {
                        eprintln!("first prompt {}: {e}", t.pane.0);
                    }
                    Phase::Idle
                }
            };
            return Ok(Step::Keep);
        }

        let was_working = prev == Some(Status::Working);
        // a mate's turn that crew news started stays out of its thread; the mate escalates what matters
        let quiet = matches!(t.role, Role::Mate(_)) && !t.phase.waiting();
        let th = th.filter(|_| !quiet);
        let mut said = None;
        if !(t.phase.waiting() || was_working) {
            return self.drain_queue(http, &mut t, m).await;
        }
        if harness::find(&t.kind).is_some_and(|h| h.transcript) {
            let Some(s) = t.session.clone() else {
                return Ok(Step::Keep);
            };
            let Some(r) = fresh_reply(m, &s, t.last_reply.as_deref()).await? else {
                return Ok(Step::Keep);
            };
            t.last_reply = Some(r.id);
            if let Some(th) = th {
                mark(http, th, &t, "✅").await;
            }
            if let Phase::Ending(teardown) = t.phase {
                self.finish(http, &t, teardown, &r.text).await?;
                return Ok(Step::Drop);
            }
            t.phase = Phase::Idle;
            if let Some(th) = th {
                post(http, th, &r.text).await?;
            }
            said = Some(r.text);
        } else if was_working {
            // other agents have no transcript to read; the screen is the reply
            if let Phase::Ending(teardown) = t.phase {
                let screen = herdr::screen(m, &t.pane, 30).await?;
                self.finish(http, &t, teardown, &tail(&screen, 1000))
                    .await?;
                return Ok(Step::Drop);
            }
            if t.phase == Phase::Waiting {
                t.phase = Phase::Idle;
            }
            let screen = herdr::screen(m, &t.pane, 80).await?;
            if let Some(th) = th {
                mark(http, th, &t, "✅").await;
                post(http, th, &format!("```\n{}\n```", tail(&screen, 5000))).await?;
            }
            said = Some(screen);
        }
        if let Some(s) = &said {
            t.reply = Some(Excerpt::of(s));
        }
        self.drain_queue(http, &mut t, m).await?;
        // the end of a turn holds its question or result
        Ok(said.map_or(Step::Keep, |s| {
            let s = s.trim();
            let more = match s.chars().count() > 800 {
                true => format!("\n(cut: `drover reply --pane {}` has all of it)", t.pane.0),
                false => String::new(),
            };
            tell(format!("{who} replied:\n{}{more}", tail(s, 800)))
        }))
    }

    // One prompt may wait while the agent works; send it once idle.
    async fn drain_queue(&self, http: &Http, t: &mut Task, m: &Machine) -> Result<Step> {
        if t.phase != Phase::Idle {
            return Ok(Step::Keep);
        }
        let Some(text) = t.queued.take() else {
            if !t.inbox.is_empty() {
                let news = t.inbox.join("\n\n");
                match herdr::prompt(m, &t.pane, &news).await {
                    Ok(()) => t.inbox.clear(),
                    Err(e) if e.is_busy() || e.is(&herdr::Code::AgentBlocked) => {}
                    Err(e) => eprintln!("mate inbox {}: {e}", t.pane.0),
                }
            }
            return Ok(Step::Keep);
        };
        match herdr::prompt(m, &t.pane, &text).await {
            Ok(()) => {
                t.phase = Phase::Waiting;
                if let Some(th) = t.thread {
                    mark(http, th, t, "👀").await;
                }
            }
            Err(e) if e.is_busy() || e.is(&herdr::Code::AgentBlocked) => {
                t.queued = Some(text); // still not ready; try next tick
            }
            Err(e) => {
                if let Some(th) = t.thread {
                    th.say(http, format!("queued prompt failed: {e}")).await?;
                } else {
                    eprintln!("queued prompt {}: {e}", t.pane.0);
                }
            }
        }
        Ok(Step::Keep)
    }
}

enum Step {
    Keep,
    Drop,                                  // the task is over or its thread is gone
    Tell { mate: MateName, text: String }, // keep, and pass this crewmate's news to its mate
}

// What herdr says about a task's pane.
enum Seen {
    Agent(Status, Option<SessionId>),
    Gone,
}

// The transcript trails the status flip: while the turn is open its last text may not be written
// yet, and the newest text is one from the middle of the turn. A turn with no end line coming
// (an interrupt, a local command) is not open, so this waits only on a turn being written.
async fn fresh_reply(
    m: &Machine,
    s: &SessionId,
    last: Option<&str>,
) -> Result<Option<herdr::Reply>> {
    let mut newest = None;
    for _ in 0..20 {
        let turn = herdr::reply(m, s).await?;
        let fresh = turn.reply.filter(|r| Some(r.id.as_str()) != last);
        if !turn.open {
            return Ok(fresh);
        }
        newest = fresh.or(newest);
        sleep(Duration::from_millis(250)).await;
    }
    Ok(newest)
}

// ── wiring ─────────────────────────────────────────────────

struct Handler(Arc<App>);

macro_rules! fail {
    ($i:expr, $http:expr, $content:expr) => {
        if $i
            .create_response($http, ephemeral($content.clone()))
            .await
            .is_err()
        {
            let _ = $i.edit_response($http, edit($content)).await;
        }
    };
}

#[async_trait]
impl EventHandler for Handler {
    async fn interaction_create(&self, ctx: Context, i: Interaction) {
        let app = &self.0;
        let http = &ctx.http;
        let user = match &i {
            Interaction::Command(c) | Interaction::Autocomplete(c) => c.user.id,
            Interaction::Component(c) => c.user.id,
            _ => return,
        };
        if !app.allowed.contains(&user) {
            match &i {
                Interaction::Command(c) => {
                    drop(c.create_response(http, ephemeral("not allowed")).await)
                }
                Interaction::Component(c) => {
                    drop(c.create_response(http, ephemeral("not allowed")).await)
                }
                _ => {}
            }
            return;
        }
        let r = match &i {
            Interaction::Autocomplete(c) => app.on_autocomplete(http, c).await,
            Interaction::Component(c) => app.on_button(http, c).await,
            Interaction::Command(c) => app.on_command(http, c).await,
            _ => Ok(()),
        };
        let Err(e) = r else { return };
        eprintln!("{e}");
        let content = head(&e.to_string(), 1900);
        match &i {
            Interaction::Command(c) => fail!(c, http, content),
            Interaction::Component(c) => fail!(c, http, content),
            _ => {}
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if let Err(e) = self.0.on_message(&ctx.http, &msg).await {
            eprintln!("{e}");
            let _ = msg.reply(&ctx.http, head(&e.to_string(), 1900)).await;
        }
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        let app = Arc::clone(&self.0);
        // only this flag is shared, so no ordering with other memory is needed
        if app.started.swap(true, Ordering::Relaxed) {
            return; // a reconnect, the poller already runs
        }
        if let Err(e) = app.guild.set_commands(&ctx.http, app.commands()).await {
            eprintln!("register commands: {e}");
        }
        let names: Vec<&str> = app.machines().map(|m| m.name.as_str()).collect();
        println!(
            "drover ready as {} on {}",
            ready.user.tag(),
            names.join(", ")
        );
        let http = Arc::clone(&ctx.http);
        let machines: Vec<Machine> = app.machines().cloned().collect();
        for m in machines {
            let app = Arc::clone(&app);
            let http = Arc::clone(&http);
            tokio::spawn(async move {
                watch_machine(app, http, m).await;
            });
        }
        let usage_app = Arc::clone(&app);
        tokio::spawn(async move {
            loop {
                if let Ok(u) = usage_app.cmd_usage().await {
                    *usage_app
                        .usage
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = Some(u);
                    usage_app.changed.notify_waiters();
                }
                tokio::time::sleep(USAGE_REFRESH).await;
            }
        });
        let (control_app, control_http) = (Arc::clone(&app), Arc::clone(&http));
        tokio::spawn(async move {
            if let Err(e) = control::serve(control_app, control_http).await {
                eprintln!("control: {e}");
            }
        });
    }
}

async fn watch_machine(app: Arc<App>, http: Arc<Http>, m: Machine) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match watch_session(&app, &http, &m).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                eprintln!("events {}: {e}", m.name);
                app.set_reachability(&m.name, Reachability::Unreachable);
                sleep(backoff).await;
                backoff = (backoff.saturating_mul(2)).min(Duration::from_secs(30));
            }
        }
    }
}

async fn watch_session(app: &App, http: &Http, m: &Machine) -> Result<()> {
    let watched = loop {
        // enabled before the check, so a pane added after it still wakes us
        let woke = app.wake.notified();
        tokio::pin!(woke);
        woke.as_mut().enable();
        let panes = app.panes_on(&m.name).await;
        if !panes.is_empty() {
            break panes;
        }
        woke.await;
    };

    // herdr rejects a subscription naming a pane that is gone; reconcile marks those
    let sub: Vec<PaneId> = match herdr::panes(m).await {
        Ok(live) => watched
            .iter()
            .filter(|p| live.contains(p))
            .cloned()
            .collect(),
        Err(_) => watched.clone(),
    };
    let mut conn = socket::Events::subscribe(m.ssh(), &sub).await?;
    app.set_reachability(&m.name, Reachability::Reachable);
    // subscribe first, then read, so nothing between the two is missed
    app.reconcile(http, &m.name).await;

    loop {
        let woke = app.wake.notified();
        tokio::pin!(woke);
        woke.as_mut().enable();
        if app.panes_on(&m.name).await != watched {
            return Ok(()); // resubscribe with the new pane set
        }
        tokio::select! {
            ev = conn.recv() => match ev? {
                socket::Push::Lost => bail!("events_lost on {}", m.name),
                push => app.on_push(http, &m.name, push).await,
            },
            () = &mut woke => {}
        }
    }
}

fn on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file())
}

// drover-wt comes from the dotfiles and needs the rest.
fn require_runtime() -> Result<()> {
    let missing: Vec<_> = ["herdr", "drover-wt", "git", "jq", "ssh"]
        .into_iter()
        .filter(|cmd| !on_path(cmd))
        .collect();
    if !missing.is_empty() {
        bail!("missing on PATH: {}", missing.join(", "));
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "serve") {
        return fail(serve().await);
    }
    std::process::exit(cli::run(cli::parse(&args)).await);
}

fn fail(r: Result<()>) {
    if let Err(e) = r {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn serve() -> Result<()> {
    require_runtime()?;
    let app = Arc::new(App::load()?);
    let intents =
        GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(&app.token, intents)
        .event_handler(Handler(Arc::clone(&app)))
        .await?;
    client.start().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_typescript_state() -> Result<()> {
        let old = r#"{"123": {"machine":"host","pane":"p1","workspace":"w1","kind":"claude","repo":"r",
            "branch":"feature/x","owned":true,"status":"idle","expecting":true,"ending":{"remove":true},
            "session":"not-a-uuid","promptMsg":"456"}}"#;
        let mut tasks = parse_state(old)?;
        let t = tasks.get("host:p1").context("task")?;
        assert_eq!(t.thread, Some(ChannelId::new(123)));
        assert!(t.phase == Phase::Ending(Teardown::Remove));
        assert!(t.origin == Origin::Opened && t.session.is_none());
        assert!(t.role == Role::Task);
        assert_eq!(t.prompt_msg, Some(MessageId::new(456)));
        assert!(t.reply.is_none());

        let mut term = t.clone();
        term.reply = Some(Excerpt::of(&format!(" {} ", "é".repeat(400))));
        term.pane = PaneId("p2".into());
        term.thread = None;
        tasks.insert("host:p2".into(), term);

        let back = parse_state(&serde_json::to_string(&tasks)?)?;
        assert!(
            back.get("host:p1")
                .is_some_and(|t| t.thread == Some(ChannelId::new(123))
                    && t.phase == Phase::Ending(Teardown::Remove))
        );
        assert!(back.get("host:p2").is_some_and(|t| {
            t.thread.is_none()
                && t.reply
                    .as_ref()
                    .is_some_and(|r| r.chars == 400 && r.text.chars().count() == 300)
        }));
        Ok(())
    }

    #[test]
    fn reads_mates_from_before_roles() -> Result<()> {
        let task = |pane: &str, repo: &str, extra: &str| {
            format!(
                r#""host:{pane}": {{"machine":"host","pane":"{pane}","workspace":"w","kind":"claude","repo":"{repo}","branch":"","origin":"attached","phase":"idle"{extra}}}"#
            )
        };
        let old = format!(
            "{{{}, {}, {}, {}}}",
            task("p1", "mates", r#","owner":"mate","mate":"a""#),
            task("p2", "drover", r#","owner":"mate","mate":"a""#),
            task("p3", "scouts", r#","owner":"user","scout":"scout-abc123""#),
            task("p4", "drover", r#","owner":"user""#),
        );
        let tasks = parse_state(&old)?;
        let a = MateName::parse("a")?;
        let role = |k: &str| tasks.get(k).map(|t| t.role.clone());
        assert!(role("host:p1") == Some(Role::Mate(a.clone())));
        assert!(
            role("host:p2")
                == Some(Role::Crew {
                    mate: a.clone(),
                    owner: Owner::Mate
                })
        );
        assert!(role("host:p3") == Some(Role::Scout("scout-abc123".into())));
        assert!(role("host:p4") == Some(Role::Task));
        let back = parse_state(&serde_json::to_string(&tasks)?)?;
        assert!(back.get("host:p2").map(|t| &t.role) == tasks.get("host:p2").map(|t| &t.role));
        Ok(())
    }

    #[test]
    fn helpers() {
        assert_eq!(slug("Feature/Fix-Bug!!x"), "feature-fix-bug-x");
        assert_eq!(home("/Users/alice/Developer/x"), "~/Developer/x");
        assert_eq!(home("/home/alice"), "~");
        assert_eq!(home("/opt/x"), "/opt/x");
        assert_eq!(tail("abcdef", 3), "…def");
        assert_eq!(today().len(), 10);
        assert_eq!(unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            unix("2026-10-02T22:50:00.185835+00:00"),
            Some(1_790_981_400)
        );
        assert_eq!(unix("2024-02-29T12:00:00Z"), Some(1_709_208_000));
    }

    #[tokio::test]
    async fn owners_skip_the_held_cell_and_gone_crew() -> Result<()> {
        let crew = |pane: &str, phase: &str| {
            format!(
                r#""host:{pane}": {{"machine":"host","pane":"{pane}","workspace":"w","kind":"claude","repo":"drover","branch":"","origin":"attached","phase":"{phase}","owner":"mate","mate":"a"}}"#
            )
        };
        let state = format!(
            "{{{}, {}, {}}}",
            crew("p1", "idle"),
            crew("p2", "idle"),
            crew("p3", "gone")
        );
        let root = std::env::temp_dir().join(format!("drover-owners-{}", std::process::id()));
        let app = App {
            token: String::new(),
            guild: GuildId::new(1),
            channel: ChannelId::new(1),
            allowed: HashSet::new(),
            host: Machine::new("host".into(), Reach::Local),
            remotes: Vec::new(),
            tasks: StdMutex::new(
                parse_state(&state)?
                    .into_iter()
                    .map(|(k, t)| (k, Arc::new(Mutex::new(t))))
                    .collect(),
            ),
            journal: Journal::open(":memory:")?,
            mates: root.clone(),
            started: AtomicBool::new(false),
            wake: Notify::new(),
            changed: Notify::new(),
            reachability: StdMutex::new(HashMap::new()),
            usage: StdMutex::new(None),
            saving: Mutex::new(()),
        };
        let a = MateName::parse("a")?;
        let cells = app.cells();
        let (_, held) = cells.iter().find(|(k, _)| k == "host:p1").context("p1")?;
        let _closing = held.lock().await; // step holds this while it tears the task down
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.write_owners(&a, Some("host:p1")),
        )
        .await
        .context("write_owners waited on the held cell")??;
        let owners = std::fs::read_to_string(root.join("state/a/owners"))?;
        assert_eq!(owners.trim(), "p2 mate");
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }
}
