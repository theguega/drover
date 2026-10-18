// Terminal front end: argv becomes one Action, sent to the running service. The reply prints
// as TOON on stdout, errors too, as `error:` and `help:`. Exit 0 ok, 1 error, 2 usage error.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::*;
use crate::action::{
    Action, Attach, Candidates, Done, JournalList, JournalRemove, Keys, Look, Mate, MateName, New,
    Note, Recall, Scout, Send, Target,
};

pub enum Invocation {
    Home,
    Version,
    Help,
    Usage(&'static Spec),
    Run(&'static Spec, Action),
}

// An error as the terminal prints it: what went wrong and the command that fixes it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Failure {
    pub error: String,
    pub help: Option<String>,
}

impl Failure {
    pub fn new(error: impl Into<String>, help: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            help: Some(help.into()),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for Failure {}

// A Failure raised in the service keeps its help; any other error has none yet.
impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        match e.downcast::<Self>() {
            Ok(f) => *f,
            Err(e) => Self {
                error: e.to_string(),
                help: None,
            },
        }
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Error::from(e).into()
    }
}

// One command: its words after `drover`, what it does, its arguments, and two examples.
pub struct Spec {
    name: &'static str,
    about: &'static str,
    args: &'static [Arg],
    examples: [&'static str; 2],
}

// How the argument is written, its default ("required" when it has none), and what it is.
// `--name <value>` takes a value, `--name` is a switch, and `<name>` is a bare word.
struct Arg(&'static str, &'static str, &'static str);

const PANE: Arg = Arg(
    "--pane <pane>",
    "required unless --thread",
    "a pane from drover agents",
);
const THREAD: Arg = Arg(
    "--thread <thread>",
    "none",
    "a Discord thread id instead of --pane",
);
const FULL: Arg = Arg(
    "--full",
    "off",
    "the whole text, not the first 1000 characters",
);
const DISCORD: Arg = Arg("--discord", "off", "also open a Discord thread");

const SPECS: &[Spec] = &[
    Spec {
        name: "new",
        about: "Open a worktree and start an agent in it (Discord /new)",
        args: &[
            Arg(
                "--machine <machine>",
                "required",
                "a machine from drover config",
            ),
            Arg("--repo <repo>", "required", "a repo under the repos root"),
            Arg(
                "--branch <branch>",
                "required",
                "a new name or an existing branch or #<pr>",
            ),
            Arg("--prompt <prompt>", "required", "the first prompt"),
            Arg("--agent <agent>", harness::DEFAULT, "claude or cursor"),
            Arg(
                "--model <model>",
                "the agent's own",
                "the model the agent runs",
            ),
            Arg("--scout", "off", "ask for a report and no changes"),
            Arg(
                "--mate <mate>",
                "none",
                "the mate that dispatches and owns the pane",
            ),
            DISCORD,
        ],
        examples: [
            "drover new --machine meerkat --repo drover --branch fix/flaky-test --prompt 'fix the flaky journal test'",
            "drover new --machine meerkat --repo drover --branch '#42' --prompt 'review this PR' --scout",
        ],
    },
    Spec {
        name: "attach",
        about: "Follow an agent already running in herdr (Discord /attach)",
        args: &[
            Arg(
                "--machine <machine>",
                "the task's or the host",
                "the machine the pane is on",
            ),
            PANE,
            DISCORD,
        ],
        examples: [
            "drover attach --pane wAX:p1",
            "drover attach --machine meerkat --pane wAX:p1 --discord",
        ],
    },
    Spec {
        name: "link",
        about: "Link a running agent to a mate: the mate sees it and the desk keeps the pane",
        args: &[
            Arg(
                "--machine <machine>",
                "required",
                "the machine the pane is on",
            ),
            Arg("--pane <pane>", "required", "a pane from drover agents"),
            Arg("--mate <mate>", "required", "a running mate"),
            DISCORD,
        ],
        examples: [
            "drover link --machine meerkat --pane wAX:p1 --mate docs",
            "drover link --machine meerkat --pane wAX:p1 --mate docs --discord",
        ],
    },
    Spec {
        name: "escalate",
        about: "Log a question for the desk and post it in the mate's thread if it has one",
        args: &[
            Arg("--mate <mate>", "required", "the mate asking"),
            Arg("--text <text>", "required", "the question"),
        ],
        examples: [
            "drover escalate --mate docs --text 'which section first?'",
            "drover escalate --mate docs --text 'node api is stuck after three rounds'",
        ],
    },
    Spec {
        name: "brief",
        about: "Add a note to a mate's log for it to read",
        args: &[
            Arg("--mate <mate>", "required", "the mate to tell"),
            Arg("--text <text>", "required", "the note"),
        ],
        examples: [
            "drover brief --mate docs --text 'the README is frozen until Friday'",
            "drover brief --mate docs --text 'use the staging host'",
        ],
    },
    Spec {
        name: "agents",
        about: "List the agents on every machine (Discord /agents)",
        args: &[],
        examples: ["drover agents", "DROVER_HOST=meerkat drover agents"],
    },
    Spec {
        name: "worktrees",
        about: "List the worktrees on every machine (Discord /worktrees)",
        args: &[],
        examples: ["drover worktrees", "DROVER_HOST=meerkat drover worktrees"],
    },
    Spec {
        name: "usage",
        about: "Show each agent's usage on every machine (Discord /usage)",
        args: &[],
        examples: ["drover usage", "DROVER_HOST=meerkat drover usage"],
    },
    Spec {
        name: "snapshot",
        about: "Print the desk as JSON: machines and tasks and mates and escalations",
        args: &[],
        examples: ["drover snapshot", "drover snapshot | jq '.tasks[] | .pane'"],
    },
    Spec {
        name: "watch",
        about: "Print the snapshot as one JSON line and then one line per change until stopped",
        args: &[],
        examples: ["drover watch", "drover watch | jq -c '.machines'"],
    },
    Spec {
        name: "screen",
        about: "Show the terminal of a task's agent (Discord /screen)",
        args: &[PANE, THREAD, FULL],
        examples: [
            "drover screen --pane wAX:p1",
            "drover screen --pane wAX:p1 --full",
        ],
    },
    Spec {
        name: "keys",
        about: "Send keys to a task's agent and show its terminal (Discord /keys)",
        args: &[
            PANE,
            THREAD,
            Arg("--keys <keys>", "required", "keys separated by spaces"),
        ],
        examples: [
            "drover keys --pane wAX:p1 --keys enter",
            "drover keys --pane wAX:p1 --keys 'esc shift+tab enter'",
        ],
    },
    Spec {
        name: "scout",
        about: "Start a scout: a short look with no repo in its own pane and folder (Discord /scout)",
        args: &[Arg("<prompt>", "required", "what to look at"), DISCORD],
        examples: [
            "drover scout how does herdr report a blocked agent",
            "drover scout compare the two usage endpoints --discord",
        ],
    },
    Spec {
        name: "scout close",
        about: "End a scout and delete its folder (Discord /scout-close)",
        args: &[PANE, THREAD],
        examples: [
            "drover scout close --pane wAX:p3",
            "drover scout close --pane scout-1a2b3c",
        ],
    },
    Spec {
        name: "mate",
        about: "Start a mate: a dispatcher pane in the drover workspace, at most three (Discord /mate)",
        args: &[
            Arg(
                "<name>",
                "required",
                "lowercase letters and digits and - or _",
            ),
            DISCORD,
        ],
        examples: ["drover mate docs", "drover mate docs --discord"],
    },
    Spec {
        name: "send",
        about: "Send a prompt to a task's agent: queued while it works",
        args: &[PANE, THREAD, Arg("--text <text>", "required", "the prompt")],
        examples: [
            "drover send --pane wAX:p1 --text 'run the tests again'",
            "drover send --thread 1300000000000000000 --text 'ship it'",
        ],
    },
    Spec {
        name: "take",
        about: "Give a mate's pane back to you (Discord /take)",
        args: &[PANE, THREAD],
        examples: [
            "drover take --pane wAX:p1",
            "drover take --thread 1300000000000000000",
        ],
    },
    Spec {
        name: "release",
        about: "Let the mate drive the pane (Discord /release)",
        args: &[PANE, THREAD],
        examples: [
            "drover release --pane wAX:p1",
            "drover release --thread 1300000000000000000",
        ],
    },
    Spec {
        name: "done",
        about: "Ask for a journal entry and then close the task's workspace (Discord /done)",
        args: &[
            PANE,
            THREAD,
            Arg(
                "--remove",
                "off",
                "also delete the checkout if drover opened it and it is clean and merged",
            ),
            Arg("--no-journal", "off", "close without an entry"),
        ],
        examples: [
            "drover done --pane wAX:p1",
            "drover done --pane wAX:p1 --remove",
        ],
    },
    Spec {
        name: "recall",
        about: "Search the journal (Discord /recall)",
        args: &[Arg("<words>", "required", "what to search for"), FULL],
        examples: [
            "drover recall fts5 index",
            "drover recall owners deadlock --full",
        ],
    },
    Spec {
        name: "journal",
        about: "Show the five newest journal entries: of the task's repo when given (Discord /journal)",
        args: &[
            Arg("--pane <pane>", "every repo", "a task whose repo to show"),
            THREAD,
            FULL,
        ],
        examples: ["drover journal", "drover journal --pane wAX:p1 --full"],
    },
    Spec {
        name: "journal rm",
        about: "Delete one journal entry (Discord /journal-rm)",
        args: &[Arg("<id>", "required", "an entry id from drover journal")],
        examples: ["drover journal rm 12", "drover journal rm 7"],
    },
    Spec {
        name: "config",
        about: "Show this service's host and remotes and channel and paths",
        args: &[],
        examples: ["drover config", "DROVER_HOST=meerkat drover config"],
    },
    // for the shell completion scripts, so not in help
    Spec {
        name: "complete",
        about: "Names for shell completion, one per line",
        args: &[Arg(
            "<kind>",
            "required",
            "threads or machines or mates or panes <machine> or journal",
        )],
        examples: ["drover complete machines", "drover complete panes meerkat"],
    },
];

impl Spec {
    // The flag names it takes, each with whether it takes a value.
    fn flags(&self) -> impl Iterator<Item = (&'static str, bool)> {
        self.args.iter().filter_map(|Arg(syntax, ..)| {
            let flag = syntax.strip_prefix("--")?;
            Some(match flag.split_once(' ') {
                Some((name, _)) => (name, true),
                None => (flag, false),
            })
        })
    }

    // Every argument, the optional ones in brackets.
    fn usage(&self) -> String {
        self.command(true)
    }

    // The shortest complete command: the required arguments only.
    fn template(&self) -> String {
        self.command(false)
    }

    fn command(&self, optional: bool) -> String {
        let mut out = format!("drover {}", self.name);
        for Arg(syntax, default, _) in self.args {
            if default.starts_with("required") {
                out.push_str(&format!(" {syntax}"));
            } else if optional {
                out.push_str(&format!(" [{syntax}]"));
            }
        }
        out
    }

    fn help(&self) -> Value {
        let flags: Vec<Value> = self
            .args
            .iter()
            .map(|Arg(flag, default, about)| json!({ "flag": flag, "default": default, "about": about }))
            .collect();
        json!({
            "command": format!("drover {}", self.name),
            "description": self.about,
            "usage": self.usage(),
            "flags": flags,
            "examples": self.examples,
        })
    }

    fn usage_error(&self, error: impl Into<String>) -> Failure {
        Failure::new(error, self.template())
    }
}

fn spec(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.name == name)
}

const DESCRIPTION: &str = "Runs coding agents in herdr worktrees and drives them from the terminal or Discord or another agent";

const HELP_HEAD: &str = "\
drover: the same herdr desk from the terminal, Discord, or an agent.

Usage: drover <command> [--flag value ...]
       drover <command> --help    one command's flags, defaults and examples
       drover                     the desk now: counts, what needs you, next steps

Commands:
";

const HELP_TAIL: &str = "
Terminal commands stay in the terminal by default and print the pane id.
Pass --discord (or run `drover attach --pane <pane> --discord` later) when you want a Discord thread.
Commands started from Discord (/new, /attach, /scout, /mate) always open a Discord thread.
Inside a task thread, a plain Discord message is the next prompt; it is queued while the agent works.
Reactions on the prompt: 👀 working, ⏸️ the agent asks a dialog, ✅ done.

Output is TOON on stdout, errors too (`error:` and `help:`). snapshot and watch print JSON.
Exit codes: 0 ok, 1 error, 2 usage error.

Setup:
  The service runs `drover serve` from this checkout, as a user service.
  It reads .env: DISCORD_TOKEN, DISCORD_GUILD_ID, DISCORD_CHANNEL_ID, ALLOWED_USER_IDS,
  HOST_NAME, REMOTES (comma-separated ssh names), REPOS_ROOT (repositories and worktrees,
  default ~/.drover-workspace). Run `drover config` for the live values.
  Needs on PATH: herdr, drover-wt, git, jq, ssh. Remotes need herdr, python3, and ssh.
  The terminal commands talk to the service over a unix socket under ~/.local/state/drover.
  Install or update: curl -fsSL https://raw.githubusercontent.com/theguega/drover/main/bootstrap.sh | sh
  Remote machines: the same command with `-s -- --remote`. The README has the Discord setup.
  Logs: journalctl --user -u drover, or drover.log in the install directory on macOS.
";

#[must_use]
pub fn help() -> String {
    let mut out = HELP_HEAD.to_string();
    for s in SPECS.iter().filter(|s| s.name != "complete") {
        out.push_str(&format!(
            "  {:<11}{}\n             {}\n",
            s.name,
            s.usage()
                .trim_start_matches("drover ")
                .trim_start_matches(s.name)
                .trim_start(),
            s.about
        ));
    }
    out + HELP_TAIL
}

pub fn parse(args: &[String]) -> Result<Invocation, Failure> {
    let Some((cmd, rest)) = args.split_first() else {
        return Ok(Invocation::Home);
    };
    match cmd.as_str() {
        "-v" | "-V" | "--version" => return Ok(Invocation::Version),
        "help" | "-h" | "--help" => {
            return Ok(
                match rest
                    .first()
                    .and_then(|c| find(c, rest.get(1..).unwrap_or_default()).0)
                {
                    Some(s) => Invocation::Usage(s),
                    None => Invocation::Help,
                },
            );
        }
        _ => {}
    }
    let (spec, rest) = find(cmd, rest);
    let spec = spec.ok_or_else(|| Failure::new(format!("unknown command {cmd}"), "drover help"))?;
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(Invocation::Usage(spec));
    }
    let action = action(spec, rest).map_err(|e| spec.usage_error(e.to_string()))?;
    Ok(Invocation::Run(spec, action))
}

// The command's spec, with `scout close` and `journal rm` taking their second word.
fn find<'a>(cmd: &str, rest: &'a [String]) -> (Option<&'static Spec>, &'a [String]) {
    if let Some((sub, tail)) = rest.split_first()
        && let Some(s) = spec(&format!("{cmd} {sub}"))
    {
        return (Some(s), tail);
    }
    (spec(cmd), rest)
}

