// Terminal front end: argv becomes one Action, sent to the running service.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::action::{
    Action, Attach, Candidates, Done, Keys, Mate, New, Note, Recall, Scout, Send, Target,
};

pub enum Invocation {
    Serve,
    Help,
    Usage(String),
    Run(Action),
}

// Flags that take no value.
const SWITCHES: [&str; 3] = ["scout", "remove", "no-journal"];

pub const HELP: &str = "\
drover: the same herdr desk from the terminal, Discord, or an agent.

Usage: drover <command> [--flag value ...]

Commands (Discord command in brackets):
  new        --machine M --repo R --branch B --prompt P [--agent NAME] [--model X] [--scout] [--mate NAME]
             [/new] worktree and agent in a new Discord thread. --branch is a new name,
             an existing branch, or #pr. --scout asks for a report and no changes.
  attach     --machine M --pane P
             [/attach] follow an agent already running in herdr. P is from `agents`.
  link       --machine M --pane P --mate NAME
             attach a running agent to a mate: the mate sees it and the desk keeps the pane
  escalate   --mate NAME --text \"...\"
             post a question for the desk in the mate's thread, and keep it in its log
  brief      --mate NAME --text \"...\"
             add a note to a mate's log for it to read
  agents     [/agents] agents on every machine, with the thread each one is bound to
  worktrees  [/worktrees] worktrees on every machine
  usage      [/usage] usage of each agent on every machine
  screen     --thread T
             [/screen] the terminal of a task's agent
  keys       --thread T --keys \"esc shift+tab enter\"
             [/keys] send keys to a task's agent
  scout      PROMPT...
             [/scout] a short look with no repo, in its own pane and folder
  scout      close --thread T
             [/scout-close] end the scout and delete its folder
  mate       NAME
             [/mate] start a mate: a dispatcher pane in the drover workspace, at most three
  send       --thread T --text \"...\"
             send a prompt to a task's agent; queued if the agent is busy
  take       --thread T
             [/take] give the pane back to you
  release    --thread T
             [/release] let the mate drive the pane
  done       --thread T [--remove] [--no-journal]
             [/done] ask for a journal entry, then close the workspace.
             --remove also deletes the checkout, only if drover opened it and it is clean and merged.
             --no-journal closes without an entry.
  recall     WORDS...
             [/recall] search the journal
  config     show this service's host, remotes, channel and paths
  help       this text

