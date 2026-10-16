// One short entry per finished task, searchable, injected into new tasks.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex, PoisonError},
};

use rusqlite::{Connection, Row, params};

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Entry {
    pub day: String,
    pub machine: String,
    pub repo: String,
    pub branch: String,
    pub text: String,
}

fn entry(r: &Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        day: r.get(0)?,
        machine: r.get(1)?,
        repo: r.get(2)?,
        branch: r.get(3)?,
        text: r.get(4)?,
    })
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

pub struct Journal(Arc<Mutex<Connection>>);

impl Journal {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute(
            "create virtual table if not exists journal using fts5(day unindexed, machine unindexed, repo, branch, text)",
            [],
        )?;
        Ok(Self(Arc::new(Mutex::new(db))))
    }

    // Queries run on a blocking thread so they never stall the runtime.
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T> {
        let db = Arc::clone(&self.0);
        Ok(tokio::task::spawn_blocking(move || {
            // a poisoned lock only means another query panicked; the connection is fine
            let db = db.lock().unwrap_or_else(PoisonError::into_inner);
            f(&db)
        })
        .await??)
    }

    pub async fn add(&self, e: Entry) -> Result<()> {
        self.run(move |db| {
            db.execute(
                "insert into journal values (?1, ?2, ?3, ?4, ?5)",
                params![e.day, e.machine, e.repo, e.branch, e.text],
            )
            .map(drop)
        })
        .await
    }

    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<Entry>> {
        let m = terms(q);
        if m.is_empty() {
            return Ok(vec![]);
        }
        self.run(move |db| {
            let mut st = db.prepare(&format!(
                "{COLS} where journal match ?1 order by rank limit ?2"
            ))?;
            st.query_map(params![m, limit], entry)?.collect()
        })
        .await
    }

    async fn recent(&self, repo: &str, limit: u32) -> Result<Vec<Entry>> {
        let repo = repo.to_string();
        self.run(move |db| {
            let mut st = db.prepare(&format!(
                "{COLS} where repo = ?1 order by rowid desc limit ?2"
            ))?;
            st.query_map(params![repo, limit], entry)?.collect()
        })
        .await
    }

    // Newest first, with the id `remove` takes. A repo narrows it to that repo.
    pub async fn list(&self, repo: Option<String>, limit: u32) -> Result<Vec<(i64, Entry)>> {
        self.run(move |db| {
            let mut st = db.prepare(
                "select day, machine, repo, branch, text, rowid from journal where ?1 is null or repo = ?1 order by rowid desc limit ?2",
            )?;
            st.query_map(params![repo, limit], |r| Ok((r.get::<_, i64>(5)?, entry(r)?)))?
                .collect()
        })
        .await
    }

    pub async fn remove(&self, id: i64) -> Result<bool> {
        self.run(move |db| {
            Ok(db.execute("delete from journal where rowid = ?1", params![id])? == 1)
        })
        .await
    }

    // Recent entries for the repo plus matches for the prompt, capped so it never grows the context.
    pub async fn context(&self, repo: &str, prompt: &str, cap: usize) -> Result<String> {
        let mut seen = HashSet::new();
        let mut lines = vec![];
        let mut size = 0;
        for e in self
            .recent(repo, 3)
            .await?
            .into_iter()
            .chain(self.search(&format!("{repo} {prompt}"), 5).await?)
        {
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

pub const ENTRY_PROMPT: &str =
    "This task is done. Reply with only a journal entry, five lines at most, no preamble:
what: <one line>
why: <one line>
outcome: <one line, include commit or PR if any>
decisions: <non-obvious choices and their reason>
next: <follow-ups, or none>";

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(repo: &str, text: &str) -> Entry {
        Entry {
            day: "2026-10-01".into(),
            machine: "host".into(),
            repo: repo.into(),
            branch: "feature/x".into(),
            text: text.into(),
        }
    }

    #[tokio::test]
    async fn search_and_context() -> Result<()> {
        let j = Journal::open(":memory:")?;
        j.add(entry("drover", "what: rust port of the discord bridge"))
            .await?;
        j.add(entry("other", "what: unrelated sqlite tuning"))
            .await?;

        assert_eq!(
            terms("a Rust, PORT!! of x_y"),
            r#""rust" OR "port" OR "x_y""#
        );
        assert_eq!(j.search("rust", 5).await?.len(), 1);
        assert!(j.search("a b", 5).await?.is_empty());

        let ctx = j.context("drover", "sqlite", 2000).await?;
        assert!(ctx.contains("rust port") && ctx.contains("sqlite tuning"));
        assert!(j.context("drover", "sqlite", 10).await?.is_empty());

        let (id, _) = j
            .list(Some("drover".into()), 5)
            .await?
            .pop()
            .ok_or("no row")?;
        assert!(j.remove(id).await? && !j.remove(id).await?);
        assert!(j.list(Some("drover".into()), 5).await?.is_empty());
        assert_eq!(j.list(None, 5).await?.len(), 1);
        Ok(())
    }
}