fn action(spec: &Spec, rest: &[String]) -> Result<Action> {
    let flags = Flags::read(spec, rest)?;
    let words = || match flags.words.is_empty() {
        true => Err(err!("missing {}", spec.args.first().map_or("", |a| a.0))),
        false => Ok(flags.words.join(" ")),
    };
    let target = || {
        Ok::<_, Error>(Target {
            thread: flags.target()?,
        })
    };
    Ok(match spec.name {
        "new" => Action::New(New {
            machine: flags.req("machine")?,
            repo: flags.req("repo")?,
            branch: flags.req("branch")?,
            prompt: flags.req("prompt")?,
            agent: flags.opt("agent"),
            model: flags.opt("model"),
            scout: flags.on("scout"),
            mate: flags
                .opt("mate")
                .as_deref()
                .map(MateName::parse)
                .transpose()?,
            discord: flags.on("discord"),
        }),
        "attach" => Action::Attach(Attach {
            machine: flags.opt("machine"),
            pane: flags.req("pane")?,
            mate: None,
            discord: flags.on("discord"),
        }),
        "link" => Action::Attach(Attach {
            machine: Some(flags.req("machine")?),
            pane: flags.req("pane")?,
            mate: Some(MateName::parse(&flags.req("mate")?)?),
            discord: flags.on("discord"),
        }),
        "escalate" => Action::Escalate(Note {
            mate: MateName::parse(&flags.req("mate")?)?,
            text: flags.req("text")?,
        }),
        "brief" => Action::Brief(Note {
            mate: MateName::parse(&flags.req("mate")?)?,
            text: flags.req("text")?,
        }),
        "agents" => Action::Agents,
        "worktrees" => Action::Worktrees,
        "usage" => Action::Usage,
        "config" => Action::Config,
        "snapshot" => Action::Snapshot,
        "watch" => Action::Watch,
        "complete" => Action::Complete(match flags.words.first().map(String::as_str) {
            Some("threads") => Candidates::Threads,
            Some("machines") => Candidates::Machines,
            Some("mates") => Candidates::Mates,
            Some("panes") => Candidates::Panes(
                flags
                    .words
                    .get(1)
                    .cloned()
                    .context("missing <machine> after panes")?,
            ),
            Some("journal") => Candidates::Journal,
            _ => bail!("missing <kind>"),
        }),
        "screen" => Action::Screen(Look {
            thread: flags.target()?,
            full: flags.on("full"),
        }),
        "keys" => Action::Keys(Keys {
            thread: flags.target()?,
            keys: flags
                .req("keys")?
                .split_whitespace()
                .map(String::from)
                .collect(),
        }),
        "send" => Action::Send(Send {
            thread: flags.target()?,
            text: flags.req("text")?,
        }),
        "scout" => Action::Scout(Scout {
            prompt: words()?,
            discord: flags.on("discord"),
        }),
        "scout close" => Action::ScoutClose(target()?),
        "mate" => Action::Mate(Mate {
            name: MateName::parse(&words()?)?,
            discord: flags.on("discord"),
        }),
        "take" => Action::Take(target()?),
        "release" => Action::Release(target()?),
        "done" => Action::Done(Done {
            thread: flags.target()?,
            remove: flags.on("remove"),
            journal: !flags.on("no-journal"),
        }),
        "recall" => Action::Recall(Recall {
            query: words()?,
            full: flags.on("full"),
        }),
        "journal" => {
            if !flags.words.is_empty() {
                bail!("unexpected {}", flags.words.join(" "));
            }
            Action::JournalList(JournalList {
                task: flags.opt("pane").or_else(|| flags.opt("thread")),
                full: flags.on("full"),
            })
        }
        "journal rm" => Action::JournalRemove(JournalRemove {
            id: words()?
                .parse::<i64>()
                .map_err(|_| err!("<id> is a number from drover journal"))?,
        }),
        other => bail!("unknown command {other}"),
    })
}

