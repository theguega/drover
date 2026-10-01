// One short entry per finished task, searchable, injected into new tasks.

use std::{
    collections::HashSet,
    sync::{Mutex, MutexGuard, PoisonError},
};

use rusqlite::{Connection, Result, Row, params};

pub struct Entry {
    pub day: String,
    pub machine: String,
    pub repo: String,
    pub branch: String,
    pub text: String,
}

pub struct Journal(Mutex<Connection>);

fn entry(r: &Row) -> Result<Entry> {
    Ok(Entry { day: r.get(0)?, machine: r.get(1)?, repo: r.get(2)?, branch: r.get(3)?, text: r.get(4)? })
}

const COLS: &str = "select day, machine, repo, branch, text from journal";

// Up to 12 words of 3+ letters, digits or underscores, OR'd together for FTS5.
#[must_use]
fn terms(q: &str) -> String {
    q.to_lowercase()
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
        .filter(|t| t.len() >= 3)
        .take(12)
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[must_use]
pub fn format(e: &Entry) -> String {
    format!("{} {} {}\n{}", e.day, e.repo, e.branch, e.text)
}

impl Journal {
    // A poisoned lock only means another query panicked; the connection itself is fine.
    fn db(&self) -> MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn open(path: &str) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute(
            "create virtual table if not exists journal using fts5(day unindexed, machine unindexed, repo, branch, text)",
            [],
        )?;
        Ok(Self(Mutex::new(db)))
    }

    pub fn add(&self, e: &Entry) -> Result<()> {
        self.db().execute(
            "insert into journal values (?1, ?2, ?3, ?4, ?5)",
            params![e.day, e.machine, e.repo, e.branch, e.text],
        )?;
        Ok(())
    }

    pub fn search(&self, q: &str, limit: u32) -> Result<Vec<Entry>> {
        let m = terms(q);
        if m.is_empty() {
            return Ok(vec![]);
        }
        let db = self.db();
        let mut st = db.prepare(&format!("{COLS} where journal match ?1 order by rank limit ?2"))?;
        st.query_map(params![m, limit], entry)?.collect()
    }

    fn recent(&self, repo: &str, limit: u32) -> Result<Vec<Entry>> {
        let db = self.db();
        let mut st = db.prepare(&format!("{COLS} where repo = ?1 order by rowid desc limit ?2"))?;
        st.query_map(params![repo, limit], entry)?.collect()
    }

    // Recent entries for the repo plus matches for the prompt, capped so it never grows the context.
    pub fn context(&self, repo: &str, prompt: &str, cap: usize) -> Result<String> {
        let mut seen = HashSet::new();
        let mut lines = vec![];
        let mut size = 0;
        for e in self.recent(repo, 3)?.into_iter().chain(self.search(&format!("{repo} {prompt}"), 5)?) {
            let s = format(&e);
            let n = s.chars().count();
            if seen.contains(&s) || size + n > cap {
                continue;
            }
            seen.insert(s.clone());
            lines.push(s);
            size += n;
        }
        Ok(lines.join("\n\n"))
    }
}

pub const ENTRY_PROMPT: &str = "This task is done. Reply with only a journal entry, five lines at most, no preamble:
what: <one line>
why: <one line>
outcome: <one line, include commit or PR if any>
decisions: <non-obvious choices and their reason>
next: <follow-ups, or none>
If you learned something durable about this repo, also save it to your project memory before replying.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_and_context() -> Result<()> {
        let j = Journal::open(":memory:")?;
        let e = |repo: &str, text: &str| Entry {
            day: "2026-10-01".into(),
            machine: "meerkat".into(),
            repo: repo.into(),
            branch: "theo/x".into(),
            text: text.into(),
        };
        j.add(&e("drover", "what: rust port of the discord bridge"))?;
        j.add(&e("other", "what: unrelated sqlite tuning"))?;

        assert_eq!(terms("a Rust, PORT!! of x_y"), r#""rust" OR "port" OR "x_y""#);
        assert_eq!(j.search("rust", 5)?.len(), 1);
        assert!(j.search("a b", 5)?.is_empty());

        let ctx = j.context("drover", "sqlite", 2000)?;
        assert!(ctx.contains("rust port") && ctx.contains("sqlite tuning"));
        assert!(j.context("drover", "sqlite", 10)?.is_empty());
        Ok(())
    }
}
