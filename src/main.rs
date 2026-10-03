mod herdr;
mod format;
mod socket;
mod journal;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    hash::{BuildHasher, Hasher, RandomState},
    num::NonZeroU64,
    sync::{
        Arc, Mutex as StdMutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serenity::all::{
    AutoArchiveDuration, ButtonStyle, Channel, ChannelId, ChannelType, Client, CommandInteraction, CommandOptionType,
    ComponentInteraction, Context, CreateActionRow, CreateAttachment, CreateAutocompleteResponse, CreateButton,
    CreateCommand, CreateCommandOption, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
    CreateThread, EditInteractionResponse, EditThread, EventHandler, GatewayIntents, GuildId, Http, Interaction,
    Message, MessageId, ReactionType, Ready, UserId, async_trait,
};
use tokio::{sync::{Mutex, Notify}, time::sleep};

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

// ── config ─────────────────────────────────────────────────

fn env(k: &str, d: Option<&str>) -> Result<String> {
    std::env::var(k).ok().or(d.map(String::from)).context(format!("missing env {k}"))
}

fn id<T: From<NonZeroU64>>(s: &str) -> Result<T> {
    Ok(s.trim().parse::<NonZeroU64>().context(format!("{s:?} is not a discord id"))?.into())
}

const STATE: &str = "state.json";
const SCOUT: &str = "Scout task: investigate and report back. Do not edit files, commit, or push.";

// ── state: one thread per herdr pane ───────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Origin {
    Opened,   // worktree opened by drover, so /done may remove it
    Attached, // started at the desk, drover only follows it
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

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "Stored")]
struct Task {
    machine: String,
    pane: PaneId,
    workspace: WorkspaceId,
    kind: String,
    repo: String,
    branch: String,
    origin: Origin,
    phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<Status>, // none until the poller has seen the agent
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_msg: Option<MessageId>, // message that carries the state reaction
    #[serde(skip_serializing_if = "Option::is_none")]
    dialog_msg: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queued: Option<String>, // one Discord prompt waiting while the agent works
}

// What state.json may hold: this format, or the flags of the TypeScript drover.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    machine: String,
    pane: PaneId,
    workspace: WorkspaceId,
    kind: String,
    repo: String,
    branch: String,
    origin: Option<Origin>,
    phase: Option<Phase>,
    status: Option<Status>,
    session: Option<String>,
    last_reply: Option<String>,
    prompt_msg: Option<MessageId>,
    dialog_msg: Option<MessageId>,
    queued: Option<String>,
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
            (_, Some(e), _) => Phase::Ending(if e.remove { Teardown::Remove } else { Teardown::Close }),
            (_, _, Some(p)) => Phase::Starting { prompt: p.clone() },
            _ if s.expecting => Phase::Waiting,
            _ => Phase::Idle,
        };
        let phase = s.phase.clone().unwrap_or_else(legacy);
        let origin = s.origin.unwrap_or(if s.owned { Origin::Opened } else { Origin::Attached });
        Self {
            session: s.session.as_deref().and_then(SessionId::parse),
            machine: s.machine,
            pane: s.pane,
            workspace: s.workspace,
            kind: s.kind,
            repo: s.repo,
            branch: s.branch,
            origin,
            phase,
            status: s.status,
            last_reply: s.last_reply,
            prompt_msg: s.prompt_msg,
            dialog_msg: s.dialog_msg,
            queued: s.queued,
        }
    }
}

// Each task has its own lock so the poller and a command on the same thread
// take turns, while other threads go on.
type Cell = Arc<Mutex<Task>>;
type Tasks = HashMap<ChannelId, Cell>;

struct App {
    token: String,
    guild: GuildId,
    channel: ChannelId,
    allowed: HashSet<UserId>,
    host: Machine,
    remotes: Vec<Machine>,
    tasks: StdMutex<Tasks>,
    journal: Journal,
    started: AtomicBool,
    /// Watchers resubscribe when the set of panes for a machine changes.
    wake: Notify,
}

impl App {
    fn load() -> Result<Self> {
        let allowed = env("ALLOWED_USER_IDS", None)?
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(id)
            .collect::<Result<_>>()?;
        let remotes = env("REMOTES", Some(""))?
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|name| Machine::new(name.into(), Reach::Ssh))
            .collect();

