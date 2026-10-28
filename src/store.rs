// drover.db: runs, their events, and mates' goal graphs. One connection behind a lock; every
// call is a few rows, so it never waits long.

use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::{
    Result,
    harness::Ident,
    herdr::{PaneId, SessionId},
    run::{HarnessName, Machine, Mode, Owner, Phase, Profile, Role, Run, RunId, now, rfc3339},
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
CREATE TABLE IF NOT EXISTS waits(run TEXT PRIMARY KEY, since INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS procs(run TEXT PRIMARY KEY, machine TEXT NOT NULL, pid INTEGER NOT NULL, start TEXT NOT NULL);
";

// What happened to a run: the C9 kinds, and `requested`, what a run was asked to do, which is
// stored and never streamed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Requested,
    Started,
    Text,
    ToolStart,
    ToolEnd,
    Needs,
    Goal,
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
            Self::Text => "text",
            Self::ToolStart => "tool_start",
            Self::ToolEnd => "tool_end",
            Self::Needs => "needs",
            Self::Goal => "goal",
            Self::Reply => "reply",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

// An event as it streams (C9), with its row id for `since`. The usage it carries is lifted out of data.
#[must_use]
pub fn c9(id: i64, r: &Run, at: i64, kind: Kind, data: &Value) -> Value {
    c9_row(
        id,
        r.id.as_str(),
        r.mode.as_str(),
        r.role.as_str(),
        r.parent.as_ref().map(RunId::as_str),
        at,
        kind.as_str(),
        data.clone(),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "one events row joined with its run"
)]
fn c9_row(
    id: i64,
    run: &str,
    mode: &str,
    role: &str,
    parent: Option<&str>,
    at: i64,
    kind: &str,
    mut data: Value,
) -> Value {
    let usage = data.as_object_mut().and_then(|d| d.remove("usage"));
    json!({
        "id": id, "run": run, "mode": mode, "role": role, "parent": parent,
        "at": rfc3339(at), "kind": kind, "data": data, "usage": usage,
    })
}

pub struct Store(Mutex<Connection>);

const OPEN: &str = "WHERE phase NOT IN ('done', 'failed')";

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
        write(&self.db(), "INSERT OR REPLACE", r)
    }

    // New runs with their first events, all or none. An id taken already fails the lot.
    pub fn insert_all(&self, runs: &[(Run, Vec<(Kind, Value)>)]) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        for (r, events) in runs {
            write(&tx, "INSERT", r)?;
            for (kind, data) in events {
                add_event(&tx, &r.id, *kind, data)?;
            }
        }
        tx.commit()?;
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
        self.select("", [])
    }

    // Runs not done or failed: what the desk and the watchers look at.
    pub fn list_open(&self) -> Result<Vec<Run>> {
        self.select(OPEN, [])
    }

    pub fn children(&self, id: &RunId) -> Result<Vec<Run>> {
        self.select("WHERE parent = ?1", [id.as_str()])
    }

    fn select(&self, filter: &str, params: impl rusqlite::Params) -> Result<Vec<Run>> {
        let rows = {
            let db = self.db();
            let mut stmt = db.prepare(&format!(
                "SELECT {COLUMNS} FROM runs {filter} ORDER BY created_at, rowid"
            ))?;
            stmt.query_map(params, Row::read)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        rows.into_iter().map(Row::run).collect()
    }

    pub fn total(&self) -> Result<usize> {
        let n: i64 = self
            .db()
            .query_row("SELECT count(*) FROM runs", [], |r| r.get(0))?;
        Ok(usize::try_from(n).unwrap_or_default())
    }

    // Each open run's newest reply event, in one query.
    pub fn open_replies(&self) -> Result<HashMap<String, Value>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!(
            "SELECT e.run, e.data FROM events e WHERE e.id IN (
                SELECT max(id) FROM events WHERE kind = 'reply'
                AND run IN (SELECT id FROM runs {OPEN}) GROUP BY run)"
        ))?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(run, data)| Ok((run, serde_json::from_str(&data)?)))
            .collect()
    }

    // Moves events from before `before` to the end of `file`, one JSON line each, and returns
    // how many. An open run keeps its own: its requested, started and last reply events drive it.
    // The lines are written before the rows go, so a crash between repeats lines, never loses one.
    pub fn archive(&self, before: i64, file: &Path) -> Result<usize> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let old = format!("at < ?1 AND run NOT IN (SELECT id FROM runs {OPEN})");
        let lines = {
            let mut stmt = tx.prepare(&format!(
                "SELECT id, run, at, kind, data FROM events WHERE {old} ORDER BY id"
            ))?;
            stmt.query_map([before], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "run": r.get::<_, String>(1)?, "at": r.get::<_, i64>(2)?,
                    "kind": r.get::<_, String>(3)?, "data": r.get::<_, String>(4)?,
                }))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if lines.is_empty() {
            return Ok(0);
        }
        let mut out = String::new();
        for mut line in lines.iter().cloned() {
            // data goes back to the JSON it was
            if let Some(d) = line.get_mut("data")
                && let Some(text) = d.as_str()
            {
                *d = serde_json::from_str(text)?;
            }
            out.push_str(&line.to_string());
            out.push('\n');
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)?
            .write_all(out.as_bytes())?;
        tx.execute(&format!("DELETE FROM events WHERE {old}"), [before])?;
        tx.commit()?;
        Ok(lines.len())
    }

    // A pending run that is dropped leaves nothing behind.
    pub fn delete(&self, id: &RunId) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM events WHERE run = ?1", [id.as_str()])?;
        db.execute("DELETE FROM goals WHERE run = ?1", [id.as_str()])?;
        db.execute("DELETE FROM waits WHERE run = ?1", [id.as_str()])?;
        db.execute("DELETE FROM runs WHERE id = ?1", [id.as_str()])?;
        Ok(())
    }

    pub fn goal(&self, run: &RunId) -> Result<Option<Value>> {
        let graph: Option<String> = self
            .db()
            .query_row(
                "SELECT graph FROM goals WHERE run = ?1",
                [run.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(graph.map(|g| serde_json::from_str(&g)).transpose()?)
    }

    pub fn set_goal(&self, run: &RunId, graph: &Value) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO goals(run, graph, updated_at) VALUES (?1, ?2, ?3)",
            params![run.as_str(), graph.to_string(), now()],
        )?;
        Ok(())
    }

    // The last event id a run's `wait` returned: the next one reads after it.
    pub fn wait_cursor(&self, run: &RunId) -> Result<i64> {
        Ok(self
            .db()
            .query_row(
                "SELECT since FROM waits WHERE run = ?1",
                [run.as_str()],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    pub fn set_wait_cursor(&self, run: &RunId, since: i64) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO waits(run, since) VALUES (?1, ?2)",
            params![run.as_str(), since],
        )?;
        Ok(())
    }

    // The new row's id and time.
    pub fn event(&self, run: &RunId, kind: Kind, data: &Value) -> Result<(i64, i64)> {
        let db = self.db();
        let at = add_event(&db, run, kind, data)?;
        Ok((db.last_insert_rowid(), at))
    }

    // Streamed events after row `since`, oldest first, as C9.
    pub fn events_after(
        &self,
        since: i64,
        run: Option<&RunId>,
        role: Option<Role>,
    ) -> Result<Vec<Value>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT e.id, e.run, r.mode, r.role, r.parent, e.at, e.kind, e.data FROM events e
             JOIN runs r ON r.id = e.run
             WHERE e.id > ?1 AND e.kind != 'requested' AND (?2 IS NULL OR e.run = ?2) AND (?3 IS NULL OR r.role = ?3)
             ORDER BY e.id LIMIT 1000",
        )?;
        let rows = stmt
            .query_map(
                params![since, run.map(RunId::as_str), role.map(Role::as_str)],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, run, mode, role, parent, at, kind, data)| {
                Ok(c9_row(
                    id,
                    &run,
                    &mode,
                    &role,
                    parent.as_deref(),
                    at,
                    &kind,
                    serde_json::from_str(&data)?,
                ))
            })
            .collect()
    }

    pub fn last_event(&self) -> Result<i64> {
        Ok(self
            .db()
            .query_row("SELECT coalesce(max(id), 0) FROM events", [], |r| r.get(0))?)
    }

    // How many tool calls of the run asked for a permission: `needs` events with a perm.
    pub fn perms(&self, run: &RunId) -> Result<usize> {
        let n: i64 = self.db().query_row(
            "SELECT count(*) FROM events WHERE run = ?1 AND kind = 'needs' AND data ->> '$.perm' IS NOT NULL",
            [run.as_str()],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or_default())
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

impl Store {
    // A headless process while it runs, so a restarted service can end one it left behind.
    pub fn remember(&self, run: &RunId, machine: &Machine, p: &Ident) -> Result<()> {
        self.db().execute(
            "INSERT OR REPLACE INTO procs(run, machine, pid, start) VALUES (?1, ?2, ?3, ?4)",
            params![run.as_str(), machine.as_str(), p.pid, p.start],
        )?;
        Ok(())
    }

    pub fn forget(&self, run: &RunId, p: &Ident) -> Result<()> {
        self.db().execute(
            "DELETE FROM procs WHERE run = ?1 AND pid = ?2 AND start = ?3",
            params![run.as_str(), p.pid, p.start],
        )?;
        Ok(())
    }

    // Recorded processes, of one run or of all.
    pub fn procs(&self, run: Option<&RunId>) -> Result<Vec<(RunId, Machine, Ident)>> {
        let db = self.db();
        let mut stmt =
            db.prepare("SELECT run, machine, pid, start FROM procs WHERE ?1 IS NULL OR run = ?1")?;
        let rows = stmt
            .query_map([run.map(RunId::as_str)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, u32>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(run, machine, pid, start)| {
                Ok((run.parse()?, machine.parse()?, Ident { pid, start }))
            })
            .collect()
    }
}

// `verb` is INSERT, or INSERT OR REPLACE.
fn write(db: &Connection, verb: &str, r: &Run) -> Result<()> {
    db.execute(
        &format!("{verb} INTO runs({COLUMNS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)"),
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

fn add_event(db: &Connection, run: &RunId, kind: Kind, data: &Value) -> Result<i64> {
    let at = now();
    db.execute(
        "INSERT INTO events(run, at, kind, data) VALUES (?1, ?2, ?3, ?4)",
        params![run.as_str(), at, kind.as_str(), data.to_string()],
    )?;
    Ok(at)
}

#[cfg(test)]
mod tests {
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

    // Old events of ended runs move to the archive, in order; an open run keeps all of its own.
    #[test]
    fn the_archive_takes_old_events_of_ended_runs() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        let mut ended = run("r-0000e1")?;
        ended.phase = Phase::Done;
        let open = run("r-0000e2")?;
        store.save(&ended)?;
        store.save(&open)?;
        for r in [&ended, &open] {
            store.event(&r.id, Kind::Requested, &json!({ "prompt": "go" }))?;
            store.event(&r.id, Kind::Reply, &json!({ "text": "ok" }))?;
        }
        let file =
            std::env::temp_dir().join(format!("drover-archive-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&file);
        assert_eq!(store.archive(0, &file)?, 0); // nothing is older than the epoch
        assert!(!file.exists());

        assert_eq!(store.archive(now() + 1, &file)?, 2);
        assert_eq!(store.archive(now() + 1, &file)?, 0);
        let text = std::fs::read_to_string(&file)?;
        std::fs::remove_file(&file)?;
        let lines: Vec<Value> = text
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        let kinds: Vec<&str> = lines
            .iter()
            .filter_map(|l| l.get("kind")?.as_str())
            .collect();
        assert_eq!(kinds, ["requested", "reply"]);
        assert!(
            lines
                .iter()
                .all(|l| l.get("run").is_some_and(|r| r == "r-0000e1"))
        );
        assert_eq!(
            lines.first().and_then(|l| l.pointer("/data/prompt")),
            Some(&json!("go"))
        );
        assert!(store.last(&ended.id, Kind::Reply)?.is_none());
        assert!(store.last(&open.id, Kind::Requested)?.is_some());
        assert_eq!(
            store.open_replies()?.keys().collect::<Vec<_>>(),
            ["r-0000e2"]
        );
        Ok(())
    }

    // A taken id fails the whole batch loudly, and nothing of it stays.
    #[test]
    fn insert_all_is_all_or_none() -> Result<()> {
        let store = Store::open(Path::new(":memory:"))?;
        let a = run("r-00000c")?;
        let b = run("r-00000d")?;
        let events = vec![(Kind::Reply, json!({ "id": "x" }))];
        let twice = [
            (a.clone(), events.clone()),
            (b, vec![]),
            (a.clone(), vec![]),
        ];
        assert!(store.insert_all(&twice).is_err());
        assert!(store.list()?.is_empty() && store.last(&a.id, Kind::Reply)?.is_none());
        store.insert_all(&[(a.clone(), events)])?;
        assert_eq!(store.list()?, vec![a]);
        Ok(())
    }
}
