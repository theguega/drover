// v1's state.json, once, into runs. Every task becomes a pane run. A crewmate's mate becomes its
// parent, and the mate owns it when it did in v1. The TypeScript drover's flags still load.

use std::{collections::BTreeMap, path::Path};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Result,
    herdr::{PaneId, SessionId},
    run::{HarnessName, Machine, Mode, Owner, Phase, Profile, Role, Run, RunId, Usage},
    store::{Kind, Store},
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
enum V1Phase {
    Starting { prompt: String },
    Restarting { from: Box<V1Phase> },
    Idle,
    Waiting,
    Ending(serde::de::IgnoredAny),
    Gone,
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum V1Owner {
    User,
    Mate,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum V1Role {
    Task,
    Crew { mate: String, owner: V1Owner },
    Mate(String),
    Scout(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct Excerpt {
    text: String,
    chars: usize,
}

#[derive(Deserialize)]
struct Ending {}

// A task as either v1 or the TypeScript drover wrote it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Task {
    machine: String,
    pane: String,
    kind: String,
    repo: String,
    role: Option<V1Role>,
    owner: Option<V1Owner>,
    mate: Option<String>,
    scout: Option<String>,
    phase: Option<V1Phase>,
    session: Option<String>,
    last_reply: Option<String>,
    reply: Option<Excerpt>,
    #[serde(default)]
    expecting: bool,
    pending: Option<String>,
    ending: Option<Ending>,
    #[serde(default)]
    gone: bool,
}

impl Task {
    fn phase(&self) -> V1Phase {
        let phase =
            self.phase
                .clone()
                .unwrap_or_else(|| match (self.gone, &self.ending, &self.pending) {
                    (true, ..) => V1Phase::Gone,
                    (_, Some(_), _) => V1Phase::Waiting,
                    (_, _, Some(p)) => V1Phase::Starting { prompt: p.clone() },
                    _ if self.expecting => V1Phase::Waiting,
                    _ => V1Phase::Idle,
                });
        // a --fresh cut short by a restart is the task as it was
        match phase {
            V1Phase::Restarting { from } => *from,
            p => p,
        }
    }

    // The role, and the mate a crewmate answers to with whether that mate drives it.
    fn role(&self) -> (Role, Option<(&str, V1Owner)>) {
        match (&self.role, &self.scout, &self.mate) {
            (Some(V1Role::Crew { mate, owner }), ..) => (Role::Crew, Some((mate, *owner))),
            (Some(V1Role::Mate(_)), ..) => (Role::Mate, None),
            (Some(V1Role::Task | V1Role::Scout(_)), ..)
            | (None, Some(_), _)
            | (None, None, None) => (Role::Task, None),
            // before roles, the mate's own pane was the one in the mates folder
            (None, None, Some(_)) if self.repo == "mates" => (Role::Mate, None),
            (None, None, Some(mate)) => (
                Role::Crew,
                Some((mate, self.owner.unwrap_or(V1Owner::User))),
            ),
        }
    }

    fn mate_name(&self) -> Option<&str> {
        match (&self.role, &self.mate) {
            (Some(V1Role::Mate(name)), _) => Some(name),
            (None, Some(name)) if self.repo == "mates" => Some(name),
            _ => None,
        }
    }
}

// A run and the events it starts with.
pub type Migrated = (Run, Vec<(Kind, Value)>);

pub fn runs(state: &str, at: i64) -> Result<Vec<Migrated>> {
    let raw: BTreeMap<String, Task> = serde_json::from_str(state)?;
    // one task per pane: a live one wins over a gone one, as v1 loaded them
    let mut tasks: BTreeMap<(String, String), Task> = BTreeMap::new();
    for t in raw.into_values() {
        let key = (t.machine.clone(), t.pane.clone());
        let gone = matches!(t.phase(), V1Phase::Gone);
        if gone
            && tasks
                .get(&key)
                .is_some_and(|prev| !matches!(prev.phase(), V1Phase::Gone))
        {
            continue;
        }
        tasks.insert(key, t);
    }
    // ids are 24 bits: a repeat is drawn again, so no run replaces another
    let mut ids = std::collections::HashSet::new();
    let tasks: Vec<(RunId, Task)> = tasks
        .into_values()
        .map(|t| {
            loop {
                let id = RunId::random();
                if ids.insert(id.clone()) {
                    break (id, t);
                }
            }
        })
        .collect();

    // a mate's name to its run: the live one if any, else the last one
    let mut mates: BTreeMap<&str, (&RunId, bool)> = BTreeMap::new();
    for (id, t) in &tasks {
        if let Some(name) = t.mate_name() {
            let live = !matches!(t.phase(), V1Phase::Gone);
            if !mates.get(name).is_some_and(|(_, l)| *l && !live) {
                mates.insert(name, (id, live));
            }
        }
    }

    let mut out = Vec::new();
    for (id, t) in &tasks {
        let (role, crew) = t.role();
        let parent = crew
            .and_then(|(mate, _)| mates.get(mate))
            .map(|(p, _)| (*p).clone());
        let owner = match (crew, &parent) {
            (Some((_, V1Owner::Mate)), Some(p)) => Owner::Run(p.clone()),
            _ => Owner::User,
        };
        let mut events = Vec::new();
        let phase = match t.phase() {
            V1Phase::Starting { prompt } => {
                events.push((Kind::Requested, json!({ "prompt": prompt })));
                Phase::Starting
            }
            V1Phase::Idle => Phase::Idle,
            // v2 has no ending turn: the entry v1 waited for is given to `done`
            V1Phase::Waiting | V1Phase::Ending(_) => Phase::Working,
            V1Phase::Gone | V1Phase::Restarting { .. } => Phase::Done,
        };
        if let (Some(r), Some(last)) = (&t.reply, &t.last_reply) {
            events.push((
                Kind::Reply,
                json!({ "id": last, "text": r.text, "chars": r.chars }),
            ));
        }
        let run = Run {
            id: id.clone(),
            machine: t.machine.parse::<Machine>()?,
            // v1 kept the repo, not the folder; the watcher fills in a live run's folder
            cwd: t.repo.clone().into(),
            harness: HarnessName::parse(&t.kind)
                .ok_or_else(|| err!("v1 task {}: unknown agent {}", t.pane, t.kind))?,
            mode: Mode::Pane {
                pane: Some(PaneId(t.pane.clone())),
            },
            role,
            parent,
            profile: Profile::Actor,
            owner,
            session: t.session.as_deref().and_then(SessionId::parse),
            phase,
            usage: Usage::default(),
            created_at: at,
            ended_at: phase.ended().then_some(at),
        };
        out.push((run, events));
    }
    Ok(out)
}

// Writes the runs of a v1 state file into a store with no runs, in one transaction, so a crash
// leaves none and the next start tries again. Returns how many, or None when the store has runs.
pub fn migrate(store: &Store, path: &Path, at: i64) -> Result<Option<usize>> {
    if !store.list()?.is_empty() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|e| err!("{}: {e}", path.display()))?;
    let runs = runs(&text, at)?;
    store.insert_all(&runs)?;
    Ok(Some(runs.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count<T: PartialEq>(runs: &[Migrated], f: impl Fn(&Run) -> T, want: T) -> usize {
        runs.iter().filter(|(r, _)| f(r) == want).count()
    }

    // The live state.json of 2026-10-28, reply excerpts scrubbed: the repo is public.
    #[test]
    fn the_live_state_migrates() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/v1-state.json");
        assert_eq!(migrate(&store, &path, 7)?, Some(33));
        let runs: Vec<Migrated> = store.list()?.into_iter().map(|r| (r, vec![])).collect();
        assert_eq!(runs.len(), 33);

        assert_eq!(count(&runs, |r| r.role, Role::Task), 7);
        assert_eq!(count(&runs, |r| r.role, Role::Mate), 12);
        assert_eq!(count(&runs, |r| r.role, Role::Crew), 14);
        assert_eq!(count(&runs, |r| r.phase, Phase::Done), 23);
        assert_eq!(count(&runs, |r| r.phase, Phase::Idle), 9);
        assert_eq!(count(&runs, |r| r.phase, Phase::Working), 1);
        assert!(
            runs.iter()
                .all(|(r, _)| r.mode.pane().is_some() && r.harness == HarnessName::Claude)
        );
        assert!(
            runs.iter()
                .all(|(r, _)| r.ended_at.is_some() == r.phase.ended())
        );

        // every crewmate has its mate as parent, and is owned by it unless v1 gave it back to you
        let by_id = |id: &RunId| runs.iter().find(|(r, _)| &r.id == id).map(|(r, _)| r);
        let crew: Vec<&Run> = runs
            .iter()
            .map(|(r, _)| r)
            .filter(|r| r.role == Role::Crew)
            .collect();
        assert!(crew.iter().all(|c| {
            c.parent
                .as_ref()
                .and_then(by_id)
                .is_some_and(|p| p.role == Role::Mate)
        }));
        assert_eq!(crew.iter().filter(|c| c.owner == Owner::User).count(), 1);
        // a live crewmate answers to the live mate of its name, not a gone one
        let live_crew: Vec<&&Run> = crew.iter().filter(|c| c.phase.live()).collect();
        assert_eq!(live_crew.len(), 6);
        assert!(live_crew.iter().all(|c| {
            c.parent
                .as_ref()
                .and_then(by_id)
                .is_some_and(|p| p.phase.live())
        }));

        let mate = runs
            .iter()
            .find(|(r, _)| r.mode.pane().is_some_and(|p| p.0 == "wBX:p7"))
            .map(|(r, _)| r);
        assert!(mate.is_some_and(|m| m.role == Role::Mate
            && m.phase == Phase::Idle
            && m.machine.as_str() == "meerkat"));
        Ok(())
    }

    // An empty drover.db, as a crash right after it was created leaves it, still migrates; a
    // store that has runs never does again.
    #[test]
    fn it_migrates_while_the_store_has_no_runs() -> Result<()> {
        let db = std::env::temp_dir().join(format!("drover-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        drop(Store::open(&db)?);
        let store = Store::open(&db)?;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/v1-state.json");
        let first = migrate(&store, &fixture, 7)?;
        let again = migrate(&store, &fixture, 8)?;
        let n = store.list()?.len();
        std::fs::remove_file(&db)?;
        assert_eq!((first, again, n), (Some(33), None, 33));
        Ok(())
    }

    #[test]
    fn typescript_flags_still_load() -> Result<()> {
        let old = r#"{
            "123": {"machine":"host","pane":"w1:p1","workspace":"w1","kind":"claude","repo":"r","branch":"x",
                "owned":true,"status":"idle","expecting":true,"ending":{"remove":true},"session":"not-a-uuid"},
            "456": {"machine":"host","pane":"w2:p1","workspace":"w2","kind":"claude","repo":"r","pending":"go"},
            "host:w3:p1": {"machine":"host","pane":"w3:p1","workspace":"w3","kind":"claude","repo":"mates","mate":"a","phase":"idle"},
            "host:w4:p1": {"machine":"host","pane":"w4:p1","workspace":"w4","kind":"claude","repo":"drover","owner":"mate","mate":"a","phase":"idle"},
            "host:w5:p1": {"machine":"host","pane":"w5:p1","workspace":"w5","kind":"claude","repo":"s","scout":"scout-abc123","gone":true},
            "host:w6:p1": {"machine":"host","pane":"w6:p1","workspace":"w6","kind":"claude","repo":"r","phase":{"restarting":{"prompt":"p","from":"waiting"}}}
        }"#;
        let runs = runs(old, 1)?;
        let at = |pane: &str| {
            runs.iter()
                .find(|(r, _)| r.mode.pane().is_some_and(|p| p.0 == pane))
        };
        assert!(at("w1:p1").is_some_and(|(r, _)| r.phase == Phase::Working
            && r.session.is_none()
            && r.role == Role::Task));
        assert!(at("w2:p1").is_some_and(|(r, e)| {
            r.phase == Phase::Starting
                && e.first().is_some_and(|(k, d)| {
                    *k == Kind::Requested && d.get("prompt").is_some_and(|p| p == "go")
                })
        }));
        let mate = at("w3:p1").map(|(r, _)| r.id.clone());
        assert!(at("w3:p1").is_some_and(|(r, _)| r.role == Role::Mate));
        assert!(at("w4:p1").is_some_and(|(r, _)| r.role == Role::Crew
            && r.parent == mate
            && mate.clone().is_some_and(|m| r.owner == Owner::Run(m))));
        assert!(at("w5:p1").is_some_and(|(r, _)| r.role == Role::Task && r.phase == Phase::Done));
        assert!(at("w6:p1").is_some_and(|(r, _)| r.phase == Phase::Working));
        Ok(())
    }
}