// `--name value` pairs, `--switch` flags, and bare words, checked against the command's spec.
struct Flags {
    values: HashMap<String, String>,
    switches: HashSet<String>,
    words: Vec<String>,
}

impl Flags {
    // A flag the command doesn't take fails here, before anything else is checked.
    fn read(spec: &Spec, args: &[String]) -> Result<Self> {
        let mut flags = Self {
            values: HashMap::new(),
            switches: HashSet::new(),
            words: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let Some(name) = arg.strip_prefix("--") else {
                flags.words.push(arg.clone());
                continue;
            };
            let Some((_, takes_value)) = spec.flags().find(|(n, _)| *n == name) else {
                let valid: Vec<String> = spec.flags().map(|(n, _)| format!("--{n}")).collect();
                bail!(
                    "unknown flag --{name}; drover {} takes {}",
                    spec.name,
                    match valid.is_empty() {
                        true => "no flags".into(),
                        false => valid.join(" "),
                    }
                );
            };
            if !takes_value {
                flags.switches.insert(name.into());
                continue;
            }
            let Some(value) = it.next() else {
                bail!("missing value for --{name}");
            };
            flags.values.insert(name.into(), value.clone());
        }
        Ok(flags)
    }

    fn req(&self, name: &str) -> Result<String> {
        self.opt(name).context(format!("missing --{name}"))
    }