A task is its Discord thread. Thread ids come from `agents` (<#id>).
Inside a task thread, a plain Discord message is the next prompt; it is queued while the agent works.
Reactions on the prompt: 👀 working, ⏸️ the agent asks a dialog, ✅ done.

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

// The block of HELP for one command: its lines up to the next blank line.
pub fn command_help(cmd: &str) -> Option<String> {
    let mut lines = Vec::new();
    let mut on = false;
    for line in HELP.lines() {
        if line.is_empty() {
            on = false;
        } else if let Some(entry) = line.strip_prefix("  ").filter(|e| !e.starts_with(' ')) {
            on = entry.split_whitespace().next() == Some(cmd);
        }
        if on {
            lines.push(line);
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n") + "\n")
}

pub fn parse(args: &[String]) -> Result<Invocation> {
    let Some((cmd, rest)) = args.split_first() else {
        return Ok(Invocation::Help);
    };
    match cmd.as_str() {
        "serve" => return Ok(Invocation::Serve),
        "help" | "-h" | "--help" => {
            return Ok(match rest.first().and_then(|c| command_help(c)) {
                Some(usage) => Invocation::Usage(usage),
                None => Invocation::Help,
            });
        }
        _ => {}
    }
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(match command_help(cmd) {
            Some(usage) => Invocation::Usage(usage),
            None => Invocation::Help,
        });
    }
    let action = action(cmd, rest).map_err(|e| match command_help(cmd) {
        Some(_) => err!("drover {cmd}: {e}\nsee `drover {cmd} --help`"),
        None => err!("drover {cmd}: {e}\nsee `drover help`"),
    })?;
    Ok(Invocation::Run(action))
}

fn action(cmd: &str, rest: &[String]) -> Result<Action> {
    let flags = Flags::read(rest)?;
    Ok(match cmd {
        "new" => Action::New(New {
            machine: flags.req("machine")?,
            repo: flags.req("repo")?,
            branch: flags.req("branch")?,
            prompt: flags.req("prompt")?,
            agent: flags.opt("agent"),
            model: flags.opt("model"),
            scout: flags.on("scout"),
            mate: flags.opt("mate"),
        }),
        "attach" => Action::Attach(Attach {
            machine: flags.req("machine")?,
            pane: flags.req("pane")?,
            mate: None,
        }),
        "link" => Action::Attach(Attach {
            machine: flags.req("machine")?,
            pane: flags.req("pane")?,
            mate: Some(flags.req("mate")?),
        }),
        "escalate" => Action::Escalate(Note {
            mate: flags.req("mate")?,
            text: flags.req("text")?,
        }),
        "brief" => Action::Brief(Note {
            mate: flags.req("mate")?,
            text: flags.req("text")?,
        }),
        "agents" => Action::Agents,
        "worktrees" => Action::Worktrees,
        "usage" => Action::Usage,
        "config" => Action::Config,
        "complete" => Action::Complete(match flags.words.first().map(String::as_str) {
            Some("threads") => Candidates::Threads,
            Some("machines") => Candidates::Machines,
            Some("mates") => Candidates::Mates,
            Some("panes") => Candidates::Panes(
                flags
                    .words
                    .get(1)
                    .cloned()
                    .context("drover complete panes MACHINE")?,
            ),
            _ => bail!("drover complete threads|machines|mates|panes [MACHINE]"),
        }),
        "screen" => Action::Screen(Target {
            thread: flags.req("thread")?,
        }),
        "keys" => Action::Keys(Keys {
            thread: flags.req("thread")?,
            keys: flags
                .req("keys")?
                .split_whitespace()
                .map(String::from)
                .collect(),
        }),
        "send" => Action::Send(Send {
            thread: flags.req("thread")?,
            text: flags.req("text")?,
        }),
        "scout" => match flags.words.split_first() {
            Some((first, _)) if first == "close" => Action::ScoutClose(Target {
                thread: flags.req("thread")?,
            }),
            _ if flags.words.is_empty() => {
                bail!("drover scout PROMPT, or drover scout close --thread T")
            }
            _ => Action::Scout(Scout {
                prompt: flags.words.join(" "),
            }),
        },
        "mate" => Action::Mate(Mate {
            name: flags.words.first().cloned().context("drover mate NAME")?,
        }),
        "take" => Action::Take(Target {
            thread: flags.req("thread")?,
        }),
        "release" => Action::Release(Target {
            thread: flags.req("thread")?,
        }),
        "done" => Action::Done(Done {
            thread: flags.req("thread")?,
            remove: flags.on("remove"),
            journal: !flags.on("no-journal"),
        }),
        "recall" => {
            if flags.words.is_empty() {
                bail!("recall needs words to search for");
            }
            Action::Recall(Recall {
                query: flags.words.join(" "),
            })
        }
        other => bail!("unknown command {other}"),
    })
}

// `--name value` pairs, `--switch` flags, and bare words.
struct Flags {
    values: HashMap<String, String>,
    switches: HashSet<String>,
    words: Vec<String>,
}

impl Flags {
    fn read(args: &[String]) -> Result<Self> {
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
            if SWITCHES.contains(&name) {
                flags.switches.insert(name.into());
                continue;
            }
            let Some(value) = it.next() else {
                bail!("--{name} needs a value");
            };
            flags.values.insert(name.into(), value.clone());
        }
        Ok(flags)
    }

    fn req(&self, name: &str) -> Result<String> {
        self.opt(name).context(format!("--{name} is required"))
    }

    fn opt(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    fn on(&self, name: &str) -> bool {
        self.switches.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn new_takes_flags_and_switches() -> Result<()> {
        let Invocation::Run(Action::New(n)) = parse(&args(
            "new --machine host --repo drover --branch feature/x --prompt fix --scout",
        ))?
        else {
            bail!("not a new");
        };
        assert!(n.scout && n.agent.is_none() && n.branch == "feature/x");
        Ok(())
    }

    #[test]
    fn done_defaults_to_journal_and_no_remove() -> Result<()> {
        let Invocation::Run(Action::Done(d)) = parse(&args("done --thread 42"))? else {
            bail!("not a done");
        };
        assert!(d.journal && !d.remove && d.thread == "42");
        Ok(())
    }

    #[test]
    fn take_and_release_name_a_thread() -> Result<()> {
        let Invocation::Run(Action::Release(t)) = parse(&args("release --thread 42"))? else {
            bail!("not a release");
        };
        assert_eq!(t.thread, "42");
        assert!(parse(&args("take")).is_err());
        Ok(())
    }

    #[test]
    fn scout_takes_a_prompt_or_close() -> Result<()> {
        let Invocation::Run(Action::Scout(s)) = parse(&args("scout look at the cache"))? else {
            bail!("not a scout");
        };
        assert_eq!(s.prompt, "look at the cache");
        let Invocation::Run(Action::ScoutClose(t)) = parse(&args("scout close --thread 9"))? else {
            bail!("not a scout close");
        };
        assert_eq!(t.thread, "9");
        assert!(parse(&args("scout")).is_err());
        Ok(())
    }

    #[test]
    fn link_sets_the_mate_and_brief_needs_text() -> Result<()> {
        let Invocation::Run(Action::Attach(a)) =
            parse(&args("link --machine host --pane wAX:p1 --mate reviewer"))?
        else {
            bail!("not an attach");
        };
        assert_eq!(a.mate.as_deref(), Some("reviewer"));
        assert!(parse(&args("brief --mate reviewer")).is_err());
        Ok(())
    }

    #[test]
    fn recall_joins_words() -> Result<()> {
        let Invocation::Run(Action::Recall(r)) = parse(&args("recall fts5 index"))? else {
            bail!("not a recall");
        };
        assert_eq!(r.query, "fts5 index");
        assert!(parse(&args("recall")).is_err());
        Ok(())
    }

    #[test]
    fn help_flag_wins_over_missing_values() {
        assert!(matches!(
            parse(&args("new --machine meerkat --help")),
            Ok(Invocation::Usage(_))
        ));
        assert!(matches!(parse(&args("new -h")), Ok(Invocation::Usage(_))));
        assert!(matches!(
            parse(&args("frobnicate --help")),
            Ok(Invocation::Help)
        ));
    }

    #[test]
    fn command_help_is_its_own_block() {
        let new = command_help("new").unwrap_or_default();
        assert!(new.starts_with("  new ") && new.contains("[/new]"));
        assert!(!new.contains("attach"));
        let scout = command_help("scout").unwrap_or_default();
        assert!(scout.contains("close --thread T") && !scout.contains("A task is"));
        assert!(command_help("frobnicate").is_none());
    }

    #[test]
    fn complete_lists_one_kind_of_name() -> Result<()> {
        assert!(matches!(
            parse(&args("complete threads"))?,
            Invocation::Run(Action::Complete(Candidates::Threads))
        ));
        let Invocation::Run(Action::Complete(Candidates::Panes(m))) =
            parse(&args("complete panes meerkat"))?
        else {
            bail!("not panes");
        };
        assert_eq!(m, "meerkat");
        assert!(parse(&args("complete panes")).is_err());
        Ok(())
    }

    #[test]
    fn errors_name_the_command() {
        let err = parse(&args("new --machine host"))
            .err()
            .map(|e| e.to_string());
        assert!(err.is_some_and(|e| e.starts_with("drover new: --repo is required")));
    }

    #[test]
    fn missing_and_unknown_are_errors() {
        assert!(parse(&args("new --machine host")).is_err());
        assert!(parse(&args("new --machine")).is_err());
        assert!(parse(&args("frobnicate")).is_err());
        assert!(matches!(parse(&[]), Ok(Invocation::Help)));
        assert!(matches!(parse(&args("help")), Ok(Invocation::Help)));
    }
}
