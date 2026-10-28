// drover.db: runs, their events, and mates' goal graphs. One connection behind a lock; every
// call is a few rows, so it never waits long.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::{
    Result,
    herdr::{PaneId, SessionId},
    run::{HarnessName, Mode, Owner, Phase, Profile, Role, Run, RunId, now},
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs(
    id TEXT PRIMARY KEY, machine TEXT NOT NULL, cwd TEXT NOT NULL, harness TEXT NOT NULL,
    mode TEXT NOT NULL, pane TEXT, role TEXT NOT NULL, parent TEXT, profile TEXT NOT NULL,
    owner TEXT NOT NULL, session TEXT, phase TEXT NOT NULL, usage TEXT NOT NULL,
    created_at INTEGER NOT NULL, ended_at INTEGER);
CREATE TABLE IF NOT EXISTS events(
    id INTEGER PRIMARY KEY, run TEXT NOT NULL, at INTEGER NOT NULL, kind TEXT NOT NULL,
    data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS events_run ON events(run, id);
CREATE TABLE IF NOT EXISTS goals(run TEXT PRIMARY KEY, graph TEXT NOT NULL, updated_at INTEGER NOT NULL);
";

// What happened to a run. `Requested` holds what a run was asked to do, kept until it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Requested,
    Started,
    Needs,
    Reply,
    Done,
    Failed,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Started => "started",
            Self::Needs => "needs",
            Self::Reply => "reply",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

pub struct Store(Mutex<Connection>);

const COLUMNS: &str = "id, machine, cwd, harness, mode, pane, role, parent, profile, owner, session, phase, usage, created_at, ended_at";

// A runs row as SQLite holds it, checked into a Run after the statement is done.
struct Row {
    id: String,
    machine: String,
    cwd: String,
    harness: String,
    mode: String,
    pane: Option<String>,
    role: String,
    parent: Option<String>,
    profile: String,
    owner: String,
    session: Option<String>,
    phase: String,
    usage: String,
    created_at: i64,
    ended_at: Option<i64>,
}

impl Row {
    fn read(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            machine: r.get(1)?,
            cwd: r.get(2)?,
            harness: r.get(3)?,
            mode: r.get(4)?,
            pane: r.get(5)?,
            role: r.get(6)?,
            parent: r.get(7)?,
            profile: r.get(8)?,
            owner: r.get(9)?,
            session: r.get(10)?,
            phase: r.get(11)?,
            usage: r.get(12)?,
            created_at: r.get(13)?,
            ended_at: r.get(14)?,
        })
    }

    fn run(self) -> Result<Run> {
        let bad = |what: &str, v: &str| err!("run {}: bad {what} {v:?}", self.id);
        let mode = match self.mode.as_str() {
            "pane" => Mode::Pane {
                pane: self.pane.clone().map(PaneId),
            },
            "headless" => Mode::Headless,
            m => return Err(bad("mode", m)),
        };
        Ok(Run {
            id: self.id.parse()?,
            machine: self.machine.parse()?,
            cwd: PathBuf::from(&self.cwd),
            harness: HarnessName::parse(&self.harness)
                .ok_or_else(|| bad("harness", &self.harness))?,
            mode,
            role: Role::parse(&self.role).ok_or_else(|| bad("role", &self.role))?,
            parent: self.parent.as_deref().map(str::parse).transpose()?,
            profile: Profile::parse(&self.profile).ok_or_else(|| bad("profile", &self.profile))?,
            owner: Owner::parse(&self.owner).ok_or_else(|| bad("owner", &self.owner))?,
            session: self.session.as_deref().and_then(SessionId::parse),
            phase: Phase::parse(&self.phase).ok_or_else(|| bad("phase", &self.phase))?,
            usage: serde_json::from_str(&self.usage)?,
            created_at: self.created_at,
            ended_at: self.ended_at,
        })
    }
}

