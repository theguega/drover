// The service: what each command does to the runs. A run's events come from its harness: the
// stream of a headless process, or the hooks a pane's agent posts over HTTP. herdr reports only
// what hooks cannot, a pane that closed. Nothing polls.

use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex, MutexGuard, PoisonError, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{Notify, broadcast, mpsc, oneshot, watch},
    time::sleep,
};

use crate::{
    Result,
    cli::{Cmd, GoalOp, Ls, Start},
    config::{Config, Paths},
    control::{self, Failure},
    goal::{Graph, Update},
    harness::{self, Abort, Answer, Decision, Harness, Ident, Out, Parse, StartSpec},
    herdr::{self, Code, Host, Name, PaneId, Reach, SessionId},
    http, migrate,
    run::{Machine, Mode, Owner, Phase, Profile, Role, Run, RunId, Usage, now},
    socket::{self, SSH_OPTS},
    store::{Kind, Store, c9},
};

// Events of ended runs older than this, 30 days, move to events.jsonl in the state dir.
const ARCHIVE_AFTER: i64 = 30 * 86_400;
// Characters of a reply its event keeps.
const REPLY_EXCERPT: usize = 300;
// A headless process gets this long between SIGTERM and SIGKILL.
const GRACE: Duration = Duration::from_secs(5);
// Text deltas of a headless turn go out at most this often.
const TEXT_BATCH: Duration = Duration::from_millis(500);
// What a tool call hears when you have not answered before its hook times out (SPIKE.md q2).
const AWAITING: &str = "awaiting approval: the user has not answered yet. Do not retry this call; you will be told when it is approved.";
// What a tool call from a session that is not the run's hears.
const FOREIGN: &str = "drover: this session is not the run's";
// What a tool call of a run that has ended hears.
const ENDED: &str = "the run has ended";
// A call drover denied stays approvable this long, and a run keeps at most EXPIRED_MAX of them.
const EXPIRED_FOR: Duration = Duration::from_secs(86_400);
const EXPIRED_MAX: usize = 16;
// Appended to a crewmate's first prompt: the end of each turn is what its mate reads.
const CREW: &str = "You are a crewmate. Your mate reads the end of each of your turns. The brief above is your only source of facts: do not assume what it does not say. When blocked or unsure, stop and end your turn with a line starting QUESTION:. When the check in the brief passes, end your turn with DONE: and its output. Do not change the check. Do not end your turn while a check or background job you started still runs: wait for it, or end with a line starting RUNNING: and what runs.";
// Appended to a mate's first prompt, where the dispatch playbook is the mates skill.
const MATE: &str = "You are a mate: you dispatch crew and write no code. Follow the `mates` skill. Your run id is $DROVER_RUN.";
// The reader profile's cwd holds only this.
const READER_AGENTS: &str = "You are a reader. You can read files and run `brain find`, `brain get`, `brain pages` and `brain code`. You cannot change anything, and you have no network. Answer from what you read.\n";

// A headless process, while it is warm.
struct Proc {
    id: u64,
    machine: Machine,
    stdin: mpsc::UnboundedSender<String>,
    busy: bool, // a turn runs
    idle_since: Instant,
    // its pid and start time, from its first stderr line
    ident: watch::Receiver<Option<Ident>>,
    // dropping it SIGKILLs the process: only once `stop` has ended it
    _child: tokio::process::Child,
}

// A tool call waiting on an answer.
struct Waiter {
    tx: oneshot::Sender<Decision>,
    tool: String,
    input: Value,
}

type Key = (RunId, String);

pub struct App {
    me: Weak<App>,
    hosts: Vec<Host>, // this machine first
    local: Machine,
    store: Store,
    // exported to local agents, so their drover calls reach this service
    state: PathBuf,
    port: u16,
    // this binary, which a harness's hook commands run
    exe: String,
    hook_timeout: u64,
    idle_kill: Duration,
    // the pane set changed: watchers wake
    wake: Notify,
    // one run's change at a time: a hook, a stream line, a command
    turns: tokio::sync::Mutex<()>,
    procs: StdMutex<HashMap<RunId, Proc>>,
    next_proc: AtomicU64,
    waiting: StdMutex<HashMap<Key, Waiter>>,
    // runs whose next hook may bring a new session: a pane drover resumed, or one after /clear
    rekey: StdMutex<std::collections::HashSet<RunId>>,
    // answered by drover with "awaiting approval"; yours to approve later, for EXPIRED_FOR
    expired: StdMutex<HashMap<Key, (String, Value, Instant)>>,
    // calls you approved late: allowed once, in the turn your approval starts
    approved: StdMutex<Vec<(RunId, String, Value)>>,
    events: broadcast::Sender<Value>,
    // an event is stored and sent under this, so a stream sees ids in order
    emitting: StdMutex<()>,
}