    fn target(&self) -> Result<String> {
        self.opt("pane")
            .or_else(|| self.opt("thread"))
            .context("missing --pane")
    }

    fn opt(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    fn on(&self, name: &str) -> bool {
        self.switches.contains(name)
    }
}

// Runs one invocation and prints its reply. The exit code is the result.
pub async fn run(parsed: Result<Invocation, Failure>) -> i32 {
    let (spec, action) = match parsed {
        Err(f) => return fail(&f, 2),
        Ok(Invocation::Version) => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            return 0;
        }
        Ok(Invocation::Help) => {
            print!("{}", help());
            return 0;
        }
        Ok(Invocation::Usage(spec)) => return print(&spec.help()),
        Ok(Invocation::Home) => {
            return match home_view().await {
                Ok(v) => print(&v),
                Err(f) => fail(&f, 1),
            };
        }
        Ok(Invocation::Run(spec, action)) => (spec, action),
    };
    let reply = match action {
        Action::Watch => control::watch_client().await.map(|()| Value::Null),
        // a shell reads these as names, so an error prints nothing
        Action::Complete(_) => {
            return match control::send(&action).await {
                Ok(Value::String(s)) => {
                    println!("{s}");
                    0
                }
                _ => 1,
            };
        }
        action => control::send(&action).await,
    };
    match reply {
        Ok(Value::Null) => 0,
        Ok(Value::String(s)) => {
            println!("{s}");
            0
        }
        Ok(v) => print(&v),
        Err(mut f) => {
            f.help
                .get_or_insert_with(|| format!("drover {} --help", spec.name));
            fail(&f, 1)
        }
    }
}

