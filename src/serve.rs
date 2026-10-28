// The service: what each command does to the runs, and one event watcher per machine that moves
// pane runs along as herdr pushes their agents' status. Nothing polls.

use std::{os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command, sync::Notify, time::sleep};

use crate::{
    Result,
    cli::{Cmd, Ls, Start},
    config::{Config, Paths},
    control::{self, Failure},
    herdr::{self, Host, Name, PaneId, Reach, SessionId, Status},
    migrate,
    run::{HarnessName, Machine, Mode, Owner, Phase, Profile, Role, Run, RunId, Usage, now},
    socket,
    store::{Kind, Store},
};

// Characters of a reply its event keeps.
const REPLY_EXCERPT: usize = 300;
// Appended to a crewmate's first prompt: the end of each turn is what its mate reads.
const CREW: &str = "You are a crewmate. Your mate reads the end of each of your turns. The brief above is your only source of facts: do not assume what it does not say. When blocked or unsure, stop and end your turn with a line starting QUESTION:. When the check in the brief passes, end your turn with DONE: and its output. Do not change the check. Do not end your turn while a check or background job you started still runs: wait for it, or end with a line starting RUNNING: and what runs.";

pub struct App {
    hosts: Vec<Host>, // this machine first
    local: Machine,
    store: Store,
    // exported to local agents, so their drover calls reach this service
    state: PathBuf,
    // the pane set changed: watchers resubscribe
    wake: Notify,
    // anything changed: `watch` sends a new snapshot
    pub changed: Notify,
    // a turn's end is read and the next prompt typed one at a time, so neither misses the other
    turns: tokio::sync::Mutex<()>,
}