        let tasks: BTreeMap<ChannelId, Task> = match std::fs::read_to_string(STATE) {
            Ok(s) => serde_json::from_str(&s).context(format!("{STATE} is not valid, fix or remove it"))?,
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
            tasks: StdMutex::new(tasks.into_iter().map(|(k, t)| (k, Arc::new(Mutex::new(t)))).collect()),
            journal: Journal::open("journal.db")?,
            started: AtomicBool::new(false),
            wake: Notify::new(),
        })
    }

    fn machines(&self) -> impl Iterator<Item = &Machine> {
        std::iter::once(&self.host).chain(&self.remotes)
    }

    fn machine(&self, name: &str) -> Result<&Machine> {
        self.machines().find(|m| m.name == name).context(format!("unknown machine {name}"))
    }

    // A poisoned lock only means a handler panicked; the map itself is fine.
    fn map(&self) -> MutexGuard<'_, Tasks> {
        self.tasks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cells(&self) -> Vec<(ChannelId, Cell)> {
        self.map().iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    fn insert(&self, id: ChannelId, t: Task) {
        self.map().insert(id, Arc::new(Mutex::new(t)));
        self.wake.notify_waiters();
    }

    fn remove(&self, id: ChannelId) {
        self.map().remove(&id);
        self.wake.notify_waiters();
    }

    fn task_in(&self, channel: ChannelId) -> Result<Cell> {
        self.map().get(&channel).cloned().context("run this inside a task thread")
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
        let mut all = BTreeMap::new();
        for (id, cell) in self.cells() {
            all.insert(id, cell.lock().await.clone());
        }
        let written = match serde_json::to_string_pretty(&all) {
            Ok(json) => tokio::fs::write(STATE, json).await.map_err(Error::from),
            Err(e) => Err(e.into()),
        };
        if let Err(e) = written {
            eprintln!("save {STATE}: {e}");
        }
    }

    async fn live(&self, machine: &str, pane: &PaneId) -> Option<ChannelId> {
        for (id, cell) in self.cells() {
            let t = cell.lock().await;
            if t.machine == machine && t.pane == *pane && t.phase != Phase::Gone {
                return Some(id);
            }
        }
        None
    }
}

// ── discord ui ─────────────────────────────────────────────

#[must_use]
fn tail(s: &str, n: usize) -> String {
    let len = s.chars().count();
    if len > n { format!("…{}", s.chars().skip(len - n).collect::<String>()) } else { s.into() }
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
            .add_file(CreateAttachment::bytes(text.as_bytes().to_vec(), "reply.md"));
        th.send_message(http, msg).await?;
        return Ok(());
    }
    for part in parts {
        th.say(http, part).await?;
    }
    Ok(())
}

const KEYS: [&[&str]; 2] = [&["1", "2", "3", "up", "down"], &["enter", "esc", "tab", "ctrl+c", "refresh"]];

#[must_use]
fn key_rows() -> Vec<CreateActionRow> {
    let button = |k: &str| {
        let style = if k == "enter" { ButtonStyle::Primary } else { ButtonStyle::Secondary };
        CreateButton::new(format!("key:{k}")).label(k).style(style)
    };
    KEYS.iter().map(|row| CreateActionRow::Buttons(row.iter().map(|k| button(k)).collect())).collect()
}

async fn screen(m: &Machine, pane: &PaneId, note: &str) -> String {
    let raw = herdr::screen(m, pane, 60).await.unwrap_or_else(|e| format!("read failed: {e}"));
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    let text = lines.iter().skip(lines.len().saturating_sub(30)).copied().collect::<Vec<_>>().join("\n");
    format!("{note}```\n{}\n```", tail(&text.replace("```", "ˋˋˋ"), 1800))
}

async fn screen_msg(m: &Machine, pane: &PaneId) -> CreateMessage {
    CreateMessage::new().content(screen(m, pane, "").await).components(key_rows())
}

// State lives in one reaction on the latest prompt: 👀 working, ⏸ needs you, ✅ done.
async fn mark(http: &Http, th: ChannelId, t: &Task, e: &str) {
    let Some(id) = t.prompt_msg else { return };
    let Ok(msg) = th.message(http, id).await else { return };
    let want = emoji(e);
    for r in msg.reactions.iter().filter(|r| r.me && r.reaction_type != want) {
        let _ = th.delete_reaction(http, id, None, r.reaction_type.clone()).await;
    }
    let _ = th.create_reaction(http, id, want).await;
}

// ── commands ───────────────────────────────────────────────

fn string(name: &str, description: &str) -> CreateCommandOption {
    CreateCommandOption::new(CommandOptionType::String, name, description)
}

#[must_use]
fn opt<'a>(i: &'a CommandInteraction, name: &str) -> Option<&'a str> {
    i.data.options.iter().find(|o| o.name == name).and_then(|o| o.value.as_str())
}

fn flag(i: &CommandInteraction, name: &str) -> Option<bool> {
    i.data.options.iter().find(|o| o.name == name).and_then(|o| o.value.as_bool())
}

fn req<'a>(i: &'a CommandInteraction, name: &str) -> Result<&'a str> {
    opt(i, name).context(format!("missing {name}"))
}

#[must_use]
fn reply(content: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content))
}

#[must_use]
fn ephemeral(content: impl Into<String>) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(content).ephemeral(true))
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
            #[allow(clippy::expect_used)]
            char::from_digit(d, 36).expect("digit below radix")
        })
        .collect();
    format!("d-{}-{suffix}", head(&slug(s), 24))
}

// UTC yyyy-mm-dd, from days since the epoch (Howard Hinnant's civil_from_days).
#[must_use]
fn today() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
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
    let reset = reset.map(|t| format!(" resets <t:{t}:R>")).unwrap_or_default();
    format!("`{name:<6} {}{} {pct:>3.0}%`{reset}", "█".repeat(filled), "░".repeat(10 - filled))
}