fn print(v: &Value) -> i32 {
    print!("{}", toon::encode(v));
    0
}

fn fail(f: &Failure, code: i32) -> i32 {
    print!(
        "{}",
        toon::encode(&json!({ "error": f.error, "help": f.help }))
    );
    code
}

async fn home_view() -> Result<Value, Failure> {
    let Value::String(snapshot) = control::send(&Action::Snapshot).await? else {
        return Err(err!("the snapshot is not JSON text").into());
    };
    let snapshot: Snapshot = serde_json::from_str(&snapshot).map_err(Error::from)?;
    let bin =
        std::env::current_exe().map_or_else(|_| "drover".into(), |p| home(&p.to_string_lossy()));
    Ok(desk(&bin, &snapshot))
}

// The parts of `drover snapshot` the home view reads.
#[derive(Deserialize)]
struct Snapshot {
    machines: Vec<SnapMachine>,
    tasks: Vec<SnapTask>,
    mates: Vec<Value>,
    escalations: Vec<SnapEscalation>,
}

#[derive(Deserialize)]
struct SnapMachine {
    reach: String,
}

#[derive(Deserialize)]
struct SnapTask {
    machine: String,
    pane: String,
    repo: String,
    branch: Option<String>,
    role: String,
    mate: Option<String>,
    phase: String,
    status: Option<String>,
}