pub async fn run(paths: &Paths) -> Result<()> {
    require_runtime()?;
    let config = Config::load(&paths.config)?;
    std::fs::create_dir_all(&paths.state)?;
    std::fs::set_permissions(&paths.state, std::fs::Permissions::from_mode(0o700))?;
    let db = paths.db();
    let first = !db.exists();
    let store = Store::open(&db)?;
    if first && let Some(v1) = &config.v1_state {
        match migrate::migrate(&store, v1, now()) {
            Ok(n) => println!("migrated {n} runs from {}", v1.display()),
            Err(e) => {
                // the next start tries again on an empty store
                drop(store);
                std::fs::remove_file(&db)?;
                bail!("migrate {}: {e}", v1.display());
            }
        }
    }
    let app = Arc::new(App::new(&config, store, paths.state.clone()));
    let names: Vec<&str> = app.hosts.iter().map(|h| h.name.as_str()).collect();
    println!(
        "drover serves {} on {}",
        paths.socket().display(),
        names.join(", ")
    );
    for h in app.hosts.clone() {
        let app = Arc::clone(&app);
        tokio::spawn(async move { watch_machine(&app, &h).await });
    }
    control::serve(app, &paths.socket()).await
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

// A run as `ls` lists it.
fn row(r: &Run) -> Value {
    json!({
        "run": r.id, "machine": r.machine, "role": r.role, "phase": r.phase,
        "harness": r.harness, "mode": r.mode.as_str(), "cwd": r.cwd,
    })
}

// What a herdr status says about a run's pane.
enum Seen {
    Agent {
        status: Status,
        session: Option<SessionId>,
        cwd: Option<String>,
    },
    Gone,
}

impl App {
    fn new(config: &Config, store: Store, state: PathBuf) -> Self {
        let root = config.repos_root.clone();
        let host = Host::new(config.host.to_string(), Reach::Local, root.clone());
        let remotes = config
            .remotes
            .iter()
            .map(|m| Host::new(m.to_string(), Reach::Ssh, root.clone()));
        Self {
            hosts: std::iter::once(host).chain(remotes).collect(),
            local: config.host.clone(),
            store,
            state,
            wake: Notify::new(),
            changed: Notify::new(),
            turns: tokio::sync::Mutex::new(()),
        }
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

    // Saves and tells `watch`; a run that gains or loses its pane also wakes the watchers.
    fn save(&self, r: &Run) -> Result<()> {
        self.store.save(r)?;
        self.changed.notify_waiters();
        Ok(())
    }

    fn event(&self, r: &Run, kind: Kind, data: &Value) -> Result<()> {
        self.store.event(&r.id, kind, data)
    }

    fn end(&self, r: &mut Run, phase: Phase, kind: Kind, data: &Value) -> Result<()> {
        r.phase = phase;
        r.ended_at = Some(now());
        self.event(r, kind, data)?;
        self.save(r)?;
        self.wake.notify_waiters();
        Ok(())
    }

    // Live pane runs on a machine, with their pane.
    fn live_on(&self, machine: &str) -> Result<Vec<(Run, PaneId)>> {
        Ok(self
            .store
            .list()?
            .into_iter()
            .filter(|r| r.machine.as_str() == machine && r.phase.live())
            .filter_map(|r| r.mode.pane().cloned().map(|p| (r, p)))
            .collect())
    }

    fn panes_on(&self, machine: &str) -> Vec<PaneId> {
        let mut panes: Vec<PaneId> = self
            .live_on(machine)
            .unwrap_or_default()
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        panes.sort_by(|a, b| a.0.cmp(&b.0));
        panes.dedup();
        panes
    }

    pub async fn act(&self, from: Option<RunId>, cmd: Cmd) -> Result<Value> {
        let from = from.as_ref();
        match cmd {
            Cmd::Run(s) => self.cmd_run(s).await,
            Cmd::Ls(l) => self.cmd_ls(&l),
            Cmd::Show { run } => self.cmd_show(&run),
            Cmd::Send { run, text } => self.cmd_send(from, &run, &text).await,
            Cmd::ApproveRun { run } => self.cmd_approve_run(from, &run).await,
            Cmd::DropRun { run } => self.cmd_drop_run(from, &run),
            Cmd::Attach { run } => self.cmd_attach(from, &run),
            Cmd::Screen { run, lines } => self.cmd_screen(&run, lines).await,
            Cmd::Keys { run, keys } => self.cmd_keys(from, &run, &keys).await,
            Cmd::Stop { run } => self.cmd_keys(from, &run, &["esc".into()]).await,
            Cmd::Done { run, remove, entry } => {
                self.cmd_done(from, &run, remove, entry.as_deref()).await
            }
            Cmd::Snapshot => self.snapshot(),
            Cmd::Home => self.home(),
            Cmd::Approve { .. }
            | Cmd::Deny { .. }
            | Cmd::Wait { .. }
            | Cmd::Goal { .. }
            | Cmd::Escalate { .. } => Err(not_implemented()),
            Cmd::Watch | Cmd::Serve => bail!("not a request"),
        }
    }

    async fn cmd_run(&self, s: Start) -> Result<Value> {
        if s.headless
            || s.scratch
            || s.profile != Profile::Actor
            || s.harness != HarnessName::Claude
        {
            return Err(not_implemented());
        }
        let machine = s.machine.unwrap_or_else(|| self.local.clone());
        self.host(&machine)?;
        let repo = Name::parse(s.repo.as_deref().unwrap_or_default())?;
        let branch = Name::parse(s.branch.as_deref().unwrap_or_default())?;
        if let Some(p) = &s.parent {
            self.get(p)?;
        }
        let pending = s.parent.is_some();
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
            mode: Mode::Pane { pane: None },
            role: if pending { Role::Crew } else { Role::Task },
            owner: s.parent.clone().map_or(Owner::User, Owner::Run),
            parent: s.parent,
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
            _ => s.prompt,
        };
        self.save(&run)?;
        self.event(
            &run,
            Kind::Requested,
            &json!({ "repo": repo.as_str(), "branch": branch.as_str(), "prompt": prompt }),
        )?;
        if !pending {
            self.start(&mut run, &repo, &branch).await?;
        }
        Ok(self.brief(&run))
    }

    // Opens the worktree and starts the agent; the first prompt goes out on its first ready push.
    async fn start(&self, run: &mut Run, repo: &Name, branch: &Name) -> Result<()> {
        run.phase = Phase::Starting;
        self.save(run)?;
        if let Err(e) = self.open(run, repo, branch).await {
            self.end(
                run,
                Phase::Failed,
                Kind::Failed,
                &json!({ "error": e.to_string() }),
            )?;
            return Err(e);
        }
        Ok(())
    }

    async fn open(&self, run: &mut Run, repo: &Name, branch: &Name) -> Result<()> {
        let h = self.host(&run.machine)?;
        let wt = herdr::worktree(h, repo, branch).await?;
        let taken = self
            .live_on(&h.name)?
            .into_iter()
            .find(|(r, p)| *p == wt.pane_id && r.id != run.id);
        if let Some((other, _)) = taken {
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
        run.mode = Mode::Pane {
            pane: Some(wt.pane_id.clone()),
        };
        run.cwd = PathBuf::from(&wt.path);
        self.save(run)?;
        self.wake.notify_waiters();
        let state = self.state.to_string_lossy();
        let mut env = vec![("DROVER_RUN", run.id.as_str())];
        if h.reach == Reach::Local {
            env.push(("DROVER_STATE_DIR", &state));
        }
        herdr::start(
            h,
            run.id.as_str(),
            run.harness.as_str(),
            &wt.pane_id,
            &wt.path,
            &env,
        )
        .await?;
        Ok(())
    }

    // What a command that starts or changes a run answers.
    fn brief(&self, r: &Run) -> Value {
        let id = &r.id;
        let help: Vec<String> = match r.phase {
            Phase::Pending => vec![
                format!("drover approve-run {id}"),
                format!("drover drop-run {id}"),
            ],
            Phase::Needs => vec![
                format!("drover screen {id}"),
                format!("drover keys {id} <keys>"),
            ],
            Phase::Idle => vec![
                format!("drover send {id} <text>"),
                format!("drover done {id}"),
            ],
            Phase::Starting | Phase::Working => {
                vec![format!("drover show {id}"), format!("drover screen {id}")]
            }
            Phase::Done | Phase::Failed => vec![format!("drover show {id}")],
        };
        json!({
            "run": id, "machine": r.machine, "phase": r.phase, "pane": r.mode.pane().map(|p| &p.0),
            "cwd": r.cwd, "owner": r.owner.to_string(), "help": help,
        })
    }

    fn cmd_ls(&self, l: &Ls) -> Result<Value> {
        let all = self.store.list()?;
        let total = all.len();
        let runs: Vec<Value> = all
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

    fn cmd_show(&self, id: &RunId) -> Result<Value> {
        let r = self.get(id)?;
        let children: Vec<RunId> = self
            .store
            .list()?
            .into_iter()
            .filter(|c| c.parent.as_ref() == Some(id))
            .map(|c| c.id)
            .collect();
        let reply = self
            .store
            .last(id, Kind::Reply)?
            .map(|d| json!({ "text": d.get("text"), "chars": d.get("chars") }));
        let mut out = self.brief(&r);
        let fields = json!({
            "harness": r.harness, "mode": r.mode.as_str(), "role": r.role, "parent": r.parent,
            "profile": r.profile, "session": r.session.as_ref().map(SessionId::as_str),
            "usage": r.usage, "created_at": r.created_at, "ended_at": r.ended_at,
            "children": children, "reply": reply,
        });
        if let (Value::Object(out), Value::Object(fields)) = (&mut out, fields) {
            // help stays last
            let help = out.remove("help");
            out.extend(fields);
            out.extend(help.map(|h| ("help".to_string(), h)));
        }
        Ok(out)
    }

    // The machine and pane of a run whose agent is up.
    fn pane_of(&self, r: &Run) -> Result<(&Host, PaneId)> {
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
        let (h, pane) = self.pane_of(&r)?;
        // a turn with no text to read leaves the run working, so herdr says whether the agent is busy
        if r.phase == Phase::Needs {
            return Err(Failure::new(
                format!("{id} waits on a dialog"),
                format!("drover screen {id}"),
            )
            .into());
        }
        // a turn that ended before its push came is read now, or the next prompt would hide it.
        // Claude queues a prompt typed mid-turn, so a turn still running refuses the send; one
        // that ended with no text (an interrupt) is idle in herdr's eyes.
        if r.phase == Phase::Working {
            self.collect(h, &mut r, false).await?;
        }
        if r.phase == Phase::Working && !ready(h, &pane).await? {
            return Err(Failure::new(
                format!("{id} is working: wait for its reply"),
                format!("drover show {id}"),
            )
            .into());
        }
        let before = r.phase;
        r.phase = Phase::Working;
        self.save(&r)?;
        if let Err(e) = prompt(h, &pane, text).await {
            r.phase = before;
            self.save(&r)?;
            return Err(e.into());
        }
        Ok(self.brief(&r))
    }

    async fn cmd_approve_run(&self, from: Option<&RunId>, id: &RunId) -> Result<Value> {
        Self::by_you(from, "approve runs")?;
        let mut r = self.get(id)?;
        if r.phase != Phase::Pending {
            return Err(Failure::new(
                format!("{id} is {}, not pending", r.phase.as_str()),
                format!("drover show {id}"),
            )
            .into());
        }
        let asked = self.store.last(id, Kind::Requested)?.unwrap_or_default();
        let name = |k: &str| Name::parse(asked.get(k).and_then(Value::as_str).unwrap_or_default());
        self.start(&mut r, &name("repo")?, &name("branch")?).await?;
        Ok(self.brief(&r))
    }

    fn cmd_drop_run(&self, from: Option<&RunId>, id: &RunId) -> Result<Value> {
        Self::by_you(from, "drop runs")?;
        let r = self.get(id)?;
        if r.phase != Phase::Pending {
            return Err(Failure::new(
                format!("{id} is {}, not pending", r.phase.as_str()),
                format!("drover done {id}"),
            )
            .into());
        }
        self.store.delete(id)?;
        self.changed.notify_waiters();
        Ok(json!({ "run": id, "dropped": true }))
    }

    fn cmd_attach(&self, from: Option<&RunId>, id: &RunId) -> Result<Value> {
        Self::by_you(from, "attach")?;
        let mut r = self.get(id)?;
        let (h, pane) = self.pane_of(&r)?;
        let noop = (r.owner == Owner::User).then_some("yours already");
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

    async fn cmd_done(
        &self,
        from: Option<&RunId>,
        id: &RunId,
        remove: bool,
        entry: Option<&str>,
    ) -> Result<Value> {
        let mut r = self.get(id)?;
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
            _ => {}
        }
        // the entry is logged before anything closes, so a failed log loses nothing
        if let Some(entry) = entry {
            brain_log(entry).await?;
        }
        let mut notes = Vec::new();
        let mut removed = false;
        match (self.host(&r.machine), r.mode.pane().cloned(), r.workspace()) {
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
                if let Err(e) = closed {
                    notes.push(format!("close failed: {e}"));
                }
            }
            (Err(e), ..) => notes.push(e.to_string()),
            _ => {}
        }
        self.end(
            &mut r,
            Phase::Done,
            Kind::Done,
            &json!({ "entry": entry.is_some(), "removed": removed }),
        )?;
        let note = notes.join("\n");
        Ok(
            json!({ "run": id, "phase": r.phase, "removed": removed, "note": (!note.trim().is_empty()).then_some(note.trim()) }),
        )
    }

    pub fn snapshot(&self) -> Result<Value> {
        let runs: Vec<Value> = self
            .store
            .list()?
            .iter()
            .map(|r| {
                let mut v = row(r);
                if let Value::Object(m) = &mut v {
                    m.insert("pane".into(), json!(r.mode.pane().map(|p| &p.0)));
                    m.insert("parent".into(), json!(r.parent));
                    m.insert("owner".into(), json!(r.owner.to_string()));
                    m.insert(
                        "reply".into(),
                        self.store
                            .last(&r.id, Kind::Reply)
                            .ok()
                            .flatten()
                            .and_then(|d| d.get("text").cloned())
                            .unwrap_or_default(),
                    );
                }
                v
            })
            .collect();
        let machines: Vec<&str> = self.hosts.iter().map(|h| h.name.as_str()).collect();
        Ok(json!({ "machines": machines, "runs": runs }))
    }

    fn home(&self) -> Result<Value> {
        let runs = self.store.list()?;
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
            .map(row)
            .collect();
        let mut help = Vec::new();
        if runs.iter().any(|r| r.phase == Phase::Pending) {
            help.push("drover approve-run <run>");
        }
        if runs.iter().any(|r| r.phase == Phase::Needs) {
            help.push("drover screen <run>");
        }
        help.extend([
            "drover ls",
            "drover run --repo <repo> --branch <branch> --prompt <prompt>",
            "drover <command> --help",
        ]);
        Ok(json!({ "runs": phases, "needs": needs, "help": help }))
    }

    // ── events: herdr pushes status per pane; agent.list only on (re)subscribe ──

    async fn reconcile(&self, h: &Host) {
        let Ok(list) = herdr::agents(h).await else {
            return; // unreachable, the watcher retries
        };
        let panes = herdr::panes(h).await.ok();
        for (r, pane) in self.live_on(&h.name).unwrap_or_default() {
            let seen = match list.iter().find(|a| a.pane_id == pane) {
                Some(a) => Seen::Agent {
                    status: a.agent_status,
                    session: a.session(),
                    cwd: Some(a.cwd.clone()),
                },
                // still starting while its pane is there
                None if r.phase == Phase::Starting
                    && panes.as_ref().is_none_or(|p| p.contains(&pane)) =>
                {
                    continue;
                }
                None => Seen::Gone,
            };
            self.step(h, r, &pane, seen).await;
        }
    }

    async fn on_push(&self, h: &Host, push: socket::Push) {
        let live = |pane: &PaneId| {
            self.live_on(&h.name)
                .unwrap_or_default()
                .into_iter()
                .find(|(_, p)| p == pane)
        };
        match push {
            socket::Push::Status(pane, status) if status != Status::Unknown => {
                let Some((r, pane)) = live(&pane) else { return };
                // agent.list knows the session
                if r.session.is_none() {
                    return self.reconcile(h).await;
                }
                let seen = Seen::Agent {
                    status,
                    session: None,
                    cwd: None,
                };
                self.step(h, r, &pane, seen).await;
            }
            socket::Push::Gone(pane) => {
                if let Some((r, pane)) = live(&pane) {
                    self.step(h, r, &pane, Seen::Gone).await;
                }
            }
            // the agent may have left or restarted
            socket::Push::Status(pane, _) | socket::Push::Detected(pane) => {
                if live(&pane).is_some() {
                    self.reconcile(h).await;
                }
            }
            socket::Push::Reconcile => self.reconcile(h).await,
            socket::Push::Lost => {}
        }
    }

    async fn step(&self, h: &Host, r: Run, pane: &PaneId, seen: Seen) {
        let _turn = self.turns.lock().await;
        // a send may have moved it on while this waited
        let fresh = match self.store.get(&r.id) {
            Ok(Some(r)) if r.phase.live() => r,
            _ => return,
        };
        let id = r.id;
        if let Err(e) = self.advance(h, fresh, pane, seen).await {
            eprintln!("run {id}: {e}");
        }
    }

    // Reads the turn that ended, as a reply event, and the run goes idle. `wait` gives a turn
    // still being written time to end; without it only an ended turn counts.
    async fn collect(&self, h: &Host, r: &mut Run, wait: bool) -> Result<()> {
        let Some(s) = &r.session else { return Ok(()) };
        let last = self.store.last(&r.id, Kind::Reply)?;
        let last = last.as_ref().and_then(|d| d.get("id")?.as_str());
        let reply = match wait {
            true => fresh_reply(h, s, last).await?,
            false => {
                let turn = herdr::reply(h, s).await?;
                turn.reply
                    .filter(|t| !turn.open && Some(t.id.as_str()) != last)
            }
        };
        if let Some(reply) = reply {
            let text = reply.text.trim();
            let excerpt = json!({ "id": reply.id, "text": head(text, REPLY_EXCERPT), "chars": text.chars().count() });
            self.event(r, Kind::Reply, &excerpt)?;
            r.phase = Phase::Idle;
        }
        Ok(())
    }

    // One run's turn: the phase follows the agent's status, the first prompt goes out once it
    // is ready, and a turn's end is read from the transcript.
    async fn advance(&self, h: &Host, mut r: Run, pane: &PaneId, seen: Seen) -> Result<()> {
        let Seen::Agent {
            status,
            session,
            cwd,
        } = seen
        else {
            return self.end(&mut r, Phase::Done, Kind::Done, &json!({ "by": "exit" }));
        };
        let before = r.clone();
        if let Some(s) = session {
            r.session = Some(s);
        }
        if let Some(cwd) = cwd {
            r.cwd = cwd.into();
        }
        let asked = self.store.last(&r.id, Kind::Requested)?;
        let unsent = asked.is_some() && self.store.last(&r.id, Kind::Started)?.is_none();
        match status {
            Status::Blocked if r.phase != Phase::Needs => {
                r.phase = Phase::Needs;
                self.event(&r, Kind::Needs, &json!({ "dialog": true }))?;
            }
            Status::Working if !unsent && matches!(r.phase, Phase::Idle | Phase::Needs) => {
                r.phase = Phase::Working
            }
            s if s.ready() && unsent => {
                let first = asked
                    .as_ref()
                    .and_then(|a| a.get("prompt")?.as_str())
                    .unwrap_or_default();
                match prompt(h, pane, first).await {
                    Ok(()) => {
                        r.phase = Phase::Working;
                        self.event(&r, Kind::Started, &json!({}))?;
                    }
                    Err(e) if e.is_busy() => {}
                    Err(e) => {
                        return self.end(
                            &mut r,
                            Phase::Failed,
                            Kind::Failed,
                            &json!({ "error": format!("first prompt: {e}") }),
                        );
                    }
                }
            }
            // a ready push with no fresh reply came before the turn began: it stays working
            s if s.ready() && matches!(r.phase, Phase::Working | Phase::Needs) => {
                r.phase = Phase::Working;
                self.collect(h, &mut r, true).await?;
            }
            _ => {}
        }
        if r != before {
            self.save(&r)?;
        }
        Ok(())
    }
}

// Types a prompt and returns once herdr sees the turn start, or after 3 s for one too short to see.
// Until then the agent still looks idle, and a second prompt would slip in behind this one.
async fn prompt(h: &Host, pane: &PaneId, text: &str) -> herdr::Result<()> {
    herdr::prompt(h, pane, text).await?;
    for _ in 0..30 {
        match ready(h, pane).await {
            Ok(true) => sleep(Duration::from_millis(100)).await,
            _ => break,
        }
    }
    Ok(())
}

// Whether herdr sees the pane's agent idle or done.
async fn ready(h: &Host, pane: &PaneId) -> herdr::Result<bool> {
    let agents = herdr::agents(h).await?;
    Ok(agents
        .iter()
        .any(|a| &a.pane_id == pane && a.agent_status.ready()))
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

// The transcript trails the status flip: while the turn is open its last text may not be written
// yet, and the newest text is one from the middle of the turn. A turn with no end line coming
// (an interrupt, a local command) is not open, so this waits only on a turn being written.
async fn fresh_reply(h: &Host, s: &SessionId, last: Option<&str>) -> Result<Option<herdr::Reply>> {
    let mut newest = None;
    for _ in 0..20 {
        let turn = herdr::reply(h, s).await?;
        let fresh = turn.reply.filter(|r| Some(r.id.as_str()) != last);
        if !turn.open {
            return Ok(fresh);
        }
        newest = fresh.or(newest);
        sleep(Duration::from_millis(250)).await;
    }
    Ok(newest)
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

async fn watch_session(app: &App, h: &Host) -> Result<()> {
    let watched = loop {
        // enabled before the check, so a pane added after it still wakes us
        let woke = app.wake.notified();
        tokio::pin!(woke);
        woke.as_mut().enable();
        let panes = app.panes_on(&h.name);
        if !panes.is_empty() {
            break panes;
        }
        woke.await;
    };

    // herdr rejects a subscription naming a pane that is gone; reconcile marks those
    let sub: Vec<PaneId> = match herdr::panes(h).await {
        Ok(live) => watched
            .iter()
            .filter(|p| live.contains(p))
            .cloned()
            .collect(),
        Err(_) => watched.clone(),
    };
    let mut conn = socket::Events::subscribe(h.ssh(), &sub).await?;
    // subscribe first, then read, so nothing between the two is missed
    app.reconcile(h).await;

    loop {
        let woke = app.wake.notified();
        tokio::pin!(woke);
        woke.as_mut().enable();
        if app.panes_on(&h.name) != watched {
            return Ok(()); // resubscribe with the new pane set
        }
        tokio::select! {
            ev = conn.recv() => match ev? {
                socket::Push::Lost => bail!("events_lost on {}", h.name),
                push => app.on_push(h, push).await,
            },
            () = &mut woke => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn app() -> Result<App> {
        let config = Config::parse("host = \"box\"")?;
        Ok(App::new(
            &config,
            Store::open(Path::new(":memory:"))?,
            PathBuf::from("/tmp/x"),
        ))
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

    fn failure(r: Result<Value>) -> Option<String> {
        r.err().map(|e| Failure::from(e).error)
    }

    // A run a mate asks for waits for you; dropping it leaves nothing.
    #[tokio::test]
    async fn a_requested_run_waits_for_you() -> Result<()> {
        let app = app()?;
        let mate = Run {
            id: "r-00000a".parse()?,
            machine: "box".parse()?,
            cwd: "/w".into(),
            harness: HarnessName::Claude,
            mode: Mode::Pane {
                pane: Some(PaneId("w1:p1".into())),
            },
            role: Role::Mate,
            parent: None,
            profile: Profile::Actor,
            owner: Owner::User,
            session: None,
            phase: Phase::Idle,
            usage: Usage::default(),
            created_at: 1,
            ended_at: None,
        };
        app.store.save(&mate)?;
        let out = request(
            &app,
            Some("r-00000a"),
            "run --repo drover --branch x --parent r-00000a --prompt go",
        )
        .await?;
        assert_eq!(at(&out, "/phase"), "pending");
        assert_eq!(at(&out, "/owner"), "r-00000a");
        let id = at(&out, "/run").as_str().unwrap_or_default().to_string();

        let ls = request(&app, None, "ls --phase pending").await?;
        assert_eq!(at(&ls, "/runs/0/role"), "crew");
        let asked = app
            .store
            .last(&id.parse()?, Kind::Requested)?
            .unwrap_or_default();
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

    #[tokio::test]
    async fn later_nodes_are_stubs() -> Result<()> {
        let app = app()?;
        for line in [
            "approve r-00000a p1",
            "deny r-00000a p1",
            "wait --any",
            "goal set --file g.json",
            "escalate --text why",
            "run --scratch --prompt go",
            "run --headless --repo r --branch b --prompt go",
            "run --harness cursor --repo r --branch b --prompt go",
            "run --profile reader --repo r --branch b --prompt go",
        ] {
            assert_eq!(
                failure(request(&app, None, line).await).as_deref(),
                Some("not implemented"),
                "{line}"
            );
        }
        assert!(
            failure(
                request(
                    &app,
                    None,
                    "run --machine elsewhere --repo r --branch b --prompt go"
                )
                .await
            )
            .is_some_and(|e| e == "unknown machine elsewhere")
        );
        Ok(())
    }
}