fn locked<T>(m: &StdMutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub async fn run(paths: &Paths) -> Result<()> {
    require_runtime()?;
    let config = Config::load(&paths.config)?;
    std::fs::create_dir_all(&paths.state)?;
    std::fs::set_permissions(&paths.state, std::fs::Permissions::from_mode(0o700))?;
    // held until the process ends, so the sockets, drover.db and the watchers have one owner
    let _lock = lock(&paths.state)?;
    let db = paths.db();
    let store = Store::open(&db)?;
    if let Some(v1) = &config.v1_state
        && let Some(n) = migrate::migrate(&store, v1, now())
            .map_err(|e| err!("migrate {}: {e}", v1.display()))?
    {
        println!("migrated {n} runs from {}", v1.display());
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", config.http_port))
        .await
        .map_err(|e| err!("bind 127.0.0.1:{}: {e}", config.http_port))?;
    let app = App::new(&config, store, paths.state.clone());
    app.orphans()?;
    let names: Vec<&str> = app.hosts.iter().map(|h| h.name.as_str()).collect();
    println!(
        "drover serves {} and http://127.0.0.1:{} on {}",
        paths.socket().display(),
        config.http_port,
        names.join(", ")
    );
    let archive = Arc::clone(&app);
    let file = paths.state.join("events.jsonl");
    tokio::spawn(async move {
        loop {
            match archive.store.archive(now() - ARCHIVE_AFTER, &file) {
                Ok(0) => {}
                Ok(n) => println!("archived {n} events to {}", file.display()),
                Err(e) => eprintln!("archive: {e}"),
            }
            sleep(Duration::from_secs(86_400)).await;
        }
    });
    for h in app.hosts.clone() {
        if h.reach == Reach::Ssh {
            tokio::spawn(tunnel(h.name.clone(), config.http_port));
        }
        let app = Arc::clone(&app);
        tokio::spawn(async move { watch_machine(&app, &h).await });
    }
    tokio::spawn(http::serve(Arc::clone(&app), listener, config.http_port));
    control::serve(app, &paths.socket()).await
}

// One service per state dir. The OS drops the lock with its process, so a crash leaves none behind.
fn lock(state: &Path) -> Result<std::fs::File> {
    let file = std::fs::File::create(state.join("drover.lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Failure::new(
            format!("drover serve already runs on {}", state.display()),
            "stop that one first, or give this one another --state-dir",
        )
        .into()),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn require_runtime() -> Result<()> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let missing: Vec<_> = ["herdr", "drover-wt", "git", "jq", "ssh"]
        .into_iter()
        .filter(|cmd| !std::env::split_paths(&path).any(|dir| dir.join(cmd).is_file()))
        .collect();
    if !missing.is_empty() {
        bail!("missing on PATH: {}", missing.join(", "));
    }
    Ok(())
}

fn not_implemented() -> crate::Error {
    Failure::new("not implemented", "a later drover v2 node builds this").into()
}

#[must_use]
fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// An event as `wait` prints it, one flat row: what the run said or asked.
fn wait_row(e: &Value) -> Value {
    let text = |k: &str| {
        e.pointer(&format!("/data/{k}"))
            .cloned()
            .unwrap_or_default()
    };
    let said = ["text", "escalation", "error", "by"]
        .iter()
        .map(|k| text(k))
        .find(|v| !v.is_null())
        .unwrap_or_default();
    json!({
        "run": e.get("run"), "kind": e.get("kind"), "text": said,
        "perm": text("perm"), "tool": text("tool"),
        "input": e.pointer("/data/input").map_or(Value::Null, |i| call(i).to_string().into()),
    })
}

// A run as `ls` lists it.
fn row(r: &Run) -> Value {
    json!({
        "run": r.id, "machine": r.machine, "role": r.role, "phase": r.phase,
        "harness": r.harness, "mode": r.mode.as_str(), "cwd": r.cwd,
    })
}

// A tool call's input as a late approval matches it: the model rewrites a Bash call's description.
fn call(input: &Value) -> Value {
    let mut v = input.clone();
    if let Some(m) = v.as_object_mut() {
        m.remove("description");
    }
    v
}

#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub run: Option<RunId>,
    pub role: Option<Role>,
    pub since: Option<i64>,
}

// One reader of the event stream: stored events after its cursor, then live ones. A reader that
// falls behind the channel reads the store again, so it misses nothing.
pub struct Feed {
    rx: broadcast::Receiver<Value>,
    last: i64,
    stored: bool,
    filter: Filter,
}

impl Feed {
    pub fn new(app: &App, filter: Filter) -> Result<Self> {
        // subscribed before the cursor is read, so nothing falls between the two
        let rx = app.events.subscribe();
        let (last, stored) = match filter.since {
            Some(s) => (s, true),
            None => (app.store.last_event()?, false),
        };
        Ok(Self {
            rx,
            last,
            stored,
            filter,
        })
    }

    pub async fn next(&mut self, app: &App) -> Result<Vec<Value>> {
        loop {
            if self.stored {
                let f = &self.filter;
                let events = app.store.events_after(self.last, f.run.as_ref(), f.role)?;
                if let Some(id) = events.last().and_then(|e| e.get("id")?.as_i64()) {
                    self.last = id;
                    return Ok(events);
                }
                self.stored = false;
            }
            match self.rx.recv().await {
                Ok(e) => {
                    let id = e.get("id").and_then(Value::as_i64).unwrap_or_default();
                    if id <= self.last {
                        continue;
                    }
                    self.last = id;
                    let run =
                        self.filter.run.as_ref().is_none_or(|r| {
                            e.get("run").and_then(Value::as_str) == Some(r.as_str())
                        });
                    let role = self
                        .filter
                        .role
                        .is_none_or(|r| e.get("role").and_then(Value::as_str) == Some(r.as_str()));
                    if run && role {
                        return Ok(vec![e]);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => self.stored = true,
                Err(broadcast::error::RecvError::Closed) => bail!("drover is stopping"),
            }
        }
    }
}

impl App {
    fn new(config: &Config, store: Store, state: PathBuf) -> Arc<Self> {
        let root = config.repos_root.clone();
        let host = Host::new(config.host.to_string(), Reach::Local, root.clone());
        let remotes = config
            .remotes
            .iter()
            .map(|m| Host::new(m.to_string(), Reach::Ssh, root.clone()));
        Self::with(
            std::iter::once(host).chain(remotes).collect(),
            config,
            store,
            state,
        )
    }

    fn with(hosts: Vec<Host>, config: &Config, store: Store, state: PathBuf) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: Weak::clone(me),
            hosts,
            local: config.host.clone(),
            store,
            state,
            port: config.http_port,
            exe: std::env::current_exe()
                .map_or_else(|_| "drover".into(), |p| p.to_string_lossy().into_owned()),
            hook_timeout: config.hook_timeout,
            idle_kill: Duration::from_secs(config.idle_kill),
            wake: Notify::new(),
            turns: tokio::sync::Mutex::new(()),
            procs: StdMutex::default(),
            next_proc: AtomicU64::new(1),
            waiting: StdMutex::default(),
            rekey: StdMutex::default(),
            expired: StdMutex::default(),
            approved: StdMutex::default(),
            events: broadcast::channel(1024).0,
            emitting: StdMutex::new(()),
        })
    }

    // Headless processes end with the service, and their runs wait for the next send, which
    // resumes them. One the service left running (it was killed) is ended now, and before a resume.
    fn orphans(&self) -> Result<()> {
        if !self.store.procs(None)?.is_empty() {
            let me = Weak::clone(&self.me);
            tokio::spawn(async move {
                if let Some(me) = me.upgrade()
                    && let Err(e) = me.reap(None).await
                {
                    eprintln!("processes drover left: {e}");
                }
            });
        }
        for mut r in self.store.list_open()? {
            if r.mode != Mode::Headless || !r.phase.live() || r.phase == Phase::Idle {
                continue;
            }
            match r.session {
                Some(_) => {
                    r.phase = Phase::Idle;
                    self.store.save(&r)?;
                }
                None => self.end(
                    &mut r,
                    Phase::Failed,
                    Kind::Failed,
                    json!({ "error": "drover stopped before the run had a session" }),
                )?,
            }
        }
        Ok(())
    }

    fn host(&self, m: &Machine) -> Result<&Host> {
        self.hosts
            .iter()
            .find(|h| h.name == m.as_str())
            .ok_or_else(|| {
                Failure::new(
                    format!("unknown machine {m}"),
                    "add it to remotes in config.toml",
                )
                .into()
            })
    }

    fn get(&self, id: &RunId) -> Result<Run> {
        self.store
            .get(id)?
            .ok_or_else(|| Failure::new(format!("no run {id}"), "drover ls --all").into())
    }

    fn harness(r: &Run) -> Result<&'static dyn Harness> {
        harness::get(r.harness).ok_or_else(not_implemented)
    }

    fn save(&self, r: &Run) -> Result<()> {
        self.store.save(r)
    }

    // A tool call of the run waits on an answer.
    fn has_waiting(&self, id: &RunId) -> bool {
        locked(&self.waiting).keys().any(|(run, _)| run == id)
    }

    // Stores the event and streams it; `requested` is only stored.
    fn emit(&self, r: &Run, kind: Kind, data: Value) -> Result<()> {
        let _order = locked(&self.emitting);
        let (id, at) = self.store.event(&r.id, kind, &data)?;
        if kind != Kind::Requested {
            let _ = self.events.send(c9(id, r, at, kind, &data));
        }
        Ok(())
    }

    fn end(&self, r: &mut Run, phase: Phase, kind: Kind, data: Value) -> Result<()> {
        r.phase = phase;
        r.ended_at = Some(now());
        let data = match (kind, data) {
            (Kind::Done, Value::Object(mut d)) => {
                d.insert("usage".into(), json!(r.usage));
                Value::Object(d)
            }
            (_, d) => d,
        };
        self.emit(r, kind, data)?;
        self.save(r)?;
        self.forget_calls(&r.id);
        self.restore_hooks(r);
        let proc = locked(&self.procs).remove(&r.id);
        if let (Some(p), Some(me)) = (proc, self.me.upgrade()) {
            let id = r.id.clone();
            tokio::spawn(async move { me.stop(&id, p).await });
        }
        self.wake.notify_waiters();
        Ok(())
    }

    // An ended run's calls: a waiting one hears ENDED, and nothing of it stays approvable.
    fn forget_calls(&self, id: &RunId) {
        let waiting: Vec<Waiter> = {
            let mut waiting = locked(&self.waiting);
            let keys: Vec<Key> = waiting
                .keys()
                .filter(|(run, _)| run == id)
                .cloned()
                .collect();
            keys.iter().filter_map(|k| waiting.remove(k)).collect()
        };
        for w in waiting {
            let _ = w.tx.send(Decision {
                allow: false,
                reason: Some(ENDED.into()),
            });
        }
        locked(&self.expired).retain(|(run, _), _| run != id);
        locked(&self.approved).retain(|(run, ..)| run != id);
    }

    // Keeps a call drover denied approvable, within EXPIRED_FOR and EXPIRED_MAX per run.
    fn expire(&self, key: Key, tool: String, input: Value) {
        let mut expired = locked(&self.expired);
        expired.retain(|_, (.., at)| at.elapsed() < EXPIRED_FOR);
        let mut mine: Vec<(Key, Instant)> = expired
            .iter()
            .filter(|((run, _), _)| *run == key.0)
            .map(|(k, (.., at))| (k.clone(), *at))
            .collect();
        mine.sort_by_key(|(_, at)| *at);
        for (old, _) in mine
            .iter()
            .take((mine.len() + 1).saturating_sub(EXPIRED_MAX))
        {
            expired.remove(old);
        }
        expired.insert(key, (tool, input, Instant::now()));
    }

    // Takes the run's process out and ends it.
    async fn retire(&self, id: &RunId) {
        let proc = locked(&self.procs).remove(id);
        if let Some(p) = proc {
            self.stop(id, p).await;
        }
    }

    // SIGTERM, then SIGKILL after GRACE. Dropping the child alone would SIGKILL claude and leave
    // the tools it started running, in sessions of their own.
    async fn stop(&self, id: &RunId, mut p: Proc) {
        let ident = match tokio::time::timeout(GRACE, p.ident.wait_for(Option::is_some)).await {
            Ok(Ok(i)) => i.clone(),
            _ => None,
        };
        if let Some(i) = ident
            && let Err(e) = self.end_proc(id, &p.machine, &i).await
        {
            eprintln!("run {id}: stop {}: {e}", i.pid);
        }
    }

    // Ends the recorded processes of a run, or of all, that no warm Proc holds: the ones a
    // service that was killed left running.
    async fn reap(&self, run: Option<&RunId>) -> Result<()> {
        for (id, machine, ident) in self.store.procs(run)? {
            self.end_proc(&id, &machine, &ident).await?;
        }
        Ok(())
    }

    // Ends a recorded process if it still runs, and forgets it.
    async fn end_proc(&self, id: &RunId, machine: &Machine, ident: &Ident) -> Result<()> {
        harness::terminate(self.host(machine)?.ssh(), ident, GRACE).await?;
        self.store.forget(id, ident)
    }

    fn hook_url(&self, run: &RunId) -> String {
        format!("http://127.0.0.1:{}/hooks/{run}", self.port)
    }

    // What a run was asked: repo, branch, prompt, model.
    fn asked(&self, r: &Run) -> Result<Value> {
        Ok(self.store.last(&r.id, Kind::Requested)?.unwrap_or_default())
    }

    // Live pane runs on a machine, with their pane.
    fn live_on(&self, machine: &str) -> Result<Vec<(Run, PaneId)>> {
        Ok(self
            .store
            .list_open()?
            .into_iter()
            .filter(|r| r.machine.as_str() == machine && r.phase.live())
            .filter_map(|r| r.mode.pane().cloned().map(|p| (r, p)))
            .collect())
    }

    pub async fn act(&self, from: Option<RunId>, cmd: Cmd) -> Result<Value> {
        let from = from.as_ref();
        match cmd {
            Cmd::Run(s) => self.cmd_run(from, s).await,
            Cmd::Ls(l) => self.cmd_ls(&l),
            Cmd::Show { run } => self.cmd_show(&run),
            Cmd::Send { run, text } => self.cmd_send(from, &run, &text).await,
            Cmd::Approve { run, perm } => self.answer(from, &run, &perm, true, None).await,
            Cmd::Deny { run, perm, reason } => self.answer(from, &run, &perm, false, reason).await,
            Cmd::ApproveRun { run } => self.cmd_approve_run(from, &run).await,
            Cmd::DropRun { run } => self.cmd_drop_run(from, &run).await,
            Cmd::Attach { run } => self.cmd_attach(from, &run).await,
            Cmd::Screen { run, lines } => self.cmd_screen(&run, lines).await,
            Cmd::Keys { run, keys } => self.cmd_keys(from, &run, &keys).await,
            Cmd::Stop { run } => self.cmd_stop(from, &run).await,
            Cmd::Done { run, remove, entry } => {
                self.cmd_done(from, &run, remove, entry.as_deref()).await
            }
            Cmd::Snapshot { all } => self.snapshot(all),
            Cmd::Home => self.home(),
            Cmd::Wait {
                any: _,
                parent,
                timeout,
            } => self.cmd_wait(from, parent, timeout).await,
            Cmd::Goal { op } => self.cmd_goal(from, op),
            Cmd::Escalate { text } => self.cmd_escalate(from, &text).await,
            Cmd::Watch { .. } | Cmd::Serve | Cmd::Hook { .. } => bail!("not a request"),
        }
    }

    async fn cmd_run(&self, from: Option<&RunId>, s: Start) -> Result<Value> {
        let h = harness::get(s.harness).ok_or_else(not_implemented)?;
        // a reader acts on nothing, so it needs no pane to pair in
        let headless = s.headless || s.profile == Profile::Reader;
        if s.scratch || (headless && !h.caps().headless) {
            return Err(not_implemented());
        }
        let machine = s.machine.unwrap_or_else(|| self.local.clone());
        self.host(&machine)?;
        let (repo, branch) = match (s.profile, &s.repo, &s.branch) {
            (Profile::Actor, Some(repo), Some(branch)) => {
                (Name::parse(repo)?, Name::parse(branch)?)
            }
            (Profile::Actor, ..) => {
                return Err(Failure::new(
                    "an actor runs in a worktree",
                    "drover run --repo <repo> --branch <branch> …",
                )
                .into());
            }
            (Profile::Reader, None, None) if machine == self.local => {
                (Name::parse("reader")?, Name::parse("reader")?)
            }
            (Profile::Reader, ..) => {
                return Err(Failure::new(
                    "a reader runs on this machine, in the state dir's reader/",
                    "drover run --profile reader --prompt <prompt>, without --machine, --repo or --branch",
                )
                .into());
            }
        };
        // a run that asks for a run is its parent, and the new run waits for you
        let parent = match (from, s.parent) {
            (Some(f), Some(p)) if &p != f => {
                return Err(Failure::new(
                    format!("run {f} cannot ask for a run on behalf of {p}"),
                    "drover run without --parent",
                )
                .into());
            }
            (Some(f), _) => Some(f.clone()),
            (None, p) => p,
        };
        if let Some(p) = &parent {
            self.get(p)?;
        }
        let pending = parent.is_some();
        let role = match (parent.is_some(), s.role) {
            (true, None | Some(Role::Crew)) => Role::Crew,
            (true, Some(r)) => {
                return Err(Failure::new(
                    format!("a run asked for by a run is crew, not {}", r.as_str()),
                    "drover run without --role",
                )
                .into());
            }
            (false, None | Some(Role::Task)) => Role::Task,
            (false, Some(Role::Mate)) => Role::Mate,
            (false, Some(r)) => {
                return Err(Failure::new(
                    format!("--role {} is not something you start", r.as_str()),
                    "drover run --role mate, or without --role",
                )
                .into());
            }
        };
        let id = loop {
            let id = RunId::random();
            if self.store.get(&id)?.is_none() {
                break id;
            }
        };
        let mut run = Run {
            id,
            machine,
            cwd: PathBuf::new(),
            harness: s.harness,
            mode: if headless {
                Mode::Headless
            } else {
                Mode::Pane { pane: None }
            },
            role,
            owner: parent.clone().map_or(Owner::User, Owner::Run),
            parent,
            profile: s.profile,
            session: None,
            phase: if pending {
                Phase::Pending
            } else {
                Phase::Starting
            },
            usage: Usage::default(),
            created_at: now(),
            ended_at: None,
        };
        let prompt = match run.role {
            Role::Crew => format!("{}\n\n{CREW}", s.prompt),
            Role::Mate => format!("{}\n\n{MATE}", s.prompt),
            _ => s.prompt,
        };
        self.save(&run)?;
        self.emit(
            &run,
            Kind::Requested,
            json!({ "repo": repo.as_str(), "branch": branch.as_str(), "prompt": prompt, "model": s.model }),
        )?;
        if !pending {
            self.start(&mut run).await?;
            run = self.get(&run.id)?;
        }
        Ok(self.brief(&run))
    }

    // Opens the run's cwd and starts its agent with the first prompt.
    async fn start(&self, run: &mut Run) -> Result<()> {
        run.phase = Phase::Starting;
        self.save(run)?;
        let opened = match run.mode {
            Mode::Headless => self.open_headless(run).await,
            Mode::Pane { .. } => self.open_pane(run).await,
        };
        if let Err(e) = opened {
            // hooks may have moved it on meanwhile
            let _turn = self.turns.lock().await;
            let mut r = self.get(&run.id)?;
            self.end(
                &mut r,
                Phase::Failed,
                Kind::Failed,
                json!({ "error": e.to_string() }),
            )?;
            *run = r;
            return Err(e);
        }
        Ok(())
    }

    // The run's worktree, unless another live run or an agent drover does not follow has it.
    async fn worktree(&self, h: &Host, run: &Run) -> Result<herdr::Worktree> {
        let asked = self.asked(run)?;
        let name = |k: &str| Name::parse(asked.get(k).and_then(Value::as_str).unwrap_or_default());
        let (repo, branch) = (name("repo")?, name("branch")?);
        let wt = herdr::worktree(h, &repo, &branch).await?;
        let taken = self.store.list_open()?.into_iter().find(|r| {
            r.id != run.id
                && r.phase.live()
                && r.machine == run.machine
                && (r.mode.pane() == Some(&wt.pane_id) || r.cwd == Path::new(&wt.path))
        });
        if let Some(other) = taken {
            return Err(Failure::new(
                format!("{} is open already, in run {}", branch.as_str(), other.id),
                format!("drover send {} <text>", other.id),
            )
            .into());
        }
        if herdr::agents(h)
            .await?
            .iter()
            .any(|a| a.pane_id == wt.pane_id)
        {
            return Err(Failure::new(
                format!("an agent drover does not follow runs in {}", wt.pane_id.0),
                "close it at the desk, then run again",
            )
            .into());
        }
        Ok(wt)
    }

    fn env<'a>(&'a self, h: &Host, run: &'a RunId, state: &'a str) -> Vec<(&'a str, &'a str)> {
        let mut env = vec![("DROVER_RUN", run.as_str())];
        if h.reach == Reach::Local {
            env.push(("DROVER_STATE_DIR", state));
        }
        env
    }

    fn spec<'a>(
        &'a self,
        r: &'a Run,
        asked: &'a Value,
        hooks: &'a str,
        resume: Option<&'a SessionId>,
    ) -> StartSpec<'a> {
        StartSpec {
            headless: r.mode == Mode::Headless,
            profile: r.profile,
            model: asked.get("model").and_then(Value::as_str),
            resume,
            hooks,
            hook_timeout: self.hook_timeout,
            drover: &self.exe,
        }
    }

    // For a harness that reads its hooks from a file in the cwd: ours, until the run ends.
    fn install_hooks(&self, r: &Run) -> Result<()> {
        let harness = Self::harness(r)?;
        let Some(rel) = harness.hook_file(r.profile) else {
            return Ok(());
        };
        if self.host(&r.machine)?.ssh().is_some() {
            bail!(
                "{} reads its hooks from a file: not on a remote yet",
                r.harness.as_str()
            );
        }
        let (asked, hooks) = (self.asked(r)?, self.hook_url(&r.id));
        let ours = harness.hook_settings(&self.spec(r, &asked, &hooks, None));
        harness::install(&r.cwd, rel, &ours).map_err(|e| err!("{}: {e}", r.cwd.join(rel).display()))
    }

    fn restore_hooks(&self, r: &Run) {
        let Some(rel) = Self::harness(r).ok().and_then(|h| h.hook_file(r.profile)) else {
            return;
        };
        if let Err(e) = harness::restore(&r.cwd, rel, r.id.as_str()) {
            eprintln!("run {}: {e}", r.id);
        }
    }

    async fn open_pane(&self, run: &mut Run) -> Result<()> {
        let h = self.host(&run.machine)?;
        let harness = Self::harness(run)?;
        let wt = self.worktree(h, run).await?;
        run.mode = Mode::Pane {
            pane: Some(wt.pane_id.clone()),
        };
        run.cwd = PathBuf::from(&wt.path);
        self.save(run)?;
        self.install_hooks(run)?;
        self.wake.notify_waiters();
        let (asked, hooks) = (self.asked(run)?, self.hook_url(&run.id));
        let argv = harness.start(&self.spec(run, &asked, &hooks, None));
        let prompt = asked
            .get("prompt")
            .and_then(Value::as_str)
            .map(String::from);
        self.launch(h, run, &wt, &argv, prompt).await
    }

    // Starts the harness's TUI in the pane, then types the first prompt, if any. A dialog at
    // startup (folder trust) is a `needs` until you answer it at the desk. Called without the turns lock.
    async fn launch(
        &self,
        h: &Host,
        r: &Run,
        wt: &herdr::Worktree,
        argv: &[String],
        prompt: Option<String>,
    ) -> Result<()> {
        let state = self.state.to_string_lossy();
        let env = self.env(h, &r.id, &state);
        let args = argv.get(1..).unwrap_or_default();
        let started = herdr::start(
            h,
            r.id.as_str(),
            r.harness.as_str(),
            &wt.pane_id,
            &wt.path,
            &env,
            args,
        )
        .await;
        match started {
            Err(e) if !e.is(&Code::AgentNotReady) => return Err(e.into()),
            _ => {}
        }
        if herdr::wait(h, &wt.pane_id, &["idle", "blocked"]).await? == "idle" {
            return self.ready(&r.id, prompt).await;
        }
        {
            let _turn = self.turns.lock().await;
            let mut r = self.get(&r.id)?;
            r.phase = Phase::Needs;
            self.emit(&r, Kind::Needs, json!({ "dialog": "startup" }))?;
            self.save(&r)?;
        }
        let (me, id, pane, h) = (
            self.me.upgrade(),
            r.id.clone(),
            wt.pane_id.clone(),
            h.clone(),
        );
        tokio::spawn(async move {
            let Some(me) = me else { return };
            let ready = match herdr::wait(&h, &pane, &["idle"]).await {
                Ok(_) => me.ready(&id, prompt).await,
                Err(e) => Err(e.into()),
            };
            if let Err(e) = ready {
                eprintln!("run {id}: after its startup dialog: {e}");
            }
        });
        Ok(())
    }

    // The agent is up: its first prompt goes out, or a run past its startup dialog is idle.
    async fn ready(&self, id: &RunId, prompt: Option<String>) -> Result<()> {
        // under the lock, so the turn's hooks land after it
        let _turn = self.turns.lock().await;
        let mut r = self.get(id)?;
        if !r.phase.live() {
            return Ok(());
        }
        match prompt {
            Some(p) => {
                self.deliver(&mut r, &p).await?;
                self.emit(&r, Kind::Started, json!({}))
            }
            None if r.phase == Phase::Needs => {
                r.phase = Phase::Idle;
                self.save(&r)
            }
            None => Ok(()),
        }
    }

    async fn open_headless(&self, run: &mut Run) -> Result<()> {
        let h = self.host(&run.machine)?;
        run.cwd = match run.profile {
            Profile::Reader => self.reader_dir()?,
            Profile::Actor => {
                let wt = self.worktree(h, run).await?;
                // the run has no pane: the workspace drover-wt opened goes, its checkout stays
                if let Some(ws) = wt.pane_id.workspace() {
                    herdr::close(h, &ws).await?;
                }
                PathBuf::from(wt.path)
            }
        };
        self.save(run)?;
        self.install_hooks(run)?;
        let prompt = self
            .asked(run)?
            .get("prompt")
            .and_then(Value::as_str)
            .map(String::from);
        self.ready(&run.id, prompt).await
    }

    fn reader_dir(&self) -> Result<PathBuf> {
        let dir = self.state.join("reader");
        std::fs::create_dir_all(&dir)?;
        for (file, text) in [("AGENTS.md", READER_AGENTS), ("CLAUDE.md", "@AGENTS.md\n")] {
            let path = dir.join(file);
            if !path.exists() {
                std::fs::write(path, text)?;
            }
        }
        Ok(dir)
    }

    // The next prompt into a run, under the turns lock: typed into its pane, or written to its
    // headless process, which starts again with --resume when it is not warm.
    async fn deliver(&self, r: &mut Run, text: &str) -> Result<()> {
        let harness = Self::harness(r)?;
        match &r.mode {
            Mode::Headless => {
                // a process that took one prompt and ended may not have been seen to yet
                let warm = harness.caps().warm && locked(&self.procs).contains_key(&r.id);
                if !warm && r.session.is_some() && !harness.caps().resume {
                    bail!(
                        "{} cannot resume its session: start a new run",
                        r.harness.as_str()
                    );
                }
                if !warm {
                    let resume = r.session.clone();
                    if resume.is_none() && self.store.last(&r.id, Kind::Started)?.is_some() {
                        bail!("{} has no session to resume", r.id);
                    }
                    // a process of this session still running would be a second one on it
                    self.reap(Some(&r.id)).await?;
                    self.spawn(r, resume.as_ref())?;
                }
                let mut procs = locked(&self.procs);
                let p = procs
                    .get_mut(&r.id)
                    .ok_or_else(|| err!("{} has no process", r.id))?;
                p.stdin
                    .send(harness.send(text))
                    .map_err(|_| err!("{}: its process is gone", r.id))?;
                p.busy = true;
            }
            Mode::Pane { pane: Some(pane) } => {
                herdr::prompt(self.host(&r.machine)?, pane, text).await?
            }
            Mode::Pane { pane: None } => bail!("{} has no pane", r.id),
        }
        r.phase = Phase::Working;
        self.save(r)
    }

    fn spawn(&self, r: &Run, resume: Option<&SessionId>) -> Result<()> {
        let h = self.host(&r.machine)?;
        let harness = Self::harness(r)?;
        let (asked, hooks) = (self.asked(r)?, self.hook_url(&r.id));
        let argv = harness.start(&self.spec(r, &asked, &hooks, resume));
        let state = self.state.to_string_lossy();
        let env = self.env(h, &r.id, &state);
        let sp = harness::spawn(&argv, &r.cwd, &env, h.ssh())
            .map_err(|e| err!("{}: {e}", argv.first().map_or("", String::as_str)))?;
        let id = self.next_proc.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(write_lines(sp.stdin, rx, !harness.caps().warm));
        let (known, ident) = watch::channel(None);
        let me = self
            .me
            .upgrade()
            .ok_or_else(|| err!("drover is stopping"))?;
        tokio::spawn(read_proc(
            me,
            (r.id.clone(), r.machine.clone()),
            id,
            harness,
            sp.stdout,
            (sp.stderr, known),
        ));
        locked(&self.procs).insert(
            r.id.clone(),
            Proc {
                id,
                machine: r.machine.clone(),
                ident,
                stdin: tx,
                busy: false,
                idle_since: Instant::now(),
                _child: sp.child,
            },
        );
        Ok(())
    }

    // A headless process ended. One drover stopped is no news; one that died mid-turn fails its run.
    async fn exited(&self, id: &RunId, proc: u64, stderr: String) {
        let _turn = self.turns.lock().await;
        let gone = {
            let mut procs = locked(&self.procs);
            match procs.get(id) {
                Some(p) if p.id == proc => procs.remove(id),
                _ => None,
            }
        };
        let Some(p) = gone else { return };
        let ident = p.ident.borrow().clone();
        if let Some(i) = ident
            && let Err(e) = self.store.forget(id, &i)
        {
            eprintln!("run {id}: {e}");
        }
        let Ok(mut r) = self.get(id) else { return };
        if r.phase.live() && (p.busy || r.phase != Phase::Idle) {
            let error = format!("the process exited: {}", stderr.trim());
            if let Err(e) = self.end(
                &mut r,
                Phase::Failed,
                Kind::Failed,
                json!({ "error": error }),
            ) {
                eprintln!("run {id}: {e}");
            }
        }
    }

    // Ends a warm process that has had no turn for idle_kill.
    async fn kill_idle(&self, id: &RunId, proc: u64) {
        let idle = {
            let mut procs = locked(&self.procs);
            let idle = procs.get(id).is_some_and(|p| {
                p.id == proc && !p.busy && p.idle_since.elapsed() >= self.idle_kill
            });
            idle.then(|| procs.remove(id)).flatten()
        };
        if let Some(p) = idle {
            self.stop(id, p).await;
        }
    }

    // A harness's events, from its stream or its hooks, moving the run along.
    async fn apply(&self, id: &RunId, outs: Vec<Out>) {
        if outs.is_empty() {
            return;
        }
        let _turn = self.turns.lock().await;
        if let Err(e) = self.apply_locked(id, outs) {
            eprintln!("run {id}: {e}");
        }
    }

    fn apply_locked(&self, id: &RunId, outs: Vec<Out>) -> Result<()> {
        let Some(mut r) = self.store.get(id)?.filter(|r| r.phase.live()) else {
            return Ok(());
        };
        let before = r.clone();
        for out in outs {
            match out {
                Out::Session(s) => r.session = Some(s),
                Out::Rekey => drop(locked(&self.rekey).insert(r.id.clone())),
                Out::Prompt => r.phase = Phase::Working,
                Out::Text(text) => self.emit(&r, Kind::Text, json!({ "text": text }))?,
                Out::ToolStart { tool } => {
                    self.emit(&r, Kind::ToolStart, json!({ "tool": tool }))?
                }
                Out::ToolEnd { tool, ok } => {
                    self.emit(&r, Kind::ToolEnd, json!({ "tool": tool, "ok": ok }))?
                }
                Out::Reply { text, usage, error } => {
                    if let Some(u) = &usage {
                        r.usage += u;
                    }
                    let text = text.trim();
                    let mut data = json!({ "text": head(text, REPLY_EXCERPT), "chars": text.chars().count(), "usage": usage });
                    if error && let Value::Object(d) = &mut data {
                        d.insert("error".into(), true.into());
                    }
                    // an escalation still waits for your answer after the turn that asked it
                    if !(r.phase == Phase::Needs && !self.has_waiting(&r.id)) {
                        r.phase = Phase::Idle;
                    }
                    self.emit(&r, Kind::Reply, data)?;
                    self.idle(&r.id);
                    // a late approval holds for the turn it started, and this one has ended
                    locked(&self.approved).retain(|(run, ..)| *run != r.id);
                }
                Out::End if r.mode != Mode::Headless => {
                    return self.end(&mut r, Phase::Done, Kind::Done, json!({ "by": "exit" }));
                }
                Out::End | Out::Permission { .. } => {}
            }
        }
        if r != before {
            self.save(&r)?;
        }
        Ok(())
    }

    // A headless process's turn ended: it stays warm for `idle_kill`.
    fn idle(&self, id: &RunId) {
        let mut procs = locked(&self.procs);
        let Some(p) = procs.get_mut(id) else { return };
        p.busy = false;
        p.idle_since = Instant::now();
        let (me, id, proc, wait) = (Weak::clone(&self.me), id.clone(), p.id, self.idle_kill);
        tokio::spawn(async move {
            sleep(wait).await;
            if let Some(me) = me.upgrade() {
                me.kill_idle(&id, proc).await;
            }
        });
    }

    // A hook a harness posted for a run: its events, and for a permission, the answer.
    pub async fn hook(&self, id: &RunId, body: &Value) -> Value {
        let Ok(Some(r)) = self.store.get(id) else {
            return json!({});
        };
        let Some(harness) = harness::get(r.harness) else {
            return json!({});
        };
        let mut outs = harness.hook(body);
        let session = outs.iter().find_map(|o| match o {
            Out::Session(s) => Some(s),
            _ => None,
        });
        if !self.own_session(&r, session) {
            eprintln!("run {id}: a hook from another session, ignored");
            let foreign = outs.iter().any(|o| matches!(o, Out::Permission { .. }));
            return match (
                foreign,
                harness.respond(&Decision {
                    allow: false,
                    reason: Some(FOREIGN.into()),
                }),
            ) {
                (true, Answer::Hook(v)) => v,
                _ => json!({}),
            };
        }
        let ask = outs
            .iter()
            .position(|o| matches!(o, Out::Permission { .. }))
            .map(|i| outs.remove(i));
        self.apply(id, outs).await;
        match ask {
            Some(Out::Permission { tool, input }) => {
                let d = self.permission(id, tool, input).await;
                match harness.respond(&d) {
                    Answer::Hook(v) => v,
                    Answer::Stdin(_) => json!({}),
                }
            }
            _ => json!({}),
        }
    }

    // Whether a hook's session is the run's: its own, the first one it has, or a new one where
    // drover expects one (a resumed pane, /clear). The headless stream's own is never asked.
    fn own_session(&self, r: &Run, hook: Option<&SessionId>) -> bool {
        match (&r.session, hook) {
            (None, _) => true,
            (Some(known), Some(s)) if known == s => true,
            (Some(_), Some(_)) => locked(&self.rekey).remove(&r.id),
            (Some(_), None) => false,
        }
    }

    // A tool call that needs a permission: a `needs` event, then the owner's answer. Without one
    // shortly before the hook's timeout, drover denies it with AWAITING and keeps it approvable.
    async fn permission(&self, id: &RunId, tool: String, input: Value) -> Decision {
        let allow = Decision {
            allow: true,
            reason: None,
        };
        let deny = |why: &str| Decision {
            allow: false,
            reason: Some(why.into()),
        };
        let (tx, mut rx) = oneshot::channel();
        let key = {
            let _turn = self.turns.lock().await;
            let Ok(Some(mut r)) = self.store.get(id) else {
                return deny("drover has no such run");
            };
            if !r.phase.live() {
                return deny(ENDED);
            }
            // the session a late approval resumed asks again for the call you approved
            {
                let mut approved = locked(&self.approved);
                let same = approved
                    .iter()
                    .position(|(r, t, i)| r == id && *t == tool && *i == call(&input));
                if let Some(i) = same {
                    approved.remove(i);
                    return allow;
                }
            }
            let perm = match self.store.perms(id) {
                Ok(n) => format!("p{}", n + 1),
                Err(e) => return deny(&format!("drover: {e}")),
            };
            let key = (id.clone(), perm.clone());
            locked(&self.waiting).insert(
                key.clone(),
                Waiter {
                    tx,
                    tool: tool.clone(),
                    input: input.clone(),
                },
            );
            r.phase = Phase::Needs;
            let needs = self
                .emit(
                    &r,
                    Kind::Needs,
                    json!({ "perm": perm, "tool": tool, "input": input }),
                )
                .and_then(|()| self.save(&r));
            if let Err(e) = needs {
                locked(&self.waiting).remove(&key);
                return deny(&format!("drover: {e}"));
            }
            key
        };
        let margin = 10.min(self.hook_timeout / 2);
        let wait = Duration::from_secs(self.hook_timeout - margin);
        if let Ok(Ok(d)) = tokio::time::timeout(wait, &mut rx).await {
            return d;
        }
        let waiter = locked(&self.waiting).remove(&key);
        let Some(w) = waiter else {
            // taken by an answer on its way
            return rx.try_recv().unwrap_or_else(|_| deny(AWAITING));
        };
        self.expire(key, w.tool, w.input);
        // the turn goes on with the deny: nothing waits, unless another call does
        let _turn = self.turns.lock().await;
        let others = locked(&self.waiting).keys().any(|(run, _)| run == id);
        if let Ok(Some(mut r)) = self.store.get(id)
            && r.phase == Phase::Needs
            && !others
        {
            r.phase = Phase::Working;
            if let Err(e) = self.save(&r) {
                eprintln!("run {id}: {e}");
            }
        }
        deny(AWAITING)
    }

    // approve and deny: the answer goes to the waiting call, or, for one drover already denied,
    // an approval goes to the run as its next prompt.
    pub async fn answer(
        &self,
        from: Option<&RunId>,
        id: &RunId,
        perm: &str,
        allow: bool,
        reason: Option<String>,
    ) -> Result<Value> {
        let _turn = self.turns.lock().await;
        let mut r = self.get(id)?;
        Self::owned(&r, from)?;
        if !r.phase.live() {
            return Err(Failure::new(
                format!("{id} is {}: it takes no approvals", r.phase.as_str()),
                format!("drover show {id}"),
            )
            .into());
        }
        let key = (id.clone(), perm.to_string());
        let waiter = locked(&self.waiting).remove(&key);
        if let Some(w) = waiter {
            let others = locked(&self.waiting).keys().any(|(run, _)| run == id);
            if r.phase == Phase::Needs && !others {
                r.phase = Phase::Working;
                self.save(&r)?;
            }
            let _ = w.tx.send(Decision { allow, reason });
            return Ok(
                json!({ "run": id, "perm": perm, "allowed": allow, "help": [format!("drover show {id}")] }),
            );
        }
        let late = locked(&self.expired).remove(&key);
        let Some((tool, input, at)) = late else {
            return Err(Failure::new(
                format!("{id} has no call {perm} waiting"),
                format!("drover show {id}"),
            )
            .into());
        };
        if allow {
            // like send: a prompt typed into a running turn would be queued behind it
            if r.phase != Phase::Idle {
                locked(&self.expired).insert(key, (tool, input, at));
                return Err(Failure::new(
                    format!(
                        "{id} is {}: approve {perm} again once it is idle",
                        r.phase.as_str()
                    ),
                    format!("drover watch --run {id}"),
                )
                .into());
            }
            let text = format!(
                "drover: the user approved the {tool} call that was denied with \"awaiting approval\": {}. Make that call again now.",
                call(&input)
            );
            if let Err(e) = self.deliver(&mut r, &text).await {
                locked(&self.expired).insert(key, (tool, input, at));
                return Err(e);
            }
            // under the turns lock, so the call it asks for again comes after this
            locked(&self.approved).push((id.clone(), tool, call(&input)));
        }
        Ok(
            json!({ "run": id, "perm": perm, "allowed": allow, "late": true, "help": [format!("drover show {id}")] }),
        )
    }

    // What a command that starts or changes a run answers.
    fn brief(&self, r: &Run) -> Value {
        let id = &r.id;
        let help: Vec<String> = match r.phase {
            Phase::Pending => vec![
                format!("drover approve-run {id}"),
                format!("drover drop-run {id}"),
            ],
            Phase::Needs
                if self
                    .store
                    .last(id, Kind::Needs)
                    .ok()
                    .flatten()
                    .is_some_and(|d| d.get("dialog").is_some()) =>
            {
                vec![
                    format!("drover screen {id}"),
                    format!("drover keys {id} <keys>"),
                ]
            }
            Phase::Needs
                if self
                    .store
                    .last(id, Kind::Needs)
                    .ok()
                    .flatten()
                    .is_some_and(|d| d.get("escalation").is_some())
                    && !self.has_waiting(id) =>
            {
                vec![
                    format!("drover show {id}"),
                    format!("drover send {id} <your answer>"),
                ]
            }
            Phase::Needs => vec![
                format!("drover show {id}"),
                format!("drover approve {id} <perm>"),
                format!("drover deny {id} <perm> --reason <why>"),
            ],
            Phase::Idle => vec![
                format!("drover send {id} <text>"),
                format!("drover done {id}"),
            ],
            Phase::Starting | Phase::Working => {
                vec![
                    format!("drover show {id}"),
                    format!("drover watch --run {id}"),
                ]
            }
            Phase::Done | Phase::Failed => vec![format!("drover show {id}")],
        };
        json!({
            "run": id, "machine": r.machine, "phase": r.phase, "mode": r.mode.as_str(), "pane": r.mode.pane().map(|p| &p.0),
            "cwd": r.cwd, "owner": r.owner.to_string(), "help": help,
        })
    }

    fn cmd_ls(&self, l: &Ls) -> Result<Value> {
        let ended = l.all || l.phase.is_some_and(Phase::ended);
        let runs = if ended {
            self.store.list()?
        } else {
            self.store.list_open()?
        };
        let total = self.store.total()?;
        let runs: Vec<Value> = runs
            .iter()
            .filter(|r| l.role.is_none_or(|x| r.role == x))
            .filter(|r| l.machine.as_ref().is_none_or(|m| &r.machine == m))
            .filter(|r| match l.phase {
                Some(p) => r.phase == p,
                None => l.all || !r.phase.ended(),
            })
            .map(row)
            .collect();
        Ok(
            json!({ "runs": runs, "total": total, "help": ["drover show <run>", "drover ls --all"] }),
        )
    }

    // What an escalating run asked you.
    fn ask(&self, id: &RunId) -> Option<Value> {
        self.store
            .last(id, Kind::Needs)
            .ok()
            .flatten()?
            .get("escalation")
            .cloned()
    }

    fn cmd_show(&self, id: &RunId) -> Result<Value> {
        let r = self.get(id)?;
        let children: Vec<RunId> = self.store.children(id)?.into_iter().map(|c| c.id).collect();
        let reply = self
            .store
            .last(id, Kind::Reply)?
            .map(|d| json!({ "text": d.get("text"), "chars": d.get("chars") }));
        let mut perms: Vec<Value> = locked(&self.waiting)
            .iter()
            .filter(|((run, _), _)| run == id)
            .map(|((_, perm), w)| json!({ "perm": perm, "tool": w.tool, "input": call(&w.input).to_string(), "state": "waiting" }))
            .collect();
        perms.extend(
            locked(&self.expired)
                .iter()
                .filter(|((run, _), _)| run == id)
                .map(|((_, perm), (tool, input, _))| json!({ "perm": perm, "tool": tool, "input": call(input).to_string(), "state": "denied, awaiting approval" })),
        );
        let mut out = self.brief(&r);
        let fields = json!({
            "harness": r.harness, "role": r.role, "parent": r.parent,
            "profile": r.profile, "session": r.session.as_ref().map(SessionId::as_str),
            "warm": (r.mode == Mode::Headless).then(|| locked(&self.procs).contains_key(id)),
            "caps": harness::get(r.harness).map(|h| h.caps()),
            "usage": r.usage, "created_at": r.created_at, "ended_at": r.ended_at,
            "children": children, "perms": perms, "reply": reply,
            "ask": (r.phase == Phase::Needs).then(|| self.ask(&r.id)).flatten(),
            "goal": self.store.goal(id)?,
        });
        if let (Value::Object(out), Value::Object(fields)) = (&mut out, fields) {
            // help stays last
            let help = out.remove("help");
            out.extend(fields);
            out.extend(help.map(|h| ("help".to_string(), h)));
        }
        Ok(out)
    }

    // The machine and pane of a pane run whose agent is up.
    fn pane_of(&self, r: &Run) -> Result<(&Host, PaneId)> {
        if r.mode == Mode::Headless {
            return Err(Failure::new(
                format!("{} is headless: it has no pane", r.id),
                format!("drover attach {}", r.id),
            )
            .into());
        }
        let pane = r
            .mode
            .pane()
            .filter(|_| r.phase.live() && r.phase != Phase::Starting);
        let Some(pane) = pane else {
            return Err(Failure::new(
                format!("{} is {}, with no agent up", r.id, r.phase.as_str()),
                format!("drover show {}", r.id),
            )
            .into());
        };
        Ok((self.host(&r.machine)?, pane.clone()))
    }

    fn owned(r: &Run, from: Option<&RunId>) -> Result<()> {
        if r.owner.is(from) {
            return Ok(());
        }
        let help = match from {
            None => format!("drover attach {}", r.id),
            Some(_) => format!("drover show {}", r.id),
        };
        Err(Failure::new(format!("{} is owned by {}", r.id, r.owner), help).into())
    }

    fn by_you(from: Option<&RunId>, what: &str) -> Result<()> {
        match from {
            None => Ok(()),
            Some(r) => Err(Failure::new(
                format!("only you {what}, not run {r}"),
                "drover escalate --text <question>",
            )
            .into()),
        }
    }

    async fn cmd_send(&self, from: Option<&RunId>, id: &RunId, text: &str) -> Result<Value> {
        let _turn = self.turns.lock().await;
        let mut r = self.get(id)?;
        Self::owned(&r, from)?;
        if r.mode != Mode::Headless {
            self.pane_of(&r)?;
        }
        // Claude queues a prompt sent mid-turn, so a turn still running refuses it. A run that
        // escalated and ended its turn is in needs, with no call waiting: your answer is its prompt.
        let escalated = r.phase == Phase::Needs && !self.has_waiting(id);
        let turn_runs = locked(&self.procs).get(id).is_some_and(|p| p.busy);
        match r.phase {
            Phase::Idle => {}
            Phase::Needs if escalated && !turn_runs => {}
            Phase::Working | Phase::Needs if escalated || r.phase == Phase::Working => {
                return Err(Failure::new(
                    format!("{id} is working: wait for its reply"),
                    format!("drover watch --run {id}"),
                )
                .into());
            }
            Phase::Needs => {
                return Err(Failure::new(
                    format!("{id} waits on an approval"),
                    format!("drover show {id}"),
                )
                .into());
            }
            p => {
                return Err(Failure::new(
                    format!("{id} is {}, with no agent up", p.as_str()),
                    format!("drover show {id}"),
                )
                .into());
            }
        }
        self.deliver(&mut r, text).await?;
        Ok(self.brief(&r))
    }

    async fn cmd_approve_run(&self, from: Option<&RunId>, id: &RunId) -> Result<Value> {
        Self::by_you(from, "approve runs")?;
        // pending to starting under the lock, so a second approve finds it started
        let turn = self.turns.lock().await;
        let mut r = self.get(id)?;
        if r.phase != Phase::Pending {
            return Err(Failure::new(
                format!("{id} is {}, not pending", r.phase.as_str()),
                format!("drover show {id}"),
            )
            .into());
        }
        r.phase = Phase::Starting;
        self.save(&r)?;
        drop(turn);
        self.start(&mut r).await?;
        Ok(self.brief(&self.get(id)?))
    }

    async fn cmd_drop_run(&self, from: Option<&RunId>, id: &RunId) -> Result<Value> {
        Self::by_you(from, "drop runs")?;
        let _turn = self.turns.lock().await;
        let r = self.get(id)?;
        if r.phase != Phase::Pending {
            return Err(Failure::new(
                format!("{id} is {}, not pending", r.phase.as_str()),
                format!("drover done {id}"),
            )
            .into());
        }
        self.store.delete(id)?;
        Ok(json!({ "run": id, "dropped": true }))
    }

    // The mate's goal graph: replaced, or one node changed. The event carries the whole graph.
    fn cmd_goal(&self, from: Option<&RunId>, op: GoalOp) -> Result<Value> {
        let Some(id) = from else {
            return Err(Failure::new(
                "a goal belongs to a run: you have none",
                "run it from a mate, with $DROVER_RUN set",
            )
            .into());
        };
        let r = self.get(id)?;
        let (node, graph) = match op {
            GoalOp::Set {
                graph: Some(text), ..
            } => (None, Graph::parse(&text)?),
            GoalOp::Set { graph: None, .. } => bail!("goal set: no file text"),
            GoalOp::Node {
                id: node,
                status,
                round,
                crew,
                review,
            } => {
                let mut graph = self
                    .store
                    .goal(id)?
                    .map(serde_json::from_value::<Graph>)
                    .transpose()?
                    .ok_or_else(|| {
                        Failure::new(
                            format!("{id} has no goal"),
                            "drover goal set --file goal.json",
                        )
                    })?;
                graph.update(
                    &node,
                    Update {
                        status,
                        round,
                        crew,
                        review,
                    },
                )?;
                (Some(node), graph)
            }
        };
        let graph = serde_json::to_value(&graph)?;
        self.store.set_goal(id, &graph)?;
        self.emit(&r, Kind::Goal, json!({ "node": node, "graph": graph }))?;
        let nodes = graph.get("nodes").and_then(Value::as_array);
        let changed = node.as_deref().and_then(|n| {
            nodes?
                .iter()
                .find(|x| x.get("id").and_then(Value::as_str) == Some(n))
        });
        Ok(match changed {
            Some(n) => json!({ "run": id, "node": n, "help": [format!("drover show {id}")] }),
            None => {
                json!({ "run": id, "nodes": nodes.map_or(0, Vec::len), "help": [format!("drover show {id}")] })
            }
        })
    }

    // A question for you, from the calling run. It stays in needs until you answer with `send`.
    async fn cmd_escalate(&self, from: Option<&RunId>, text: &str) -> Result<Value> {
        let Some(id) = from else {
            return Err(Failure::new(
                "you are who escalations are for",
                "drover send <run> <text>",
            )
            .into());
        };
        let _turn = self.turns.lock().await;
        let mut r = self.get(id)?;
        if !r.phase.live() {
            return Err(Failure::new(
                format!("{id} is {}: it cannot escalate", r.phase.as_str()),
                format!("drover show {id}"),
            )
            .into());
        }
        r.phase = Phase::Needs;
        self.emit(&r, Kind::Needs, json!({ "escalation": text }))?;
        self.save(&r)?;
        Ok(json!({
            "run": id, "escalated": true,
            "help": ["end your turn: the answer arrives as your next prompt"],
        }))
    }

    // The next reply, needs, done or failed of the caller's crew, after the last one it was given.
    // A run's cursor is kept here, so a wait that timed out or was cut loses nothing; a
    // call from you reads what comes after now.
    async fn cmd_wait(
        &self,
        from: Option<&RunId>,
        parent: Option<RunId>,
        timeout: u64,
    ) -> Result<Value> {
        let parent = match (from, parent) {
            (Some(f), Some(p)) if &p != f => {
                return Err(Failure::new(
                    format!("run {f} cannot wait on the crew of {p}"),
                    "drover wait --any, or with --parent $DROVER_RUN",
                )
                .into());
            }
            (Some(f), _) => Some(f.clone()),
            (None, p) => p,
        };
        let since = from.map(|f| self.store.wait_cursor(f)).transpose()?;
        let mut feed = Feed::new(
            self,
            Filter {
                since,
                ..Filter::default()
            },
        )?;
        let hits = async {
            loop {
                let hits: Vec<Value> = feed
                    .next(self)
                    .await?
                    .into_iter()
                    .filter(|e| {
                        matches!(
                            e.get("kind").and_then(Value::as_str),
                            Some("reply" | "needs" | "done" | "failed")
                        ) && parent.as_ref().is_none_or(|p| {
                            e.get("parent").and_then(Value::as_str) == Some(p.as_str())
                        })
                    })
                    .collect();
                if !hits.is_empty() {
                    return Ok::<_, crate::Error>(hits);
                }
            }
        };
        let hits = tokio::time::timeout(Duration::from_secs(timeout), hits)
            .await
            .unwrap_or_else(|_| Ok(Vec::new()))?;
        if let (Some(f), Some(last)) = (from, hits.last().and_then(|e| e.get("id")?.as_i64())) {
            self.store.set_wait_cursor(f, last)?;
        }
        let mut help = vec![];
        for e in &hits {
            let run = e
                .pointer("/run")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(perm) = e.pointer("/data/perm").and_then(Value::as_str) {
                help.push(format!(
                    "drover approve {run} {perm} | drover deny {run} {perm} --reason <why>"
                ));
            }
        }
        help.push("drover wait --any".to_string());
        let events: Vec<Value> = hits.iter().map(wait_row).collect();
        Ok(json!({ "events": events, "help": help }))
    }

    // A pane run becomes yours. A headless run's process stops, so two never share its session,
    // and the session opens in a pane with --resume.
    async fn cmd_attach(&self, from: Option<&RunId>, id: &RunId) -> Result<Value> {
        Self::by_you(from, "attach")?;
        let turn = self.turns.lock().await;
        let mut r = self.get(id)?;
        let noop = (r.owner == Owner::User && r.mode != Mode::Headless).then_some("yours already");
        if r.mode == Mode::Headless {
            let session = r
                .session
                .clone()
                .filter(|_| r.phase.live() && r.phase != Phase::Starting);
            let Some(session) = session else {
                return Err(Failure::new(
                    format!("{id} is {}, with no session to open", r.phase.as_str()),
                    format!("drover show {id}"),
                )
                .into());
            };
            if r.profile == Profile::Reader {
                return Err(Failure::new(
                    format!("{id} is a reader: it has no worktree to open"),
                    format!("drover show {id}"),
                )
                .into());
            }
            let h = self.host(&r.machine)?;
            let harness = Self::harness(&r)?;
            self.retire(id).await;
            let wt = self.worktree(h, &r).await?;
            let asked = self.asked(&r)?;
            let hooks = self.hook_url(id);
            r.mode = Mode::Pane {
                pane: Some(wt.pane_id.clone()),
            };
            let argv = harness.start(&self.spec(&r, &asked, &hooks, Some(&session)));
            self.install_hooks(&r)?;
            r.phase = Phase::Idle;
            r.owner = Owner::User;
            self.save(&r)?;
            self.wake.notify_waiters();
            drop(turn);
            // claude may resume a session under the id of a copy
            locked(&self.rekey).insert(id.clone());
            self.launch(h, &r, &wt, &argv, None).await?;
            return Ok(
                json!({ "run": id, "machine": h.name, "pane": wt.pane_id.0, "owner": "user", "noop": noop }),
            );
        }
        let (h, pane) = self.pane_of(&r)?;
        r.owner = Owner::User;
        self.save(&r)?;
        Ok(json!({ "run": id, "machine": h.name, "pane": pane.0, "owner": "user", "noop": noop }))
    }

    async fn cmd_screen(&self, id: &RunId, lines: u32) -> Result<Value> {
        let r = self.get(id)?;
        let (h, pane) = self.pane_of(&r)?;
        let text = herdr::screen(h, &pane, lines).await?;
        Ok(json!({ "run": id, "phase": r.phase, "screen": text.trim_end() }))
    }

    async fn cmd_keys(&self, from: Option<&RunId>, id: &RunId, keys: &[String]) -> Result<Value> {
        let r = self.get(id)?;
        Self::owned(&r, from)?;
        let (h, pane) = self.pane_of(&r)?;
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        herdr::keys(h, &pane, &keys).await?;
        Ok(json!({ "run": id, "keys": keys, "help": [format!("drover screen {id}")] }))
    }

    async fn cmd_stop(&self, from: Option<&RunId>, id: &RunId) -> Result<Value> {
        if self.get(id)?.mode != Mode::Headless {
            return self.cmd_keys(from, id, &["esc".into()]).await;
        }
        let _turn = self.turns.lock().await;
        let mut r = self.get(id)?;
        Self::owned(&r, from)?;
        match Self::harness(&r)?.abort() {
            // the next send resumes its session
            Abort::Kill => self.retire(id).await,
            Abort::Stdin(line) => {
                if let Some(p) = locked(&self.procs).get(id) {
                    let _ = p.stdin.send(line);
                }
            }
        }
        if matches!(r.phase, Phase::Working | Phase::Needs) {
            r.phase = Phase::Idle;
            self.save(&r)?;
        }
        Ok(self.brief(&r))
    }

    async fn cmd_done(
        &self,
        from: Option<&RunId>,
        id: &RunId,
        remove: bool,
        entry: Option<&str>,
    ) -> Result<Value> {
        // checked under the lock, closed without it: an unreachable remote holds up no hook
        let r = {
            let _turn = self.turns.lock().await;
            let r = self.get(id)?;
            // closing is not typing: you may close any run
            if from.is_some() {
                Self::owned(&r, from)?;
            }
            match r.phase {
                Phase::Pending => {
                    return Err(Failure::new(
                        format!("{id} is pending"),
                        format!("drover drop-run {id}"),
                    )
                    .into());
                }
                p if p.ended() => {
                    return Err(Failure::new(
                        format!("{id} is {} already", p.as_str()),
                        format!("drover show {id}"),
                    )
                    .into());
                }
                _ => r,
            }
        };
        // the entry is logged before anything closes, so a failed log loses nothing
        if let Some(entry) = entry {
            brain_log(entry).await?;
        }
        let mut notes = Vec::new();
        let mut removed = false;
        match (self.host(&r.machine), r.mode.pane().cloned(), r.workspace()) {
            (Ok(h), _, _) if r.mode == Mode::Headless => {
                self.retire(id).await;
                if remove && r.profile == Profile::Actor {
                    match herdr::remove_path(h, &r.cwd.to_string_lossy()).await {
                        Ok(said) => (removed, notes) = (true, vec![said]),
                        Err(e) => notes.push(format!("kept the checkout: {e}")),
                    }
                }
            }
            (Ok(h), Some(pane), Some(ws)) => {
                if remove && r.role != Role::Mate {
                    match herdr::remove(h, &ws).await {
                        Ok(said) => (removed, notes) = (true, vec![said]),
                        Err(e) => notes.push(format!("kept the checkout: {e}")),
                    }
                }
                // v1 mates share one workspace, so only their pane goes
                let closed = match (removed, r.role) {
                    (true, _) => Ok(()),
                    (false, Role::Mate) => herdr::close_pane(h, &pane).await,
                    (false, _) => herdr::close(h, &ws).await,
                };
                // a pane that is gone already needs no closing; any other failure keeps the run,
                // or its agent would go on in a pane nobody follows
                if let Err(e) = closed
                    && !herdr::panes(h).await.is_ok_and(|p| !p.contains(&pane))
                {
                    let logged = match entry {
                        Some(_) => "; its entry is logged already",
                        None => "",
                    };
                    return Err(Failure::new(
                        format!("{id} is not closed: {e}{logged}"),
                        format!("drover done {id}, once {} answers", h.name),
                    )
                    .into());
                }
            }
            // a machine this service does not drive has nothing it could close
            (Err(e), ..) => notes.push(format!("{e}: nothing closed")),
            _ => {}
        }
        let _turn = self.turns.lock().await;
        // a hook may have moved it on meanwhile; this end, with its entry, is the record
        let mut r = self.get(id)?;
        self.end(
            &mut r,
            Phase::Done,
            Kind::Done,
            json!({ "entry": entry.is_some(), "removed": removed }),
        )?;
        let note = notes.join("\n");
        Ok(
            json!({ "run": id, "phase": r.phase, "removed": removed, "note": (!note.trim().is_empty()).then_some(note.trim()) }),
        )
    }

    // Open runs, or with `all` every run, each with its last reply.
    pub fn snapshot(&self, all: bool) -> Result<Value> {
        let replies = self.store.open_replies()?;
        let runs = if all {
            self.store.list()?
        } else {
            self.store.list_open()?
        };
        let runs: Vec<Value> = runs
            .iter()
            .map(|r| {
                let mut v = row(r);
                if let Value::Object(m) = &mut v {
                    m.insert("pane".into(), json!(r.mode.pane().map(|p| &p.0)));
                    m.insert("parent".into(), json!(r.parent));
                    m.insert("owner".into(), json!(r.owner.to_string()));
                    let reply = replies.get(r.id.as_str()).and_then(|d| d.get("text"));
                    m.insert("reply".into(), reply.cloned().unwrap_or_default());
                }
                v
            })
            .collect();
        let machines: Vec<&str> = self.hosts.iter().map(|h| h.name.as_str()).collect();
        Ok(json!({ "machines": machines, "runs": runs }))
    }

    fn home(&self) -> Result<Value> {
        let runs = self.store.list_open()?;
        let mut phases = serde_json::Map::new();
        for p in [
            Phase::Pending,
            Phase::Starting,
            Phase::Working,
            Phase::Needs,
            Phase::Idle,
        ] {
            phases.insert(
                p.as_str().into(),
                runs.iter().filter(|r| r.phase == p).count().into(),
            );
        }
        let needs: Vec<Value> = runs
            .iter()
            .filter(|r| matches!(r.phase, Phase::Pending | Phase::Needs))
            .map(|r| {
                let mut v = row(r);
                if let (Value::Object(m), Some(ask)) = (&mut v, self.ask(&r.id)) {
                    m.insert("ask".into(), ask);
                }
                v
            })
            .collect();
        let mut help = Vec::new();
        if runs.iter().any(|r| r.phase == Phase::Pending) {
            help.push("drover approve-run <run>");
        }
        if runs.iter().any(|r| r.phase == Phase::Needs) {
            help.push("drover show <run>, then drover approve|deny <run> <perm>");
        }
        help.extend([
            "drover ls",
            "drover run --repo <repo> --branch <branch> [--headless] --prompt <prompt>",
            "drover <command> --help",
        ]);
        Ok(json!({ "runs": phases, "needs": needs, "help": help }))
    }

    // ── herdr: a pane run ends when its pane goes ──

    async fn gone(&self, h: &Host, pane: Option<&PaneId>) {
        let _turn = self.turns.lock().await;
        let live = self.live_on(&h.name).unwrap_or_default();
        // a closed workspace names no pane: ask which are left
        let left = match pane {
            Some(_) => None,
            None => match herdr::panes(h).await {
                Ok(p) => Some(p),
                Err(_) => return, // unreachable, the watcher retries
            },
        };
        for (mut r, p) in live {
            let closed = match (&left, pane) {
                (Some(left), _) => !left.contains(&p),
                (None, Some(pane)) => &p == pane,
                (None, None) => false,
            };
            if closed
                && let Err(e) = self.end(&mut r, Phase::Done, Kind::Done, json!({ "by": "exit" }))
            {
                eprintln!("run {}: {e}", r.id);
            }
        }
    }
}

