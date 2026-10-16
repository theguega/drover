// What drover does, once. Discord and the terminal both build an Action and print the text run returns.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serenity::all::{ChannelId, Http};
use tokio::time::sleep;

use super::*;

#[derive(Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Action {
    New(New),
    Attach(Attach),
    Agents,
    Worktrees,
    Usage,
    Config,
    Screen(Target),
    Mate(Mate),
    Take(Target),
    Send(Send),
    Release(Target),
    Keys(Keys),
    Done(Done),
    Recall(Recall),
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
}

#[derive(Serialize, Deserialize)]
pub struct Mate {
    pub name: String,
}

#[derive(Serialize, Deserialize)]
pub struct Attach {
    pub machine: String,
    pub pane: String,
}

// A task is addressed by its Discord thread id, as `agents` prints it.
#[derive(Serialize, Deserialize)]
pub struct Target {
    pub thread: String,
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
}

// Most mates a desk runs at once.
const MAX_MATES: usize = 3;

// What a mate is told before its first message: its instructions live in mates/AGENTS.md.
const MATE_ROLE: &str = "You are a mate. Your instructions are in AGENTS.md in this folder. Wait for the desk's first message in this thread.";

// Agent names take 32 characters, and "mate-" takes five of them.
fn mate_name(s: &str) -> Result<String> {
    let valid = s.len() <= 26
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if valid {
        Ok(s.into())
    } else {
        bail!(
            "a mate name is lowercase letters, digits, - or _, starting with a letter, up to 26 characters"
        )
    }
}

impl App {
    pub async fn act(&self, http: &Http, action: Action) -> Result<String> {
        match action {
            Action::New(n) => self.cmd_new(http, &n).await,
            Action::Attach(a) => self.cmd_attach(http, &a).await,
            Action::Agents => self.cmd_agents().await,
            Action::Worktrees => self.cmd_worktrees().await,
            Action::Usage => self.cmd_usage().await,
            Action::Config => Ok(self.config()),
            Action::Mate(m) => self.cmd_mate(http, &m.name).await,
            Action::Send(s) => self.cmd_send(&s).await,
            Action::Take(t) => self.set_owner(&t.thread, Owner::User).await,
            Action::Release(t) => self.set_owner(&t.thread, Owner::Mate).await,
            Action::Screen(t) => {
                let (m, pane) = self.target(id(&t.thread)?).await?;
                Ok(screen(m, &pane, "").await)
            }
            Action::Keys(k) => {
                let (m, pane) = self.target(id(&k.thread)?).await?;
                let keys: Vec<&str> = k.keys.iter().map(String::as_str).collect();
                herdr::keys(m, &pane, &keys).await?;
                sleep(Duration::from_millis(700)).await;
                Ok(screen(m, &pane, "").await)
            }
            Action::Done(d) => self.cmd_done(http, &d).await,
            Action::Recall(r) => {
                let hits = self.journal.search(&r.query, 5).await?;
                Ok(match hits.is_empty() {
                    true => "nothing".into(),
                    false => hits
                        .iter()
                        .map(journal::format)
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                })
            }
        }
    }