impl Store {
    // `:memory:` for tests.
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        Ok(Self(Mutex::new(db)))
    }

    // A poisoned lock only means a handler panicked; the connection is fine.
    fn db(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // Inserts the run, or replaces the row with the same id.
    pub fn save(&self, r: &Run) -> Result<()> {
        self.db().execute(
            &format!("INSERT OR REPLACE INTO runs({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"),
            params![
                r.id.as_str(),
                r.machine.as_str(),
                r.cwd.to_string_lossy(),
                r.harness.as_str(),
                r.mode.as_str(),
                r.mode.pane().map(|p| p.0.as_str()),
                r.role.as_str(),
                r.parent.as_ref().map(RunId::as_str),
                r.profile.as_str(),
                r.owner.to_string(),
                r.session.as_ref().map(SessionId::as_str),
                r.phase.as_str(),
                serde_json::to_string(&r.usage)?,
                r.created_at,
                r.ended_at,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, id: &RunId) -> Result<Option<Run>> {
        let row = self
            .db()
            .query_row(
                &format!("SELECT {COLUMNS} FROM runs WHERE id = ?1"),
                [id.as_str()],
                Row::read,
            )
            .optional()?;
        row.map(Row::run).transpose()
    }

    // Oldest first.
    pub fn list(&self) -> Result<Vec<Run>> {
        let rows = {
            let db = self.db();
            let mut stmt = db.prepare(&format!(
                "SELECT {COLUMNS} FROM runs ORDER BY created_at, rowid"
            ))?;
            stmt.query_map([], Row::read)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        rows.into_iter().map(Row::run).collect()
    }

    // A pending run that is dropped leaves nothing behind.
    pub fn delete(&self, id: &RunId) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM events WHERE run = ?1", [id.as_str()])?;
        db.execute("DELETE FROM runs WHERE id = ?1", [id.as_str()])?;
        Ok(())
    }

    pub fn event(&self, run: &RunId, kind: Kind, data: &Value) -> Result<()> {
        self.db().execute(
            "INSERT INTO events(run, at, kind, data) VALUES (?1, ?2, ?3, ?4)",
            params![run.as_str(), now(), kind.as_str(), data.to_string()],
        )?;
        Ok(())
    }

    // The data of the run's newest event of this kind.
    pub fn last(&self, run: &RunId, kind: Kind) -> Result<Option<Value>> {
        let data: Option<String> = self
            .db()
            .query_row(
                "SELECT data FROM events WHERE run = ?1 AND kind = ?2 ORDER BY id DESC LIMIT 1",
                [run.as_str(), kind.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(data.map(|d| serde_json::from_str(&d)).transpose()?)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::run::Usage;

    pub fn run(id: &str) -> Result<Run> {
        Ok(Run {
            id: id.parse()?,
            machine: "host".parse()?,
            cwd: "/w/drover.x".into(),
            harness: HarnessName::Claude,
            mode: Mode::Pane {
                pane: Some(PaneId("w1:p1".into())),
            },
            role: Role::Crew,
            parent: Some("r-00000a".parse()?),
            profile: Profile::Actor,
            owner: Owner::Run("r-00000a".parse()?),
            session: SessionId::parse("0b1d1900-aaaa-bbbb-cccc-0123456789ab"),
            phase: Phase::Idle,
            usage: Usage {
                input: 3,
                cost_usd: 0.5,
                ..Usage::default()
            },
            created_at: 1,
            ended_at: None,
        })
    }

    #[test]
    fn a_run_reads_back_as_saved() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        assert!(store.list()?.is_empty());
        let mut r = run("r-00000b")?;
        store.save(&r)?;
        assert_eq!(store.get(&r.id)?.as_ref(), Some(&r));
        assert_eq!(r.workspace().map(|w| w.0), Some("w1".into()));

        r.mode = Mode::Pane { pane: None };
        r.phase = Phase::Pending;
        r.session = None;
        store.save(&r)?;
        assert_eq!(store.list()?, vec![r.clone()]);

        store.event(&r.id, Kind::Requested, &json!({"prompt": "a"}))?;
        store.event(&r.id, Kind::Requested, &json!({"prompt": "b"}))?;
        assert_eq!(
            store.last(&r.id, Kind::Requested)?,
            Some(json!({"prompt": "b"}))
        );
        assert_eq!(store.last(&r.id, Kind::Reply)?, None);
        store.delete(&r.id)?;
        assert!(store.list()?.is_empty() && store.last(&r.id, Kind::Requested)?.is_none());
        Ok(())
    }
}
