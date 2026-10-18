// The desk as one JSON document, for `drover snapshot`. Read only: it holds no state of its own.

use std::{collections::BTreeSet, path::Path};

use serde_json::{Value, json};

use super::*;

// Log lines kept per mate.
const LOG_TAIL: usize = 200;
// Escalations across all mates, newest last.
const ESCALATIONS: usize = 20;
// Characters of a goal file kept.
const GOAL_CAP: usize = 4000;

impl App {
    pub async fn snapshot(&self) -> Result<String> {
        let mut tasks = Vec::new();
        let mut names = BTreeSet::new();
        for (key, cell) in self.cells() {
            let t = cell.lock().await;
            if let Role::Mate(name) = &t.role
                && t.phase != Phase::Gone
            {
                names.insert(name.as_str().to_string());
            }
            tasks.push(task_json(&key, &t));
        }

        let mut mates = Vec::new();
        let mut escalations = Vec::new();
        for name in &names {
            let dir = self.mates.join("state").join(name);
            let log = log_tail(&dir.join("log"));
            for line in &log {
                if let Some((at, text)) = escalation(line) {
                    escalations.push((at, json!({ "at": at, "mate": name, "text": text })));
                }
            }
            let goal = std::fs::read_to_string(dir.join("goal.md")).unwrap_or_default();
            mates.push(json!({ "name": name, "goal": head(&goal, GOAL_CAP), "log": log }));
        }
        escalations.sort_by_key(|(at, _)| *at);
        let start = escalations.len().saturating_sub(ESCALATIONS);
        let escalations: Vec<Value> = escalations
            .into_iter()
            .skip(start)
            .map(|(_, e)| e)
            .collect();

        let reachability = self
            .reachability
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let machines: Vec<Value> = self
            .machines()
            .map(|m| {
                let reach = match reachability.get(&m.name) {
                    Some(Reachability::Reachable) => "reachable",
                    Some(Reachability::Unreachable) => "unreachable",
                    None => "unknown",
                };
                json!({ "name": m.name, "host": m.name == self.host.name, "reach": reach })
            })
            .collect();
        drop(reachability);

        Ok(json!({
            "machines": machines,
            "tasks": tasks,
            "mates": mates,
            "escalations": escalations,
        })
        .to_string())
    }
}

fn task_json(key: &str, t: &Task) -> Value {
    let (role, mate, owner) = match &t.role {
        Role::Task => ("task", None, None),
        Role::Crew { mate, owner } => ("crew", Some(mate.as_str()), Some(owner.as_str())),
        Role::Mate(name) => ("mate", Some(name.as_str()), None),
        Role::Scout(_) => ("scout", None, None),
    };
    json!({
        "key": key,
        "machine": t.machine,
        "pane": t.pane.0,
        "repo": t.repo,
        "branch": t.branch,
        "harness": t.kind,
        "role": role,
        "mate": mate,
        "owner": owner,
        "origin": match t.origin {
            Origin::Opened => "opened",
            Origin::Attached => "attached",
        },
        "phase": phase(&t.phase),
        "status": t.status.map(Status::as_str),
        "reply": t.reply,
        "queued": t.queued.is_some(),
        "inbox": t.inbox.len(),
        "thread": t.thread.is_some(),
    })
}

fn phase(p: &Phase) -> &'static str {
    match p {
        Phase::Starting { .. } => "starting",
        Phase::Idle => "idle",
        Phase::Waiting => "waiting",
        Phase::Ending(_) => "ending",
        Phase::Gone => "gone",
    }
}

// The last lines of a mate's log, one event per line.
fn log_tail(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(String::from).collect();
    let start = lines.len().saturating_sub(LOG_TAIL);
    lines.into_iter().skip(start).collect()
}

// An `escalated` line: its unix time and the question.
fn escalation(line: &str) -> Option<(u64, &str)> {
    let (at, rest) = line.split_once(' ')?;
    let (event, text) = rest.split_once(' ')?;
    (event == log::Event::Escalated.as_str()).then(|| (at.parse().unwrap_or(0), text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_escalated_lines_are_escalations() {
        assert_eq!(
            escalation("1781600000 escalated docs stuck: which section?"),
            Some((1_781_600_000, "docs stuck: which section?"))
        );
        assert_eq!(escalation("1781600000 crew replied DONE"), None);
        assert_eq!(escalation("garbage"), None);
    }
}