#[derive(Deserialize)]
struct SnapEscalation {
    at: u64,
    mate: String,
    text: String,
}

// Escalations the home view shows, newest last.
const HOME_ESCALATIONS: usize = 5;
// Characters of an escalation's text it keeps.
const HOME_TEXT: usize = 200;

fn desk(bin: &str, s: &Snapshot) -> Value {
    // every key shows, zero or not, in the order given
    let count = |keys: &[&str], of: &mut dyn Iterator<Item = String>| {
        let mut n: serde_json::Map<String, Value> =
            keys.iter().map(|k| ((*k).into(), 0.into())).collect();
        for k in of {
            let v = n.entry(k).or_insert(0.into());
            *v = (v.as_u64().unwrap_or(0) + 1).into();
        }
        n
    };
    // a gone task is history, not the desk
    let live = || s.tasks.iter().filter(|t| t.phase != "gone");
    let tasks = count(
        &["working", "blocked", "idle", "done", "unknown"],
        &mut live().map(|t| t.status.clone().unwrap_or_else(|| "unknown".into())),
    );
    let machines = count(
        &["reachable", "unreachable", "unknown"],
        &mut s.machines.iter().map(|m| m.reach.clone()),
    );
    let blocked: Vec<Value> = live()
        .filter(|t| t.status.as_deref() == Some("blocked"))
        .map(
            |t| json!({ "pane": t.pane, "machine": t.machine, "repo": t.repo, "branch": t.branch }),
        )
        .collect();
    let start = s.escalations.len().saturating_sub(HOME_ESCALATIONS);
    let escalations: Vec<Value> = s
        .escalations
        .iter()
        .skip(start)
        .map(|e| {
            let pane = live()
                .find(|t| t.role == "mate" && t.mate.as_deref() == Some(e.mate.as_str()))
                .map(|t| t.pane.clone());
            json!({ "mate": e.mate, "pane": pane, "at": e.at, "text": head(&e.text, HOME_TEXT) })
        })
        .collect();

    let mut help = vec![];
    if !blocked.is_empty() {
        help.push("drover screen --pane <pane>");
        help.push("drover keys --pane <pane> --keys <keys>");
    }
    if !escalations.is_empty() {
        help.push("drover send --pane <pane> --text <answer>");
    }
    help.extend([
        "drover agents",
        "drover new --machine <machine> --repo <repo> --branch <branch> --prompt <prompt>",
        "drover <command> --help",
    ]);
    json!({
        "bin": bin,
        "description": DESCRIPTION,
        "desk": { "tasks": tasks, "mates": s.mates.len(), "machines": machines },
        "needs": { "blocked": blocked, "escalations": escalations },
        "help": help,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    fn failure(s: &str) -> Option<Failure> {
        parse(&args(s)).err()
    }

    #[test]
    fn new_takes_flags_and_switches() -> Result<()> {
        let Invocation::Run(_, Action::New(n)) = parse(&args(
            "new --machine host --repo drover --branch feature/x --prompt fix --scout",
        ))?
        else {
            bail!("not a new");
        };
        assert!(n.scout && !n.discord && n.agent.is_none() && n.branch == "feature/x");
        Ok(())
    }

    #[test]
    fn unknown_flags_fail_first_and_list_the_valid_ones() {
        let f = failure("new --machine h --repo r --branch b --prompt p --modle opus");
        assert!(f.is_some_and(|f| f.error
            == "unknown flag --modle; drover new takes --machine --repo --branch --prompt --agent --model --scout --mate --discord"
            && f.help.as_deref()
                == Some("drover new --machine <machine> --repo <repo> --branch <branch> --prompt <prompt>")));
        let f = failure("agents --bogus");
        assert!(f.is_some_and(
            |f| f.error == "unknown flag --bogus; drover agents takes no flags"
                && f.help.as_deref() == Some("drover agents")
        ));
        // unknown wins over missing
        assert!(
            failure("new --bogus").is_some_and(|f| f.error.starts_with("unknown flag --bogus"))
        );
        assert!(failure("screen --pane p1 --remove").is_some());
        assert!(parse(&args("journal --pane p1")).is_ok());
    }

    #[test]
    fn discord_is_opt_in_from_terminal() -> Result<()> {
        let Invocation::Run(_, Action::Attach(a)) = parse(&args("attach --pane wAX:p1 --discord"))?
        else {
            bail!("not an attach");
        };
        assert!(a.discord && a.machine.is_none() && a.pane == "wAX:p1");
        Ok(())
    }

    #[test]
    fn done_defaults_to_journal_and_no_remove() -> Result<()> {
        let Invocation::Run(_, Action::Done(d)) = parse(&args("done --pane wAX:p1"))? else {
            bail!("not a done");
        };
        assert!(d.journal && !d.remove && d.thread == "wAX:p1");
        Ok(())
    }

    #[test]
    fn take_and_release_name_a_pane_or_thread() -> Result<()> {
        let Invocation::Run(_, Action::Release(t)) = parse(&args("release --thread 42"))? else {
            bail!("not a release");
        };
        assert_eq!(t.thread, "42");
        let Invocation::Run(_, Action::Take(t)) = parse(&args("take --pane wAX:p1"))? else {
            bail!("not a take");
        };
        assert_eq!(t.thread, "wAX:p1");
        assert!(failure("take").is_some_and(|f| f.error == "missing --pane"
            && f.help.as_deref() == Some("drover take --pane <pane>")));
        Ok(())
    }

    #[test]
    fn scout_takes_a_prompt_or_close() -> Result<()> {
        let Invocation::Run(_, Action::Scout(s)) = parse(&args("scout look at the cache"))? else {
            bail!("not a scout");
        };
        assert_eq!(s.prompt, "look at the cache");
        let Invocation::Run(_, Action::ScoutClose(t)) = parse(&args("scout close --thread 9"))?
        else {
            bail!("not a scout close");
        };
        assert_eq!(t.thread, "9");
        assert!(
            failure("scout").is_some_and(|f| f.help.as_deref() == Some("drover scout <prompt>"))
        );
        Ok(())
    }

    #[test]
    fn link_sets_the_mate_and_brief_needs_text() -> Result<()> {
        let Invocation::Run(_, Action::Attach(a)) =
            parse(&args("link --machine host --pane wAX:p1 --mate reviewer"))?
        else {
            bail!("not an attach");
        };
        assert_eq!(a.mate.as_ref().map(MateName::as_str), Some("reviewer"));
        assert!(failure("brief --mate reviewer").is_some());
        Ok(())
    }

    #[test]
    fn recall_joins_words() -> Result<()> {
        let Invocation::Run(_, Action::Recall(r)) = parse(&args("recall fts5 index"))? else {
            bail!("not a recall");
        };
        assert_eq!(r.query, "fts5 index");
        assert!(failure("recall").is_some());
        let Invocation::Run(_, Action::JournalRemove(r)) = parse(&args("journal rm 12"))? else {
            bail!("not a journal rm");
        };
        assert_eq!(r.id, 12);
        assert!(failure("journal rm").is_some());
        assert!(failure("journal rm x").is_some());
        Ok(())
    }

    #[test]
    fn help_and_version_win_over_missing_values() {
        assert!(matches!(
            parse(&args("new --machine meerkat --help")),
            Ok(Invocation::Usage(s)) if s.name == "new"
        ));
        assert!(matches!(parse(&args("new -h")), Ok(Invocation::Usage(_))));
        assert!(
            matches!(parse(&args("help journal rm")), Ok(Invocation::Usage(s)) if s.name == "journal rm")
        );
        assert!(matches!(
            parse(&args("help frobnicate")),
            Ok(Invocation::Help)
        ));
        assert!(
            failure("frobnicate --help").is_some_and(|f| f.help.as_deref() == Some("drover help"))
        );
        for v in ["-v", "-V", "--version"] {
            assert!(matches!(parse(&args(v)), Ok(Invocation::Version)));
        }
        assert!(matches!(parse(&[]), Ok(Invocation::Home)));
    }

    #[test]
    fn command_help_is_only_that_command() -> Result<()> {
        let new = toon::encode(&spec("new").context("new")?.help());
        assert!(
            new.contains("command: drover new\n")
                && new.contains("\"--prompt <prompt>\",required,the first prompt\n")
        );
        assert!(new.contains("examples[2]:") && !new.contains("attach"));
        let f = spec("scout close")
            .context("scout close")?
            .flags()
            .collect::<Vec<_>>();
        assert_eq!(f, [("pane", true), ("thread", true)]);
        Ok(())
    }

    #[test]
    fn every_command_parses_its_own_examples() {
        for s in SPECS {
            for ex in s.examples.iter().filter(|e| e.starts_with("drover ")) {
                // single quotes group words the way a shell would
                let mut argv = vec![];
                for (i, part) in ex.trim_start_matches("drover ").split('\'').enumerate() {
                    match i % 2 {
                        1 => argv.push(part.to_string()),
                        _ => argv.extend(part.split_whitespace().map(String::from)),
                    }
                }
                let argv: Vec<String> = argv.into_iter().take_while(|a| a != "|").collect();
                assert!(parse(&argv).is_ok(), "{ex}");
            }
        }
    }

    #[test]
    fn complete_lists_one_kind_of_name() -> Result<()> {
        assert!(matches!(
            parse(&args("complete threads"))?,
            Invocation::Run(_, Action::Complete(Candidates::Threads))
        ));
        let Invocation::Run(_, Action::Complete(Candidates::Panes(m))) =
            parse(&args("complete panes meerkat"))?
        else {
            bail!("not panes");
        };
        assert_eq!(m, "meerkat");
        assert!(failure("complete panes").is_some());
        Ok(())
    }

    #[test]
    fn home_counts_the_desk_and_lists_what_needs_you() -> Result<()> {
        let snapshot: Snapshot = serde_json::from_value(json!({
            "machines": [{"name": "host", "host": true, "reach": "reachable"}, {"name": "m2", "host": false, "reach": "unreachable"}],
            "tasks": [
                {"machine": "host", "pane": "w1:p1", "repo": "drover", "branch": "fix/x", "role": "task", "mate": null, "phase": "waiting", "status": "blocked"},
                {"machine": "host", "pane": "w2:p1", "repo": "mates", "branch": null, "role": "mate", "mate": "docs", "phase": "idle", "status": "idle"},
                {"machine": "host", "pane": "w3:p1", "repo": "drover", "branch": "old", "role": "task", "mate": null, "phase": "gone", "status": "blocked"},
            ],
            "mates": [{"name": "docs", "goal": "", "log": []}],
            "escalations": [{"at": 1_781_600_000, "mate": "docs", "text": "which section?"}],
        }))?;
        assert_eq!(
            toon::encode(&desk("~/.local/bin/drover", &snapshot)),
            "\
bin: ~/.local/bin/drover
description: Runs coding agents in herdr worktrees and drives them from the terminal or Discord or another agent
desk:
  tasks:
    working: 0
    blocked: 1
    idle: 1
    done: 0
    unknown: 0
  mates: 1
  machines:
    reachable: 1
    unreachable: 1
    unknown: 0
needs:
  blocked[1]{pane,machine,repo,branch}:
    \"w1:p1\",host,drover,fix/x
  escalations[1]{mate,pane,at,text}:
    docs,\"w2:p1\",1781600000,which section?
help[6]:
  - drover screen --pane <pane>
  - drover keys --pane <pane> --keys <keys>
  - drover send --pane <pane> --text <answer>
  - drover agents
  - drover new --machine <machine> --repo <repo> --branch <branch> --prompt <prompt>
  - drover <command> --help
"
        );
        Ok(())
    }
}