// The last `max` bytes of `s`, or a few fewer: the cut lands on a char boundary.
fn keep_tail(s: &mut String, max: usize) {
    let cut = s.ceil_char_boundary(s.len().saturating_sub(max));
    s.drain(..cut);
}

// `once`: one prompt per process, which reads its stdin to EOF, so the stream closes after it.
async fn write_lines(mut stdin: ChildStdin, mut rx: mpsc::UnboundedReceiver<String>, once: bool) {
    while let Some(mut line) = rx.recv().await {
        line.push('\n');
        if stdin.write_all(line.as_bytes()).await.is_err() || once {
            return;
        }
    }
}

// A headless process's stdout as events. Text deltas go out together, every TEXT_BATCH, and
// before any other event, so the stream keeps their order.
async fn read_proc(
    app: Arc<App>,
    (id, machine): (RunId, Machine),
    proc: u64,
    harness: &'static dyn Harness,
    stdout: ChildStdout,
    (stderr, known): (ChildStderr, watch::Sender<Option<Ident>>),
) {
    // the first line names the process; the rest, its last 2000 bytes, says why it failed
    let me = Arc::clone(&app);
    let run = id.clone();
    let tail = tokio::spawn(async move {
        let mut tail = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if known.borrow().is_none()
                && let Some(ident) = harness::Ident::parse(&l)
            {
                if let Err(e) = me.store.remember(&run, &machine, &ident) {
                    eprintln!("run {run}: {e}");
                }
                known.send_replace(Some(ident));
                continue;
            }
            tail.push_str(&l);
            tail.push('\n');
            keep_tail(&mut tail, 2000);
        }
        tail
    });
    let mut lines = BufReader::new(stdout).lines();
    let mut p = Parse::default();
    let mut text = String::new();
    let mut due = None;
    loop {
        let line = tokio::select! {
            l = lines.next_line() => l,
            () = tokio::time::sleep_until(due.unwrap_or_else(tokio::time::Instant::now)), if due.is_some() => {
                due = None;
                app.apply(&id, vec![Out::Text(std::mem::take(&mut text))]).await;
                continue;
            }
        };
        let Ok(Some(line)) = line else { break };
        let mut outs = Vec::new();
        for out in harness.events(&line, &mut p) {
            match out {
                Out::Text(t) => {
                    text.push_str(&t);
                    due.get_or_insert_with(|| tokio::time::Instant::now() + TEXT_BATCH);
                }
                o => {
                    if !text.is_empty() {
                        outs.push(Out::Text(std::mem::take(&mut text)));
                        due = None;
                    }
                    outs.push(o);
                }
            }
        }
        app.apply(&id, outs).await;
    }
    if !text.is_empty() {
        app.apply(&id, vec![Out::Text(text)]).await;
    }
    let stderr = tail.await.unwrap_or_default();
    app.exited(&id, proc, stderr).await;
}