#[must_use]
fn tokens(n: f64) -> String {
    match n {
        n if n >= 1e6 => format!("{:.1}M", n / 1e6),
        n if n >= 1e3 => format!("{:.0}k", n / 1e3),
        n => format!("{n:.0}"),
    }
}

// One machine's plans; an agent that isn't logged in there is left out.
async fn usage_lines(m: &Machine) -> Vec<String> {
    fn failed(lines: &mut Vec<String>, agent: &str, e: &herdr::Error) {
        lines.push(format!("*{agent}* unavailable: {}", head(&e.to_string(), 200)));
    }
    let (claude, cursor, pi) = tokio::join!(herdr::usage(m), herdr::cursor_usage(m), herdr::pi_usage(m));
    let mut lines = vec![format!("**{}**", m.name)];
    match claude {
        Ok(None) => {}
        Ok(Some(u)) => {
            lines.push("*claude*".into());
            let windows = [("5h", &u.five_hour), ("week", &u.seven_day), ("opus", &u.seven_day_opus), ("sonnet", &u.seven_day_sonnet)];
            lines.extend(windows.iter().filter_map(|(n, w)| w.as_ref().map(|w| bar(n, w.utilization, w.resets_at.as_deref().and_then(unix)))));
            if let Some(x) = u.extra_usage.filter(|x| x.is_enabled) {
                lines.push(format!("extra ${:.2} of ${:.2}", x.used_credits / 100.0, x.monthly_limit / 100.0));
            }
        }
        Err(e) => failed(&mut lines, "claude", &e),
    }
    match cursor {
        Ok(None) => {}
        Ok(Some(c)) => {
            lines.push("*cursor*".into());
            let reset = c.billing_cycle_end.map(|ms| (ms.0 / 1000.0) as i64);
            if let Some(p) = c.plan_usage {
                let windows = [("plan", p.total_percent_used, reset), ("auto", p.auto_percent_used, None), ("api", p.api_percent_used, None)];
                lines.extend(windows.iter().filter_map(|(n, pct, r)| pct.map(|pct| bar(n, pct.0, *r))));
                if let (Some(spent), Some(limit)) = (p.total_spend, p.limit) {
                    let bonus = p.bonus_spend.filter(|b| b.0 > 0.0).map(|b| format!(", ${:.2} bonus", b.0 / 100.0)).unwrap_or_default();
                    lines.push(format!("${:.2} spent: ${:.2} included{bonus}", spent.0 / 100.0, limit.0 / 100.0));
                }
            }
            if let Some(o) = c.spend_limit_usage {
                let (used, limit) = match o.limit_type.as_deref() {
                    Some("team") => (o.pooled_used, o.pooled_limit),
                    _ => (o.individual_used, o.individual_limit),
                };
                if let (Some(used), Some(limit)) = (used, limit) {
                    let kind = o.limit_type.unwrap_or_default();
                    lines.push(format!("on-demand ${:.2} of ${:.2} {kind}", used.0 / 100.0, limit.0 / 100.0));
                }
            }
        }
        Err(e) => failed(&mut lines, "cursor", &e),
    }
    match pi {
        Ok(None) => {}
        Ok(Some(p)) => {
            let spend = |s: &herdr::Spend| format!("{} ${:.2}", tokens(s.tokens), s.cost);
            lines.push(format!("*pi* {}", p.providers.join(", ")));
            lines.push(format!("`24h {} · 7d {} · 30d {}`", spend(&p.day), spend(&p.week), spend(&p.month)));
        }
        Err(e) => failed(&mut lines, "pi", &e),
    }
    lines
}