    async fn cmd_new(&self, http: &Http, n: &New) -> Result<String> {
        let m = self.machine(&n.machine)?;
        let repo = Name::parse(&n.repo)?;
        let r#ref = Name::parse(&n.branch)?;
        let kind = harness::find(n.agent.as_deref().unwrap_or(harness::DEFAULT))
            .context("unknown agent, see drover help")?
            .name;
        let model = n.model.as_deref().map(Name::parse).transpose()?;

        let wt = herdr::worktree(m, &repo, &r#ref).await?;
        if let Some(bound) = self.live(&m.name, &wt.pane_id).await {
            return Ok(format!("already open in <#{bound}>"));
        }
        if herdr::agents(m)
            .await?
            .iter()
            .any(|a| a.pane_id == wt.pane_id)
        {
            return Ok(format!(
                "an agent already runs in {}, attach to it instead",
                wt.pane_id.0
            ));
        }

        let repo = repo.as_str();
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

        let memory = self.journal.context(repo, &n.prompt, 2000).await?;
        let mut prompt = if n.scout {
            format!("{SCOUT}\n\n{}", n.prompt)
        } else {
            n.prompt.clone()
        };
        if !memory.is_empty() {
            prompt = format!(
                "{prompt}\n\nContext from earlier tasks (journal, may be stale):\n{memory}"
            );
        }
        self.insert(
            th,
            Task {
                machine: m.name.clone(),
                pane: wt.pane_id.clone(),
                workspace: wt.workspace_id,
                kind: kind.into(),
                repo: repo.into(),
                branch: wt.branch.clone(),
                origin: Origin::Opened,
                owner: Owner::User,
                mate: None,
                phase: Phase::Starting { prompt },
                status: None,
                session: None,
                last_reply: None,
                prompt_msg: Some(head.id),
                dialog_msg: None,
                queued: None,
            },
        );
        self.save().await;
        th.create_reaction(http, head.id, emoji("👀")).await?;
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
        Ok(format!("<#{th}>"))
    }

    async fn cmd_attach(&self, http: &Http, at: &Attach) -> Result<String> {
        let m = self.machine(&at.machine)?;
        let pane = PaneId(at.pane.clone());
        let a = herdr::agents(m)
            .await?
            .into_iter()
            .find(|x| x.pane_id == pane)
            .context(format!("no agent in {} on {}", pane.0, m.name))?;
        if let Some(bound) = self.live(&m.name, &pane).await {
            return Ok(format!("already open in <#{bound}>"));
        }

        let repo = a.cwd.rsplit('/').next().unwrap_or_default().to_string();
        let th = self
            .open_thread(http, &format!("{repo} · {} · {}", a.agent, m.name))
            .await?;
        let session = a.session();
        let last_reply = match &session {
            Some(s) => herdr::reply(m, s).await?.map(|r| r.id),
            None => None,
        };
        self.insert(
            th,
            Task {
                machine: m.name.clone(),
                pane: pane.clone(),
                workspace: a.workspace_id,
                kind: a.agent,
                repo,
                branch: String::new(),
                origin: Origin::Attached,
                owner: Owner::User,
                mate: None,
                phase: Phase::Idle,
                status: Some(a.agent_status),
                session,
                last_reply,
                prompt_msg: None,
                dialog_msg: None,
                queued: None,
            },
        );
        self.save().await;
        th.send_message(http, screen_msg(m, &pane).await).await?;
        Ok(format!("<#{th}>"))
    }

    async fn cmd_agents(&self) -> Result<String> {
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
                let bound = self
                    .live(&m.name, &a.pane_id)
                    .await
                    .map(|b| format!(" <#{b}>"))
                    .unwrap_or_default();
                let icon = a.agent_status.icon();
                lines.push(format!(
                    "`{icon} {}` {} {}{bound}",
                    a.pane_id.0,
                    a.agent,
                    home(&a.cwd)
                ));
            }
        }
        Ok(lines.join("\n"))
    }

    async fn cmd_usage(&self) -> Result<String> {
        let jobs: Vec<_> = self
            .machines()
            .cloned()
            .map(|m| tokio::spawn(async move { usage_lines(&m).await }))
            .collect();
        let mut lines = vec![];
        for job in jobs {
            lines.extend(job.await?);
        }
        Ok(lines.join("\n"))
    }

    // What this service runs with, so a terminal or an agent can see the setup without the .env.
    fn config(&self) -> String {
        let remotes: Vec<&str> = self.remotes.iter().map(|m| m.name.as_str()).collect();
        let remotes = match remotes.is_empty() {
            true => "none".into(),
            false => remotes.join(", "),
        };
        [
            format!("host: {}", self.host.name),
            format!("remotes: {remotes}"),
            format!("channel: <#{}>", self.channel),
            format!(
                "repos: {}",
                std::env::var("REPOS_ROOT").unwrap_or_else(|_| "~/.drover-workspace".into())
            ),
            format!("state: {STATE}"),
            "journal: journal.db".into(),
            format!(
                "socket: {}",
                control::path().map_or_else(|e| e.to_string(), |p| p.display().to_string())
            ),
        ]
        .join("\n")
    }

    // A prompt from the terminal: the same path as a Discord message in the thread.
    async fn cmd_send(&self, s: &Send) -> Result<String> {
        let cell = self.task_in(id(&s.thread)?)?;
        let outcome = {
            let mut t = cell.lock().await;
            if t.phase == Phase::Gone {
                bail!("that agent has exited");
            }
            let m = self.machine(&t.machine)?;
            match herdr::prompt(m, &t.pane, &s.text).await {
                Ok(()) => {
                    if t.phase == Phase::Idle {
                        t.phase = Phase::Waiting;
                    }
                    "sent"
                }
                Err(e) if e.is_busy() => {
                    if t.queued.is_some() {
                        bail!("one prompt is already queued for this agent");
                    }
                    t.queued = Some(s.text.clone());
                    "queued, sent when the agent is idle"
                }
                Err(e) => return Err(e.into()),
            }
        };
        self.save().await;
        Ok(outcome.into())
    }

    async fn cmd_mate(&self, http: &Http, name: &str) -> Result<String> {
        let name = mate_name(name)?;
        let mut live = 0;
        for (_, cell) in self.cells() {
            let t = cell.lock().await;
            if t.phase == Phase::Gone {
                continue;
            }
            match t.mate.as_deref() {
                Some(n) if n == name => bail!("mate {name} is already running"),
                Some(_) => live += 1,
                None => {}
            }
        }
        if live >= MAX_MATES {
            bail!("{MAX_MATES} mates already run, close one first");
        }

        let cwd = self.mates.to_string_lossy().into_owned();
        let (workspace, pane) = herdr::mate_pane(&self.host, &cwd).await?;
        let th = self
            .open_thread(http, &format!("mate · {name} · {}", self.host.name))
            .await?;
        let head = th.say(http, format!("mate `{name}` in `{cwd}`")).await?;
        self.insert(
            th,
            Task {
                machine: self.host.name.clone(),
                pane: pane.clone(),
                workspace,
                kind: harness::DEFAULT.into(),
                repo: "mates".into(),
                branch: String::new(),
                origin: Origin::Attached,
                owner: Owner::Mate,
                mate: Some(name.clone()),
                phase: Phase::Starting {
                    prompt: MATE_ROLE.into(),
                },
                status: None,
                session: None,
                last_reply: None,
                prompt_msg: Some(head.id),
                dialog_msg: None,
                queued: None,
            },
        );
        self.save().await;
        log::append(
            &self.mates.join("state").join(&name),
            log::Event::Started,
            &pane.0,
        )?;
        self.write_owners(&name).await?;
        herdr::start(
            &self.host,
            &format!("mate-{name}"),
            harness::DEFAULT,
            &pane,
            &cwd,
            None,
        )
        .await?;
        Ok(format!("<#{th}>"))
    }

    // The panes linked to a mate and who may drive each one, for the mate to read before it sends input.
    async fn write_owners(&self, name: &str) -> Result<()> {
        let mut lines = Vec::new();
        for (_, cell) in self.cells() {
            let t = cell.lock().await;
            if t.mate.as_deref() == Some(name) {
                lines.push(format!("{} {}", t.pane.0, t.owner.as_str()));
            }
        }
        let dir = self.mates.join("state").join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("owners"), lines.join("\n") + "\n")?;
        Ok(())
    }

    async fn set_owner(&self, thread: &str, owner: Owner) -> Result<String> {
        let cell = self.task_in(id(thread)?)?;
        let (text, mate) = {
            let mut t = cell.lock().await;
            t.owner = owner;
            (
                format!("{} is {}", t.pane.0, owner.as_str()),
                t.mate.clone(),
            )
        };
        if let Some(name) = mate {
            log::append(
                &self.mates.join("state").join(&name),
                log::Event::Owner,
                owner.as_str(),
            )?;
            self.write_owners(&name).await?;
        }
        self.save().await;
        Ok(text)
    }

    async fn cmd_worktrees(&self) -> Result<String> {
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
        Ok(parts.join("\n"))
    }

    async fn cmd_done(&self, http: &Http, d: &Done) -> Result<String> {
        let th = id::<ChannelId>(&d.thread)?;
        let cell = self.task_in(th)?;
        let teardown = if d.remove {
            Teardown::Remove
        } else {
            Teardown::Close
        };
        {
            let mut t = cell.lock().await;
            if teardown == Teardown::Remove && t.origin == Origin::Attached {
                bail!("drover didn't open this worktree, remove it by hand");
            }
            if d.journal {
                herdr::prompt(self.machine(&t.machine)?, &t.pane, journal::ENTRY_PROMPT).await?;
                t.phase = Phase::Ending(teardown);
            }
        }
        if !d.journal {
            let t = cell.lock().await.clone();
            self.remove(th);
            self.save().await;
            self.teardown(http, th, &t, teardown).await?;
            return Ok("closed without a journal entry".into());
        }
        let msg = th.say(http, "writing the journal entry").await?;
        cell.lock().await.prompt_msg = Some(msg.id);
        self.save().await;
        th.create_reaction(http, msg.id, emoji("👀")).await?;
        Ok("asked the agent for the journal entry".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mate_names_are_short_and_lowercase() {
        assert!(mate_name("reviewer-2").is_ok());
        assert!(mate_name("Reviewer").is_err());
        assert!(mate_name("2fast").is_err());
        assert!(mate_name(&"a".repeat(27)).is_err());
    }
}