// A remote's agents post their hooks to 127.0.0.1 on their own machine; this carries them here.
async fn tunnel(host: String, port: u16) {
    let forward = format!("127.0.0.1:{port}:127.0.0.1:{port}");
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        let status = Command::new("ssh")
            .args(SSH_OPTS)
            .args([
                "-N",
                "-o",
                "ExitOnForwardFailure=yes",
                "-R",
                &forward,
                &host,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await;
        eprintln!("hooks tunnel to {host} ended: {status:?}");
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
    }
}

// `brain log` takes the entry on stdin. brain may be absent; then nothing closes.
async fn brain_log(entry: &str) -> Result<()> {
    let child = Command::new("brain")
        .arg("log")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Failure::new(
                "brain is not on PATH: the entry was not logged",
                "drover done <run>, without --entry",
            )
            .into());
        }
        Err(e) => return Err(e.into()),
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(entry.as_bytes()).await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("brain log: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

async fn watch_machine(app: &App, h: &Host) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match watch_session(app, h).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                eprintln!("events {}: {e}", h.name);
                sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
            }
        }
    }
}

// Subscribed while the machine has live pane runs.
async fn watch_session(app: &App, h: &Host) -> Result<()> {
    loop {
        // enabled before the check, so a pane added after it still wakes us
        let woke = app.wake.notified();
        tokio::pin!(woke);
        woke.as_mut().enable();
        if !app.live_on(&h.name)?.is_empty() {
            break;
        }
        woke.await;
    }
    let mut conn = socket::Events::subscribe(h.ssh()).await?;
    // subscribe first, then look, so a pane that goes between the two is seen
    app.gone(h, None).await;
    loop {
        let woke = app.wake.notified();
        tokio::pin!(woke);
        woke.as_mut().enable();
        if app.live_on(&h.name)?.is_empty() {
            return Ok(());
        }
        tokio::select! {
            ev = conn.recv() => match ev? {
                socket::Push::Lost => bail!("events_lost on {}", h.name),
                socket::Push::Gone(pane) => app.gone(h, Some(&pane)).await,
                socket::Push::Reconcile => app.gone(h, None).await,
            },
            () = &mut woke => {}
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn app_with(config: &str) -> Result<Arc<App>> {
        let config = Config::parse(&format!("host = \"box\"\n{config}"))?;
        Ok(App::new(
            &config,
            Store::open(Path::new(":memory:"))?,
            PathBuf::from("/tmp/x"),
        ))
    }

    pub(crate) fn app() -> Result<Arc<App>> {
        app_with("")
    }

    // A multi-byte character at the cut stays whole.
    #[test]
    fn the_stderr_tail_cuts_between_chars() {
        let mut tail = "é\n".repeat(1500);
        keep_tail(&mut tail, 2000);
        // 2500 bytes go, which ends inside an é: the cut moves past it
        assert_eq!(tail.len(), 1999);
        assert!(tail.starts_with('\n') && tail.ends_with("é\n"));
        let mut short = "abc".to_string();
        keep_tail(&mut short, 2000);
        assert_eq!(short, "abc");
    }

    // A hook from another session is ignored, and its call denied, unless drover expects a new
    // session: the first one, after /clear, after a resume it started.
    #[tokio::test]
    async fn a_hook_from_another_session_is_ignored() -> Result<()> {
        let app = app()?;
        let mut r = run("r-0000e1", Role::Task)?;
        r.phase = Phase::Idle;
        app.store.save(&r)?;
        let (a, b, c) = (
            "aaaaaaaa-0000-0000-0000-000000000000",
            "bbbbbbbb-0000-0000-0000-000000000000",
            "cccccccc-0000-0000-0000-000000000000",
        );
        let hook = |sid: &str, name: &str, extra: Value| {
            let mut body = hook_body(name, extra);
            if let Some(s) = body.get_mut("session_id") {
                *s = json!(sid);
            }
            body
        };
        let session = |app: &App| {
            app.store
                .get(&"r-0000e1".parse().ok()?)
                .ok()?
                .and_then(|r| r.session)
                .map(|s| s.as_str().to_string())
        };
        // the first session a pane run has is its own
        app.hook(&r.id, &hook(a, "UserPromptSubmit", json!({})))
            .await;
        assert_eq!(session(&app).as_deref(), Some(a));
        // another one moves nothing, and its tool call is denied without a needs
        app.hook(
            &r.id,
            &hook(b, "Stop", json!({"last_assistant_message": "x"})),
        )
        .await;
        assert_eq!(phase(&app, "r-0000e1"), Some(Phase::Working));
        let answer = app
            .hook(
                &r.id,
                &hook(
                    b,
                    "PermissionRequest",
                    json!({"tool_name": "Bash", "tool_input": {}}),
                ),
            )
            .await;
        assert_eq!(at(&answer, "/hookSpecificOutput/decision/message"), FOREIGN);
        assert_eq!(
            (session(&app).as_deref(), app.store.perms(&r.id)?),
            (Some(a), 0)
        );
        // /clear: the next hook brings the new session, once
        app.hook(&r.id, &hook(a, "SessionEnd", json!({"reason": "clear"})))
            .await;
        app.hook(&r.id, &hook(b, "UserPromptSubmit", json!({})))
            .await;
        assert_eq!(session(&app).as_deref(), Some(b));
        app.hook(&r.id, &hook(c, "UserPromptSubmit", json!({})))
            .await;
        assert_eq!(session(&app).as_deref(), Some(b));
        // a resume drover started may come back under a copy's id
        locked(&app.rekey).insert(r.id.clone());
        app.hook(&r.id, &hook(c, "UserPromptSubmit", json!({})))
            .await;
        assert_eq!(session(&app).as_deref(), Some(c));
        Ok(())
    }

    async fn request(app: &App, from: Option<&str>, line: &str) -> Result<Value> {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from(
            std::iter::once("drover").chain(line.split_whitespace()),
        )?;
        let cmd = cli.into_cmd();
        app.act(from.map(str::parse).transpose()?, cmd).await
    }

    fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
        v.pointer(path).unwrap_or(&Value::Null)
    }

    fn run(id: &str, role: Role) -> Result<Run> {
        Ok(Run {
            id: id.parse()?,
            machine: "box".parse()?,
            cwd: "/w".into(),
            harness: crate::run::HarnessName::Claude,
            mode: Mode::Pane {
                pane: Some(PaneId("w1:p1".into())),
            },
            role,
            parent: None,
            profile: Profile::Actor,
            owner: Owner::User,
            session: None,
            phase: Phase::Idle,
            usage: Usage::default(),
            created_at: 1,
            ended_at: None,
        })
    }

    fn failure(r: Result<Value>) -> Option<String> {
        r.err().map(|e| Failure::from(e).error)
    }

    fn phase(app: &App, id: &str) -> Option<Phase> {
        app.store.get(&id.parse().ok()?).ok()?.map(|r| r.phase)
    }

    // A mate's wait returns its crew's replies in order, each once; its goal and escalation are carried.
    #[tokio::test]
    async fn a_mate_hears_its_crew_and_asks_you() -> Result<()> {
        let app = app()?;
        let mate = run("r-00000a", Role::Mate)?;
        app.store.save(&mate)?;
        let mut crew = run("r-00000b", Role::Crew)?;
        crew.parent = Some(mate.id.clone());
        crew.owner = Owner::Run(mate.id.clone());
        app.store.save(&crew)?;
        let wait = || {
            request(
                &app,
                Some("r-00000a"),
                "wait --any --parent r-00000a --timeout 1",
            )
        };
        let reply = |text: &str| app.emit(&crew, Kind::Reply, json!({ "text": text }));
        reply("DONE: one")?;
        reply("QUESTION: two")?;
        app.emit(&mate, Kind::Reply, json!({ "text": "not its crew" }))?;
        let got = wait().await?;
        assert_eq!(
            (
                at(&got, "/events/0/text"),
                at(&got, "/events/1/text"),
                at(&got, "/events").as_array().map(Vec::len)
            ),
            (&json!("DONE: one"), &json!("QUESTION: two"), Some(2))
        );
        // nothing twice, and a reply that lands while it waits is not lost
        let late = async {
            sleep(Duration::from_millis(100)).await;
            reply("DONE: three")
        };
        let (got, late) = tokio::join!(wait(), late);
        late?;
        assert_eq!(at(&got?, "/events/0/text"), "DONE: three");
        let got = wait().await?;
        assert_eq!(at(&got, "/events").as_array().map(Vec::len), Some(0));
        let theirs = request(&app, Some("r-00000b"), "wait --parent r-00000a").await;
        assert!(failure(theirs).is_some_and(|e| e.contains("cannot wait on the crew of")));

        let file = r#"{"goal":"g","nodes":[{"id":"docs","check":"ls"}]}"#;
        let set = crate::cli::Cmd::Goal {
            op: crate::cli::GoalOp::Set {
                file: String::new(),
                graph: Some(file.into()),
            },
        };
        app.act(Some(mate.id.clone()), set).await?;
        request(
            &app,
            Some("r-00000a"),
            "goal node docs --status pass --round 2 --crew r-00000b",
        )
        .await?;
        let shown = request(&app, None, "show r-00000a").await?;
        assert_eq!(
            (
                at(&shown, "/goal/nodes/0/status"),
                at(&shown, "/goal/nodes/0/round"),
                at(&shown, "/goal/nodes/0/crew")
            ),
            (&json!("pass"), &json!(2), &json!("r-00000b"))
        );
        assert!(failure(request(&app, None, "goal node docs --round 1").await).is_some());
        assert!(
            failure(request(&app, Some("r-00000a"), "goal node gone --round 1").await).is_some()
        );

        request(&app, Some("r-00000a"), "escalate --text branch").await?;
        let home = request(&app, None, "").await?;
        assert_eq!(
            (at(&home, "/needs/0/run"), at(&home, "/needs/0/ask")),
            (&json!("r-00000a"), &json!("branch"))
        );
        // its turn ends, and the question still stands; your answer is its next prompt
        assert_eq!(phase(&app, "r-00000a"), Some(Phase::Needs));
        Ok(())
    }

    // A run a mate asks for waits for you; dropping it leaves nothing.
    #[tokio::test]
    async fn a_requested_run_waits_for_you() -> Result<()> {
        let app = app()?;
        let mate = run("r-00000a", Role::Mate)?;
        app.store.save(&mate)?;
        let out = request(
            &app,
            Some("r-00000a"),
            "run --repo drover --branch x --parent r-00000a --headless --model haiku --prompt go",
        )
        .await?;
        assert_eq!(at(&out, "/phase"), "pending");
        assert_eq!(at(&out, "/mode"), "headless");
        assert_eq!(at(&out, "/owner"), "r-00000a");
        let id = at(&out, "/run").as_str().unwrap_or_default().to_string();

        let ls = request(&app, None, "ls --phase pending").await?;
        assert_eq!(at(&ls, "/runs/0/role"), "crew");
        let asked = app
            .store
            .last(&id.parse()?, Kind::Requested)?
            .unwrap_or_default();
        assert_eq!(at(&asked, "/model"), "haiku");
        assert!(
            at(&asked, "/prompt")
                .as_str()
                .is_some_and(|p| p.starts_with("go\n\n") && p.ends_with("RUNNING: and what runs."))
        );

        // the mate can neither approve nor type into it before it starts
        assert!(
            failure(request(&app, Some("r-00000a"), &format!("approve-run {id}")).await)
                .is_some_and(|e| e.starts_with("only you"))
        );
        assert!(
            failure(request(&app, Some("r-00000a"), &format!("send {id} hi")).await)
                .is_some_and(|e| e.contains("pending"))
        );
        assert!(
            failure(request(&app, None, &format!("send {id} hi")).await)
                .is_some_and(|e| e.contains("owned by r-00000a"))
        );
        assert!(
            failure(request(&app, None, &format!("done {id}")).await)
                .is_some_and(|e| e.contains("pending"))
        );

        assert_eq!(
            at(
                &request(&app, None, &format!("drop-run {id}")).await?,
                "/dropped"
            ),
            true
        );
        assert!(app.store.get(&id.parse()?)?.is_none());
        assert!(
            failure(
                request(
                    &app,
                    None,
                    "run --repo drover --branch x --parent r-00000b --prompt go"
                )
                .await
            )
            .is_some_and(|e| e == "no run r-00000b")
        );
        Ok(())
    }

    // A run asking for a run cannot skip approval by leaving out --parent, nor name another parent.
    #[tokio::test]
    async fn a_run_started_by_a_run_is_its_pending_crew() -> Result<()> {
        let app = app()?;
        let mut mate = run("r-00000a", Role::Mate)?;
        app.store.save(&mate)?;
        mate.id = "r-00000b".parse()?;
        app.store.save(&mate)?;

        let out = request(
            &app,
            Some("r-00000a"),
            "run --repo drover --branch x --prompt go",
        )
        .await?;
        assert_eq!(at(&out, "/phase"), "pending");
        assert_eq!(at(&out, "/owner"), "r-00000a");
        let id: RunId = at(&out, "/run").as_str().unwrap_or_default().parse()?;
        let r = app.store.get(&id)?;
        assert!(r.is_some_and(
            |r| r.role == Role::Crew && r.parent.is_some_and(|p| p.as_str() == "r-00000a")
        ));

        let other = request(
            &app,
            Some("r-00000a"),
            "run --repo drover --branch x --parent r-00000b --prompt go",
        )
        .await;
        assert!(
            failure(other)
                .is_some_and(|e| e == "run r-00000a cannot ask for a run on behalf of r-00000b")
        );
        // a caller that is no run at all is refused too
        assert!(
            failure(
                request(
                    &app,
                    Some("r-0000ff"),
                    "run --repo drover --branch x --prompt go"
                )
                .await
            )
            .is_some_and(|e| e == "no run r-0000ff")
        );
        assert_eq!(app.store.list()?.len(), 3);
        Ok(())
    }

    // A second serve on the same state dir refuses; once the first is gone, a new one starts.
    #[test]
    fn one_serve_per_state_dir() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("drover-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let first = lock(&dir)?;
        let second = lock(&dir).err().map(|e| Failure::from(e).error);
        assert!(second.is_some_and(|e| e.starts_with("drover serve already runs on")));
        drop(first);
        // a child another test forks holds a copy of the fd until it execs
        let mut again = lock(&dir);
        for _ in 0..50 {
            if again.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            again = lock(&dir);
        }
        std::fs::remove_dir_all(&dir)?;
        assert!(again.is_ok());
        Ok(())
    }

    // A hook saves its copy of a run while it holds the turns lock. attach and done wait for it,
    // so their change lands after and is the one that stays.
    #[tokio::test]
    async fn a_command_waits_for_a_hook_and_wins() -> Result<()> {
        let app = app()?;
        let mut crew = run("r-00000c", Role::Crew)?;
        crew.owner = Owner::Run("r-00000a".parse()?);
        app.store.save(&crew)?;
        // on a machine this service does not drive, so done touches no herdr
        let mut task = run("r-00000d", Role::Task)?;
        task.machine = "elsewhere".parse()?;
        app.store.save(&task)?;

        let turn = app.turns.lock().await;
        let (a, b) = (Arc::clone(&app), Arc::clone(&app));
        let attach = tokio::spawn(async move {
            a.act(
                None,
                Cmd::Attach {
                    run: "r-00000c".parse()?,
                },
            )
            .await
        });
        let done = tokio::spawn(async move {
            let cmd = Cmd::Done {
                run: "r-00000d".parse()?,
                remove: false,
                entry: None,
            };
            b.act(None, cmd).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!attach.is_finished() && !done.is_finished());
        // the hook's stale copies: still the mate's, still working
        crew.phase = Phase::Working;
        task.phase = Phase::Working;
        app.store.save(&crew)?;
        app.store.save(&task)?;
        drop(turn);

        attach.await??;
        done.await??;
        let crew = app.store.get(&crew.id)?;
        let task = app.store.get(&task.id)?;
        assert!(crew.is_some_and(|r| r.owner == Owner::User));
        assert!(task.is_some_and(|r| r.phase == Phase::Done && r.ended_at.is_some()));
        Ok(())
    }

    // A fake herdr: every close fails, and pane.list names `panes`.
    fn fake_herdr(path: &Path, panes: &'static [&'static str]) -> Result<()> {
        let listener = tokio::net::UnixListener::bind(path)?;
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (read, mut write) = stream.into_split();
                let mut line = String::new();
                if BufReader::new(read).read_line(&mut line).await.is_err() {
                    continue;
                }
                let req: Value = serde_json::from_str(&line).unwrap_or_default();
                let reply = match req.get("method").and_then(Value::as_str) {
                    Some("pane.list") => {
                        let panes: Vec<Value> =
                            panes.iter().map(|p| json!({ "pane_id": p })).collect();
                        json!({ "id": req.get("id"), "result": { "panes": panes } })
                    }
                    _ => {
                        json!({ "id": req.get("id"), "error": { "code": "boom", "message": "close refused" } })
                    }
                };
                let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
            }
        });
        Ok(())
    }

    // done ends a run only once its pane is closed, or gone already.
    #[tokio::test]
    async fn done_keeps_a_run_whose_pane_did_not_close() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("drover-done-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let cases: [(&str, Option<&'static [&'static str]>, bool); 3] = [
            ("r-0000a1", Some(&["w1:p1"]), false), // herdr refuses, the pane is still there
            ("r-0000a2", None, false),             // herdr does not answer
            ("r-0000a3", Some(&[]), true),         // the pane is gone already
        ];
        for (n, (id, panes, closes)) in cases.into_iter().enumerate() {
            let socket = dir.join(format!("h{n}.sock"));
            if let Some(panes) = panes {
                fake_herdr(&socket, panes)?;
            }
            let host = Host {
                link: Arc::new(socket::Link::at(socket)),
                ..Host::new("box".into(), Reach::Local, None)
            };
            let app = App::with(
                vec![host],
                &Config::parse("host = \"box\"")?,
                Store::open(Path::new(":memory:"))?,
                PathBuf::from("/tmp/x"),
            );
            app.store.save(&run(id, Role::Task)?)?;
            let out = app
                .act(
                    None,
                    Cmd::Done {
                        run: id.parse()?,
                        remove: true,
                        entry: None,
                    },
                )
                .await;
            let r = app.store.get(&id.parse()?)?;
            if closes {
                assert!(
                    out.is_ok() && r.is_some_and(|r| r.phase == Phase::Done),
                    "{id}"
                );
            } else {
                assert!(
                    failure(out).is_some_and(|e| e.starts_with(&format!("{id} is not closed:"))),
                    "{id}"
                );
                assert!(
                    r.is_some_and(|r| r.phase == Phase::Idle && r.ended_at.is_none()),
                    "{id}"
                );
            }
        }
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    fn hook_body(name: &str, extra: Value) -> Value {
        let mut b =
            json!({"session_id": "317266eb-32ce-4e67-9457-7a097c828473", "hook_event_name": name});
        if let (Value::Object(b), Value::Object(e)) = (&mut b, extra) {
            b.extend(e);
        }
        b
    }

    fn touch(file: &str) -> Value {
        hook_body(
            "PermissionRequest",
            json!({"tool_name": "Bash", "tool_input": {"command": format!("touch {file}"), "description": "make it"}}),
        )
    }

    async fn needs(app: &App, id: &RunId) -> Result<Value> {
        for _ in 0..100 {
            if phase(app, id.as_str()) == Some(Phase::Needs) {
                return Ok(app.store.last(id, Kind::Needs)?.unwrap_or_default());
            }
            sleep(Duration::from_millis(10)).await;
        }
        bail!("no needs")
    }

    // A permission hook waits for the owner's answer; the run is `needs` until it comes.
    #[tokio::test]
    async fn the_owner_answers_a_permission() -> Result<()> {
        let app = app()?;
        let mut crew = run("r-00000c", Role::Crew)?;
        crew.owner = Owner::Run("r-00000a".parse()?);
        crew.phase = Phase::Working;
        app.store.save(&crew)?;
        let id = crew.id.clone();

        let a = Arc::clone(&app);
        let hook = tokio::spawn(async move {
            Ok::<_, crate::Error>(a.hook(&"r-00000c".parse()?, &touch("x")).await)
        });
        let n = needs(&app, &id).await?;
        assert_eq!(
            (at(&n, "/perm"), at(&n, "/tool"), at(&n, "/input/command")),
            (&json!("p1"), &json!("Bash"), &json!("touch x"))
        );
        // a crew run's approvals go to its mate, not to you
        assert!(
            failure(request(&app, None, "approve r-00000c p1").await)
                .is_some_and(|e| e.contains("owned by r-00000a"))
        );
        assert!(
            failure(request(&app, Some("r-00000a"), "approve r-00000c p9").await)
                .is_some_and(|e| e.contains("no call p9"))
        );
        request(&app, Some("r-00000a"), "approve r-00000c p1").await?;
        let answer = hook.await??;
        assert_eq!(
            at(&answer, "/hookSpecificOutput/decision/behavior"),
            "allow"
        );
        assert_eq!(phase(&app, "r-00000c"), Some(Phase::Working));

        let a = Arc::clone(&app);
        let hook = tokio::spawn(async move {
            Ok::<_, crate::Error>(a.hook(&"r-00000c".parse()?, &touch("y")).await)
        });
        assert_eq!(at(&needs(&app, &id).await?, "/perm"), "p2");
        app.act(
            Some("r-00000a".parse()?),
            Cmd::Deny {
                run: id.clone(),
                perm: "p2".into(),
                reason: Some("not y".into()),
            },
        )
        .await?;
        let answer = hook.await??;
        assert_eq!(at(&answer, "/hookSpecificOutput/decision/behavior"), "deny");
        assert_eq!(at(&answer, "/hookSpecificOutput/decision/message"), "not y");
        Ok(())
    }

    // A warm headless process that only takes stdin lines, which the test reads.
    fn warm(app: &App, id: &RunId) -> Result<mpsc::UnboundedReceiver<String>> {
        let sp = harness::spawn(&["cat".to_string()], Path::new("/"), &[], None)?;
        let (tx, rx) = mpsc::unbounded_channel();
        let ident = watch::channel(None).1;
        let p = Proc {
            id: 0,
            machine: app.local.clone(),
            stdin: tx,
            busy: true,
            idle_since: Instant::now(),
            ident,
            _child: sp.child,
        };
        locked(&app.procs).insert(id.clone(), p);
        Ok(rx)
    }

    fn approved(app: &App, id: &RunId) -> usize {
        locked(&app.approved)
            .iter()
            .filter(|(r, ..)| r == id)
            .count()
    }

    // Unanswered before the hook's timeout, a call is denied with "awaiting approval" and the run
    // goes on working. Approved later, only once the run is idle, it is the run's next prompt,
    // and the same call is allowed once, in that turn, without asking.
    #[tokio::test]
    async fn a_late_approval_waits_for_idle_and_allows_the_call_once() -> Result<()> {
        let app = app_with("hook_timeout = 2")?;
        let mut r = run("r-00000e", Role::Task)?;
        r.mode = Mode::Headless;
        r.phase = Phase::Working;
        app.store.save(&r)?;
        let mut stdin = warm(&app, &r.id)?;
        let answer = app.hook(&r.id, &touch("z")).await;
        assert!(
            at(&answer, "/hookSpecificOutput/decision/message")
                .as_str()
                .is_some_and(|m| m.starts_with("awaiting approval"))
        );
        // nothing waits: the run is not left in needs
        assert_eq!(phase(&app, "r-00000e"), Some(Phase::Working));
        let show = request(&app, None, "show r-00000e").await?;
        assert_eq!(at(&show, "/perms/0/state"), "denied, awaiting approval");

        // a turn runs: the approval would be queued behind it, so it is refused and kept
        assert!(
            failure(request(&app, None, "approve r-00000e p1").await)
                .is_some_and(|e| e == "r-00000e is working: approve p1 again once it is idle")
        );
        app.hook(
            &r.id,
            &hook_body("Stop", json!({"last_assistant_message": "waiting"})),
        )
        .await;
        assert_eq!(phase(&app, "r-00000e"), Some(Phase::Idle));
        request(&app, None, "approve r-00000e p1").await?;
        assert!(
            stdin
                .try_recv()
                .is_ok_and(|l| l.contains("the user approved the Bash call"))
        );
        assert_eq!(phase(&app, "r-00000e"), Some(Phase::Working));
        assert!(
            failure(request(&app, None, "approve r-00000e p1").await)
                .is_some_and(|e| e.contains("no call p1"))
        );

        // the model writes another description for the same command
        let mut again = touch("z");
        if let Some(d) = again.pointer_mut("/tool_input/description") {
            *d = json!("again");
        }
        let answer =
            tokio::time::timeout(Duration::from_millis(500), app.hook(&r.id, &again)).await?;
        assert_eq!(
            at(&answer, "/hookSpecificOutput/decision/behavior"),
            "allow"
        );
        assert_eq!((app.store.perms(&r.id)?, approved(&app, &r.id)), (1, 0));

        // an approval the resumed turn did not use ends with that turn
        locked(&app.approved).push((r.id.clone(), "Bash".into(), json!({"command": "touch z"})));
        app.hook(
            &r.id,
            &hook_body("Stop", json!({"last_assistant_message": "done"})),
        )
        .await;
        assert_eq!(approved(&app, &r.id), 0);
        Ok(())
    }

    // An ended run takes no hooks and no approvals: its waiting call is denied, nothing of it
    // stays approvable, and it never comes back.
    #[tokio::test]
    async fn an_ended_run_stays_ended() -> Result<()> {
        let app = app()?;
        let mut r = run("r-00000d", Role::Task)?;
        // on a machine this service does not drive, so done touches no herdr
        r.machine = "elsewhere".parse()?;
        r.phase = Phase::Working;
        app.store.save(&r)?;
        let id = r.id.clone();
        let a = Arc::clone(&app);
        let hook = tokio::spawn(async move {
            Ok::<_, crate::Error>(a.hook(&"r-00000d".parse()?, &touch("x")).await)
        });
        needs(&app, &id).await?;
        app.expire((id.clone(), "p9".into()), "Bash".into(), json!({}));
        locked(&app.approved).push((id.clone(), "Bash".into(), json!({})));

        request(&app, None, "done r-00000d").await?;
        let answer = hook.await??;
        assert_eq!(at(&answer, "/hookSpecificOutput/decision/message"), ENDED);
        assert!(
            locked(&app.expired).is_empty()
                && approved(&app, &id) == 0
                && locked(&app.waiting).is_empty()
        );

        let answer = app.hook(&id, &touch("y")).await;
        assert_eq!(at(&answer, "/hookSpecificOutput/decision/message"), ENDED);
        assert!(
            app.store
                .get(&id)?
                .is_some_and(|r| r.phase == Phase::Done && r.ended_at.is_some())
        );
        assert_eq!(app.store.perms(&id)?, 1);
        assert!(
            failure(request(&app, None, "approve r-00000d p1").await)
                .is_some_and(|e| e == "r-00000d is done: it takes no approvals")
        );
        assert_eq!(at(&request(&app, None, "ls").await?, "/runs"), &json!([]));
        Ok(())
    }

    // Calls drover denied stay approvable for a day, at most EXPIRED_MAX per run.
    #[tokio::test]
    async fn expired_calls_are_bounded() -> Result<()> {
        let app = app()?;
        let (a, b): (RunId, RunId) = ("r-0000d1".parse()?, "r-0000d2".parse()?);
        let old = Instant::now()
            .checked_sub(EXPIRED_FOR + Duration::from_secs(1))
            .ok_or("clock")?;
        locked(&app.expired).insert((b.clone(), "p1".into()), ("Bash".into(), json!({}), old));
        for n in 1..=20 {
            app.expire((a.clone(), format!("p{n}")), "Bash".into(), json!({}));
        }
        let expired = locked(&app.expired);
        assert_eq!(expired.len(), EXPIRED_MAX);
        assert!(
            expired.contains_key(&(a.clone(), "p20".into()))
                && !expired.contains_key(&(a, "p4".into()))
        );
        assert!(!expired.contains_key(&(b, "p1".into())));
        Ok(())
    }

    // A pane run moves along on its agent's hooks alone.
    #[tokio::test]
    async fn hooks_move_a_pane_run() -> Result<()> {
        let app = app()?;
        let mut r = run("r-00000f", Role::Task)?;
        r.phase = Phase::Idle;
        app.store.save(&r)?;
        let mut feed = Feed::new(
            &app,
            Filter {
                run: Some(r.id.clone()),
                ..Filter::default()
            },
        )?;
        app.hook(
            &r.id,
            &hook_body("UserPromptSubmit", json!({"prompt": "go"})),
        )
        .await;
        assert_eq!(phase(&app, "r-00000f"), Some(Phase::Working));
        app.hook(
            &r.id,
            &hook_body("PreToolUse", json!({"tool_name": "Read"})),
        )
        .await;
        app.hook(
            &r.id,
            &hook_body("Stop", json!({"last_assistant_message": " Done. "})),
        )
        .await;
        let r2 = app.get(&r.id)?;
        assert_eq!(
            (r2.phase, r2.session.as_ref().map(SessionId::as_str)),
            (Phase::Idle, Some("317266eb-32ce-4e67-9457-7a097c828473"))
        );
        let kinds: Vec<Value> = [feed.next(&app).await?, feed.next(&app).await?]
            .concat()
            .iter()
            .map(|e| e.get("kind").cloned().unwrap_or_default())
            .collect();
        assert_eq!(kinds, [json!("tool_start"), json!("reply")]);
        assert_eq!(
            app.store
                .last(&r.id, Kind::Reply)?
                .map(|d| d.get("text").cloned()),
            Some(Some(json!("Done.")))
        );
        app.hook(
            &r.id,
            &hook_body("SessionEnd", json!({"reason": "prompt_input_exit"})),
        )
        .await;
        assert_eq!(phase(&app, "r-00000f"), Some(Phase::Done));
        Ok(())
    }

    // A feed reads what was stored after its cursor, then what comes, in order and filtered.
    #[tokio::test]
    async fn the_feed_misses_nothing() -> Result<()> {
        let app = app()?;
        let (a, b) = (run("r-0000b1", Role::Task)?, run("r-0000b2", Role::Crew)?);
        app.store.save(&a)?;
        app.store.save(&b)?;
        app.emit(&a, Kind::Requested, json!({}))?;
        app.emit(&a, Kind::Started, json!({}))?;
        app.emit(&b, Kind::Started, json!({}))?;
        let mut all = Feed::new(
            &app,
            Filter {
                since: Some(0),
                ..Filter::default()
            },
        )?;
        let mut crew = Feed::new(
            &app,
            Filter {
                role: Some(Role::Crew),
                ..Filter::default()
            },
        )?;
        let first = all.next(&app).await?;
        assert_eq!(
            first
                .iter()
                .map(|e| at(e, "/run").clone())
                .collect::<Vec<_>>(),
            [json!("r-0000b1"), json!("r-0000b2")]
        );
        app.emit(
            &a,
            Kind::Reply,
            json!({ "text": "hi", "usage": Usage::default() }),
        )?;
        app.emit(&b, Kind::Done, json!({}))?;
        let next = Value::from(all.next(&app).await?);
        assert_eq!(
            (
                at(&next, "/0/kind"),
                at(&next, "/0/usage/input"),
                at(&next, "/0/data/usage")
            ),
            (&json!("reply"), &json!(0), &Value::Null)
        );
        assert_eq!(at(&Value::from(crew.next(&app).await?), "/0/kind"), "done");
        Ok(())
    }

    // A process a killed service left running is ended before its run resumes, and only its own.
    #[tokio::test]
    async fn a_leftover_process_is_ended_before_a_resume() -> Result<()> {
        let app = app()?;
        let left = |run: &str| -> Result<(harness::Spawned, Ident, RunId)> {
            let argv = ["sleep".to_string(), "30".to_string()];
            let sp = harness::spawn(&argv, Path::new("/"), &[], None)?;
            let ident = Ident {
                pid: sp.child.id().ok_or("no pid")?,
                start: String::new(),
            };
            Ok((sp, ident, run.parse()?))
        };
        let (mut a, mut ia, ra) = left("r-0000c1")?;
        let (mut b, mut ib, rb) = left("r-0000c2")?;
        for (sp, ident) in [(&mut a, &mut ia), (&mut b, &mut ib)] {
            let mut line = String::new();
            BufReader::new(&mut sp.stderr).read_line(&mut line).await?;
            *ident = Ident::parse(&line).ok_or("no ident")?;
        }
        app.store.remember(&ra, &app.local, &ia)?;
        app.store.remember(&rb, &app.local, &ib)?;
        app.reap(Some(&ra)).await?;
        assert!(a.child.try_wait()?.is_some(), "the run's leftover is gone");
        assert!(b.child.try_wait()?.is_none(), "another run's is not");
        assert_eq!(app.store.procs(None)?.len(), 1);
        app.reap(None).await?;
        assert!(app.store.procs(None)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn later_nodes_are_stubs() -> Result<()> {
        let app = app()?;
        for line in [
            "run --scratch --prompt go",
            "run --harness codex --repo r --branch b --prompt go",
        ] {
            assert_eq!(
                failure(request(&app, None, line).await).as_deref(),
                Some("not implemented"),
                "{line}"
            );
        }
        for (line, error) in [
            (
                "run --machine elsewhere --repo r --branch b --prompt go",
                "unknown machine elsewhere",
            ),
            ("run --prompt go", "an actor runs in a worktree"),
            (
                "run --profile reader --repo r --branch b --prompt go",
                "a reader runs on this machine, in the state dir's reader/",
            ),
        ] {
            assert_eq!(
                failure(request(&app, None, line).await).as_deref(),
                Some(error),
                "{line}"
            );
        }
        Ok(())
    }
}