#[must_use]
fn home(cwd: &str) -> String {
    let user = ["/Users/", "/home/"].iter().find_map(|p| cwd.strip_prefix(p)).filter(|r| !r.is_empty() && !r.starts_with('/'));
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
                .fold(string("machine", "where it runs").required(true), |o, m| o.add_string_choice(&m.name, &m.name))
        };
        let kinds = ["claude", "codex", "cursor", "opencode", "pi"];
        vec![
            CreateCommand::new("new")
                .description("Worktree + agent in a new thread")
                .add_option(machine())
                .add_option(string("repo", "repo under REPOS_ROOT").required(true).set_autocomplete(true))
                .add_option(string("branch", "new name, existing branch, or #pr").required(true))
                .add_option(string("prompt", "first instruction").required(true))
                .add_option(kinds.iter().fold(string("agent", "default claude"), |o, k| o.add_string_choice(*k, *k)))
                .add_option(CreateCommandOption::new(CommandOptionType::Boolean, "scout", "report only, no changes")),
            CreateCommand::new("attach")
                .description("Follow an agent already running in herdr")
                .add_option(machine())
                .add_option(string("agent", "running agent").required(true).set_autocomplete(true)),
            CreateCommand::new("agents").description("Agents on every machine"),
            CreateCommand::new("worktrees").description("Worktrees on every machine"),
            CreateCommand::new("usage").description("Claude, Cursor and pi usage on every machine"),
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
                .add_option(CreateCommandOption::new(CommandOptionType::Boolean, "journal", "false: close without an entry")),
            CreateCommand::new("recall")
                .description("Search the journal")
                .add_option(string("query", "words").required(true)),
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

    // The machine and pane of a thread's task, for commands that only look at its terminal.
    async fn target(&self, channel: ChannelId) -> Result<(&Machine, PaneId)> {
        let cell = self.task_in(channel)?;
        let t = cell.lock().await;
        Ok((self.machine(&t.machine)?, t.pane.clone()))
    }

    async fn cmd_new(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        let m = self.machine(req(i, "machine")?)?;
        let repo = Name::parse(req(i, "repo")?)?;
        let r#ref = Name::parse(req(i, "branch")?)?;
        let text = req(i, "prompt")?;
        let kind = opt(i, "agent").unwrap_or("claude");
        let scout = flag(i, "scout") == Some(true);

        i.defer(http).await?;
        let wt = herdr::worktree(m, &repo, &r#ref).await?;
        if let Some(bound) = self.live(&m.name, &wt.pane_id).await {
            i.edit_response(http, edit(format!("already open in <#{bound}>"))).await?;
            return Ok(());
        }
        if herdr::agents(m).await?.iter().any(|a| a.pane_id == wt.pane_id) {
            i.edit_response(http, edit(format!("an agent already runs in {}, /attach it", wt.pane_id.0))).await?;
            return Ok(());
        }

        let repo = repo.as_str();
        let th = self.open_thread(http, &format!("{repo} · {} · {}", wt.branch, m.name)).await?;
        i.edit_response(http, edit(format!("<#{th}>"))).await?;
        let head = th.say(http, format!("`{}`\n> {}", wt.path, tail(text, 1500).replace('\n', "\n> "))).await?;

        let memory = self.journal.context(repo, text, 2000)?;
        let mut prompt = if scout { format!("{SCOUT}\n\n{text}") } else { text.into() };
        if !memory.is_empty() {
            prompt = format!("{prompt}\n\nContext from earlier tasks (journal, may be stale):\n{memory}");
        }
        self.insert(th, Task {
            machine: m.name.clone(),
            pane: wt.pane_id.clone(),
            workspace: wt.workspace_id,
            kind: kind.into(),
            repo: repo.into(),
            branch: wt.branch.clone(),
            origin: Origin::Opened,
            phase: Phase::Starting { prompt },
            status: None,
            session: None,
            last_reply: None,
            prompt_msg: Some(head.id),
            dialog_msg: None,
            queued: None,
        });
        self.save().await;
        th.create_reaction(http, head.id, emoji("👀")).await?;
        // a startup dialog shows up through the poller; the prompt goes out once the agent is idle
        Ok(herdr::start(m, &agent_name(&wt.branch), kind, &wt.pane_id, &wt.path).await?)
    }

    async fn cmd_attach(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        let m = self.machine(req(i, "machine")?)?;
        let pane = PaneId(req(i, "agent")?.into());
        let a = herdr::agents(m)
            .await?
            .into_iter()
            .find(|x| x.pane_id == pane)
            .context(format!("no agent in {} on {}", pane.0, m.name))?;
        if let Some(bound) = self.live(&m.name, &pane).await {
            i.create_response(http, reply(format!("already open in <#{bound}>"))).await?;
            return Ok(());
        }

        i.defer(http).await?;
        let repo = a.cwd.rsplit('/').next().unwrap_or_default().to_string();
        let th = self.open_thread(http, &format!("{repo} · {} · {}", a.agent, m.name)).await?;
        let session = a.session();
        let last_reply = match &session {
            Some(s) => herdr::reply(m, s).await.map(|r| r.id),
            None => None,
        };
        self.insert(th, Task {
            machine: m.name.clone(),
            pane: pane.clone(),
            workspace: a.workspace_id,
            kind: a.agent,
            repo,
            branch: String::new(),
            origin: Origin::Attached,
            phase: Phase::Idle,
            status: Some(a.agent_status),
            session,
            last_reply,
            prompt_msg: None,
            dialog_msg: None,
            queued: None,
        });
        self.save().await;
        i.edit_response(http, edit(format!("<#{th}>"))).await?;
        th.send_message(http, screen_msg(m, &pane).await).await?;
        Ok(())
    }

    async fn cmd_agents(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        i.defer(http).await?;
        let mut lines = vec![];
        for m in self.machines() {
            lines.push(format!("**{}**", m.name));
            let list = match herdr::agents(m).await {
                Ok(list) => list,
                Err(e) => {
                    lines.push(format!("unreachable: {e}"));
                    continue;
                }
            };
            for a in list {
                let bound = self.live(&m.name, &a.pane_id).await.map(|b| format!(" <#{b}>")).unwrap_or_default();
                let icon = a.agent_status.icon();
                lines.push(format!("`{icon} {}` {} {}{bound}", a.pane_id.0, a.agent, home(&a.cwd)));
            }
        }
        i.edit_response(http, edit(tail(&lines.join("\n"), 1990))).await?;
        Ok(())
    }

    async fn cmd_usage(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        i.defer(http).await?;
        let jobs: Vec<_> = self.machines().cloned().map(|m| tokio::spawn(async move { usage_lines(&m).await })).collect();
        let mut lines = vec![];
        for job in jobs {
            lines.extend(job.await?);
        }
        i.edit_response(http, edit(tail(&lines.join("\n"), 1990))).await?;
        Ok(())
    }

    async fn cmd_worktrees(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        i.defer(http).await?;
        let jobs: Vec<_> = self
            .machines()
            .cloned()
            .map(|m| {
                tokio::spawn(async move {
                    let s = match herdr::worktrees(&m).await {
                        Ok(s) if s.is_empty() => "none".into(),
                        Ok(s) => s,
                        Err(e) => format!("unreachable: {e}"),
                    };
                    format!("**{}**\n```\n{s}\n```", m.name)
                })
            })
            .collect();
        let mut parts = Vec::new();
        for job in jobs {
            parts.push(job.await?);
        }
        i.edit_response(http, edit(tail(&parts.join("\n"), 1990))).await?;
        Ok(())
    }

    async fn cmd_done(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        let cell = self.task_in(i.channel_id)?;
        let teardown = if flag(i, "remove") == Some(true) { Teardown::Remove } else { Teardown::Close };
        let journal = flag(i, "journal") != Some(false);
        i.defer(http).await?;
        {
            let mut t = cell.lock().await;
            if teardown == Teardown::Remove && t.origin == Origin::Attached {
                bail!("drover didn't open this worktree, remove it by hand");
            }
            if journal {
                herdr::prompt(self.machine(&t.machine)?, &t.pane, journal::ENTRY_PROMPT).await?;
                t.phase = Phase::Ending(teardown);
            }
        }
        if !journal {
            let t = cell.lock().await.clone();
            self.remove(i.channel_id);
            self.save().await;
            i.edit_response(http, edit("closing without a journal entry")).await?;
            return self.teardown(http, i.channel_id, &t, teardown).await;
        }
        let msg = i.edit_response(http, edit("writing the journal entry")).await?;
        cell.lock().await.prompt_msg = Some(msg.id);
        self.save().await;
        i.channel_id.create_reaction(http, msg.id, emoji("👀")).await?;
        Ok(())
    }

    async fn finish(&self, http: &Http, th: ChannelId, t: &Task, teardown: Teardown, entry: &str) -> Result<()> {
        self.journal.add(&journal::Entry {
            day: today(),
            machine: t.machine.clone(),
            repo: t.repo.clone(),
            branch: t.branch.clone(),
            text: entry.into(),
        })?;
        th.say(http, format!("```\n{}\n```", entry.replace("```", ""))).await?;
        self.teardown(http, th, t, teardown).await
    }

    async fn teardown(&self, http: &Http, th: ChannelId, t: &Task, teardown: Teardown) -> Result<()> {
        let m = self.machine(&t.machine)?;
        let removed = match teardown {
            Teardown::Remove => herdr::remove(m, &t.workspace).await.map_err(|e| format!("kept the checkout: {e}")),
            Teardown::Close => Err(String::new()),
        };
        let note = match removed {
            Ok(said) => said,
            Err(kept) => match herdr::close(m, &t.workspace).await {
                Ok(()) => kept,
                Err(e) => format!("{kept}\nclose failed: {e}"),
            },
        };
        if !note.trim().is_empty() {
            let _ = th.say(http, note.trim()).await;
        }
        let _ = th.edit_thread(http, EditThread::new().archived(true)).await;
        Ok(())
    }

    async fn on_command(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        match i.data.name.as_str() {
            "new" => self.cmd_new(http, i).await,
            "attach" => self.cmd_attach(http, i).await,
            "agents" => self.cmd_agents(http, i).await,
            "worktrees" => self.cmd_worktrees(http, i).await,
            "usage" => self.cmd_usage(http, i).await,
            "done" => self.cmd_done(http, i).await,
            "screen" | "keys" => {
                let (m, pane) = self.target(i.channel_id).await?;
                i.defer(http).await?;
                if i.data.name == "keys" {
                    let keys: Vec<&str> = req(i, "keys")?.split_whitespace().collect();
                    herdr::keys(m, &pane, &keys).await?;
                    sleep(Duration::from_millis(700)).await;
                }
                i.edit_response(http, edit(screen(m, &pane, "").await).components(key_rows())).await?;
                Ok(())
            }
            "recall" => {
                let hits = self.journal.search(req(i, "query")?, 5)?;
                let text = match hits.is_empty() {
                    true => "nothing".into(),
                    false => tail(&hits.iter().map(journal::format).collect::<Vec<_>>().join("\n\n"), 1990),
                };
                i.create_response(http, reply(text)).await?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn on_autocomplete(&self, http: &Http, i: &CommandInteraction) -> Result<()> {
        let m = match opt(i, "machine") {
            Some(name) => self.machine(name)?,
            None => &self.host,
        };
        let q = i.data.autocomplete().map(|a| a.value.to_lowercase()).unwrap_or_default();
        let agent = |a: Agent| {
            let mut dir: Vec<&str> = a.cwd.rsplit('/').take(2).collect();
            dir.reverse();
            let name = format!("{} {} {} {}", a.pane_id.0, a.agent, a.agent_status.as_str(), dir.join("/"));
            (head(&name, 100), a.pane_id.0)
        };
        let choices: Vec<(String, String)> = match i.data.name.as_str() {
            "new" => herdr::repos(m).await.unwrap_or_default().into_iter().map(|r| (r.clone(), r)).collect(),
            _ => herdr::agents(m).await.unwrap_or_default().into_iter().map(agent).collect(),
        };
        let resp = choices
            .into_iter()
            .filter(|(name, _)| name.to_lowercase().contains(&q))
            .take(25)
            .fold(CreateAutocompleteResponse::new(), |r, (name, value)| r.add_string_choice(name, value));
        let _ = i.create_response(http, CreateInteractionResponse::Autocomplete(resp)).await;
        Ok(())
    }

    async fn on_button(&self, http: &Http, i: &ComponentInteraction) -> Result<()> {
        let Some(k) = i.data.custom_id.strip_prefix("key:") else { return Ok(()) };
        let (m, pane) = self.target(i.channel_id).await?;
        i.defer(http).await?;
        if k != "refresh" {
            herdr::keys(m, &pane, &[k]).await?;
            sleep(Duration::from_millis(700)).await;
        }
        i.edit_response(http, edit(screen(m, &pane, "").await).components(key_rows())).await?;
        Ok(())
    }

    // A plain message in a task thread is the next prompt.
    async fn on_message(&self, http: &Http, msg: &Message) -> Result<()> {
        if msg.author.bot || !self.allowed.contains(&msg.author.id) {
            return Ok(());
        }
        let Ok(cell) = self.task_in(msg.channel_id) else { return Ok(()) };
        let text = std::iter::once(msg.content.clone())
            .chain(msg.attachments.iter().map(|a| format!("attachment: {}", a.url)))
            .collect::<Vec<_>>()
            .join("\n");
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let mut t = cell.lock().await;
        if t.phase == Phase::Gone {
            msg.reply(http, "agent exited, /attach a running one or /new").await?;
            return Ok(());
        }
        let m = self.machine(&t.machine)?;
        if let Err(e) = herdr::prompt(m, &t.pane, text).await {
            if e.is(&herdr::Code::AgentBlocked) {
                let content = screen(m, &t.pane, "answer the dialog first, then resend\n").await;
                drop(t);
                let out = CreateMessage::new().content(content).components(key_rows()).reference_message(msg);
                msg.channel_id.send_message(http, out).await?;
                return Ok(());
            }
            if e.is_busy() {
                if t.queued.is_some() {
                    drop(t);
                    msg.reply(http, "one message already queued; send again after it runs").await?;
                    return Ok(());
                }
                t.queued = Some(text.into());
                drop(t);
                self.save().await;
                msg.react(http, emoji("👀")).await?;
                msg.reply(http, "queued — sends when the agent is idle").await?;
                return Ok(());
            }
            return Err(e.into());
        }
        // a pending first prompt or journal entry keeps its phase; its reply covers this one too
        if t.phase == Phase::Idle {
            t.phase = Phase::Waiting;
        }
        t.prompt_msg = Some(msg.id);
        drop(t);
        self.save().await;
        msg.react(http, emoji("👀")).await?;
        Ok(())
    }

    // ── events: herdr pushes status per pane; agent.list only on (re)connect ──

    async fn reconcile(&self, http: &Http, machine: &str) {
        let Ok(m) = self.machine(machine) else { return };
        let Ok(list) = herdr::agents(m).await else { return }; // unreachable, the watcher retries
        for (id, cell) in self.cells() {
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
            self.run(http, id, &cell, seen).await;
        }
        self.save().await;
    }

    async fn on_push(&self, http: &Http, machine: &str, push: socket::Push) {
        let (pane, status) = match push {
            socket::Push::Status(pane, s) if s != Status::Unknown => (pane, Some(s)),
            socket::Push::Gone(pane) => (pane, None),
            socket::Push::Status(pane, _) | socket::Push::Detected(pane) => {
                if self.live(machine, &pane).await.is_some() {
                    self.reconcile(http, machine).await; // the agent may have left or restarted
                }
                return;
            }
            socket::Push::Reconcile => return self.reconcile(http, machine).await,
            socket::Push::Lost => return,
        };
        let Some(id) = self.live(machine, &pane).await else { return };
        let Ok(cell) = self.task_in(id) else { return };
        if status.is_some() && cell.lock().await.session.is_none() {
            return self.reconcile(http, machine).await; // learn the session from agent.list
        }
        self.run(http, id, &cell, status.map_or(Seen::Gone, |s| Seen::Agent(s, None))).await;
        self.save().await;
    }

    async fn run(&self, http: &Http, id: ChannelId, cell: &Cell, seen: Seen) {
        match self.step(http, id, cell, seen).await {
            Ok(Step::Keep) => {}
            Ok(Step::Drop) => self.remove(id),
            Err(e) => eprintln!("task {id}: {e}"),
        }
    }

    // One task's turn.
    async fn step(&self, http: &Http, th: ChannelId, cell: &Cell, seen: Seen) -> Result<Step> {
        let mut t = cell.lock().await;
        let Ok(m) = self.machine(&t.machine) else { return Ok(Step::Keep) }; // a machine this host doesn't drive
        if t.phase == Phase::Gone {
            return Ok(Step::Keep);
        }
        if let Err(e) = th.to_channel(http).await {
            return if not_found(&e) { Ok(Step::Drop) } else { Err(e.into()) };
        }
        let Seen::Agent(now, session) = seen else {
            t.phase = Phase::Gone;
            th.say(http, "agent exited").await?;
            return Ok(Step::Keep);
        };

        let prev = t.status.replace(now);
        if let Some(s) = session {
            t.session = Some(s);
        }

        let blocked = Some(Status::Blocked);
        if now == Status::Blocked && prev != blocked {
            t.dialog_msg = Some(th.send_message(http, screen_msg(m, &t.pane).await).await?.id);
            mark(http, th, &t, "⏸️").await;
        } else if prev == blocked && now != Status::Blocked {
            if let Some(d) = t.dialog_msg.take() {
                let _ = th.delete_message(http, d).await;
            }
            mark(http, th, &t, "👀").await;
        }

        if !now.ready() {
            return Ok(Step::Keep);
        }
        if let Phase::Starting { prompt } = &t.phase {
            let prompt = prompt.clone();
            if let Some(s) = &t.session {
                t.last_reply = herdr::reply(m, s).await.map(|r| r.id);
            }
            t.phase = match herdr::prompt(m, &t.pane, &prompt).await {
                Ok(()) => Phase::Waiting,
                Err(e) => {
                    th.say(http, format!("first prompt failed: {e}")).await?;
                    Phase::Idle
                }
            };
            return Ok(Step::Keep);
        }

        let was_working = prev == Some(Status::Working);
        if !(t.phase.waiting() || was_working) {
            return self.drain_queue(http, th, &mut t, m).await;
        }
        if t.kind == "claude" {
            let Some(s) = t.session.clone() else { return Ok(Step::Keep) };
            let Some(r) = fresh_reply(m, &s, t.last_reply.as_deref()).await else { return Ok(Step::Keep) };
            t.last_reply = Some(r.id);
            mark(http, th, &t, "✅").await;
            if let Phase::Ending(teardown) = t.phase {
                self.finish(http, th, &t, teardown, &r.text).await?;
                return Ok(Step::Drop);
            }
            t.phase = Phase::Idle;
            post(http, th, &r.text).await?;
        } else if was_working {
            // other agents have no transcript to read; the screen is the reply
            if t.phase == Phase::Waiting {
                t.phase = Phase::Idle;
            }
            mark(http, th, &t, "✅").await;
            let screen = herdr::screen(m, &t.pane, 80).await?;
            post(http, th, &format!("```\n{}\n```", tail(&screen, 5000))).await?;
        }
        self.drain_queue(http, th, &mut t, m).await
    }

    // One Discord message may wait while the agent works; send it once idle.
    async fn drain_queue(&self, http: &Http, th: ChannelId, t: &mut Task, m: &Machine) -> Result<Step> {
        if t.phase != Phase::Idle {
            return Ok(Step::Keep);
        }
        let Some(text) = t.queued.take() else {
            return Ok(Step::Keep);
        };
        match herdr::prompt(m, &t.pane, &text).await {
            Ok(()) => {
                t.phase = Phase::Waiting;
                mark(http, th, t, "👀").await;
            }
            Err(e) if e.is_busy() || e.is(&herdr::Code::AgentBlocked) => {
                t.queued = Some(text); // still not ready; try next tick
            }
            Err(e) => {
                th.say(http, format!("queued prompt failed: {e}")).await?;
            }
        }
        Ok(Step::Keep)
    }
}

enum Step {
    Keep,
    Drop, // the task is over or its thread is gone
}

// What herdr says about a task's pane.
enum Seen {
    Agent(Status, Option<SessionId>),
    Gone,
}

// The transcript can trail the status flip by a moment, so look again briefly.
async fn fresh_reply(m: &Machine, s: &SessionId, last: Option<&str>) -> Option<herdr::Reply> {
    for _ in 0..4 {
        if let Some(r) = herdr::reply(m, s).await.filter(|r| Some(r.id.as_str()) != last) {
            return Some(r);
        }
        sleep(Duration::from_millis(250)).await;
    }
    None
}

// ── wiring ─────────────────────────────────────────────────

struct Handler(Arc<App>);

macro_rules! fail {
    ($i:expr, $http:expr, $content:expr) => {
        if $i.create_response($http, ephemeral($content.clone())).await.is_err() {
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
                Interaction::Command(c) => drop(c.create_response(http, ephemeral("not allowed")).await),
                Interaction::Component(c) => drop(c.create_response(http, ephemeral("not allowed")).await),
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
        let app = self.0.clone();
        if app.started.swap(true, Ordering::SeqCst) {
            return; // a reconnect, the poller already runs
        }
        if let Err(e) = app.guild.set_commands(&ctx.http, app.commands()).await {
            eprintln!("register commands: {e}");
        }
        let names: Vec<&str> = app.machines().map(|m| m.name.as_str()).collect();
        println!("drover ready as {} on {}", ready.user.tag(), names.join(", "));
        let http = ctx.http.clone();
        let machines: Vec<Machine> = app.machines().cloned().collect();
        for m in machines {
            let app = app.clone();
            let http = http.clone();
            tokio::spawn(async move {
                watch_machine(app, http, m).await;
            });
        }
    }
}

async fn watch_machine(app: Arc<App>, http: Arc<Http>, m: Machine) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match watch_session(&app, &http, &m).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                eprintln!("events {}: {e}", m.name);
                sleep(backoff).await;
                backoff = (backoff.saturating_mul(2)).min(Duration::from_secs(30));
            }
        }
    }
}

async fn watch_session(app: &App, http: &Http, m: &Machine) -> Result<()> {
    let watched = loop {
        let panes = app.panes_on(&m.name).await;
        if !panes.is_empty() {
            break panes;
        }
        tokio::select! {
            () = app.wake.notified() => {}
            () = sleep(Duration::from_secs(5)) => {}
        }
    };

    // herdr rejects a subscription naming a pane that is gone; reconcile marks those
    let sub: Vec<PaneId> = match herdr::panes(m).await {
        Ok(live) => watched.iter().filter(|p| live.contains(p)).cloned().collect(),
        Err(_) => watched.clone(),
    };
    let mut conn = socket::Events::subscribe(m.ssh(), &sub).await?;
    // subscribe first, then read, so nothing between the two is missed
    app.reconcile(http, &m.name).await;

    loop {
        tokio::select! {
            ev = conn.recv() => match ev? {
                socket::Push::Lost => bail!("events_lost on {}", m.name),
                push => app.on_push(http, &m.name, push).await,
            },
            () = app.wake.notified() => {}
        }
        // checked after every push too: a wake while busy above is not stored
        if app.panes_on(&m.name).await != watched {
            return Ok(()); // resubscribe with the new pane set
        }
    }
}

fn on_path(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file())
}

// herdr-wt comes from the dotfiles and needs the rest.
fn require_runtime() -> Result<()> {
    let missing: Vec<_> = ["herdr", "herdr-wt", "git", "jq", "ssh"].into_iter().filter(|cmd| !on_path(cmd)).collect();
    if !missing.is_empty() {
        bail!("missing on PATH: {}", missing.join(", "));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    require_runtime()?;
    let app = Arc::new(App::load()?);
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(&app.token, intents).event_handler(Handler(app.clone())).await?;
    client.start().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_typescript_state() -> Result<()> {
        let old = r#"{"123": {"machine":"meerkat","pane":"p1","workspace":"w1","kind":"claude","repo":"r",
            "branch":"theo/x","owned":true,"status":"idle","expecting":true,"ending":{"remove":true},
            "session":"not-a-uuid","promptMsg":"456"}}"#;
        let tasks: BTreeMap<ChannelId, Task> = serde_json::from_str(old)?;
        let t = tasks.get(&ChannelId::new(123)).context("task")?;
        assert!(t.phase == Phase::Ending(Teardown::Remove));
        assert!(t.origin == Origin::Opened && t.session.is_none());
        assert_eq!(t.prompt_msg, Some(MessageId::new(456)));

        let back: BTreeMap<ChannelId, Task> = serde_json::from_str(&serde_json::to_string(&tasks)?)?;
        assert!(back.get(&ChannelId::new(123)).is_some_and(|t| t.phase == Phase::Ending(Teardown::Remove)));
        Ok(())
    }

    #[test]
    fn helpers() {
        assert_eq!(slug("Theo/Fix-Bug!!x"), "theo-fix-bug-x");
        assert_eq!(home("/Users/theo/Developer/x"), "~/Developer/x");
        assert_eq!(home("/home/theo"), "~");
        assert_eq!(home("/opt/x"), "/opt/x");
        assert_eq!(tail("abcdef", 3), "…def");
        assert_eq!(today().len(), 10);
        assert_eq!(unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix("2026-10-02T22:50:00.185835+00:00"), Some(1_790_981_400));
        assert_eq!(unix("2024-02-29T12:00:00Z"), Some(1_709_208_000));
    }
}
