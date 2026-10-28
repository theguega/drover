// A mate's goal graph (C16). drover stores it and streams it, and does not interpret it: the
// mate decides what runs next. The shape is checked once, when it is set, so every later read
// and update can trust it.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::{
    Result,
    control::Failure,
    run::{NodeStatus, Review, RunId},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    pub goal: String,
    // the check for the whole goal
    #[serde(default)]
    pub done_when: Option<String>,
    pub nodes: Vec<Node>,
    // [before, after]: `after` starts when `before` has passed
    #[serde(default)]
    pub edges: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    #[serde(default)]
    pub task: Option<String>,
    // something the mate can run or read: a command, a diff, a log line
    pub check: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub review: Review,
    #[serde(default)]
    pub status: NodeStatus,
    #[serde(default)]
    pub round: u32,
    #[serde(default)]
    pub crew: Option<RunId>,
}

// What `goal node` changes: only the fields given.
#[derive(Default)]
pub struct Update {
    pub status: Option<NodeStatus>,
    pub round: Option<u32>,
    pub crew: Option<RunId>,
    pub review: Option<Review>,
}

fn bad(what: String) -> crate::Error {
    Failure::new(
        what,
        "drover goal --help, and the goal.json shape in the mates skill",
    )
    .into()
}

impl Graph {
    pub fn parse(text: &str) -> Result<Self> {
        let g: Self = serde_json::from_str(text).map_err(|e| bad(format!("goal.json: {e}")))?;
        let mut ids = HashSet::new();
        for n in &g.nodes {
            if n.id.is_empty() || !ids.insert(n.id.as_str()) {
                return Err(bad(format!(
                    "goal.json: node id {:?} is empty or repeated",
                    n.id
                )));
            }
        }
        for (a, b) in &g.edges {
            for id in [a, b] {
                if !ids.contains(id.as_str()) {
                    return Err(bad(format!("goal.json: edge names no node {id:?}")));
                }
            }
        }
        Ok(g)
    }

    pub fn update(&mut self, id: &str, u: Update) -> Result<&Node> {
        let n = self
            .nodes
            .iter_mut()
            .find(|n| n.id == id)
            .ok_or_else(|| bad(format!("no node {id:?} in the goal")))?;
        n.status = u.status.unwrap_or(n.status);
        n.round = u.round.unwrap_or(n.round);
        n.review = u.review.unwrap_or(n.review);
        n.crew = u.crew.or_else(|| n.crew.take());
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOAL: &str = r#"{"goal":"g","nodes":[{"id":"a","check":"cargo test"},
        {"id":"b","check":"ls","review":"code","crew":"r-00aa11"}],"edges":[["a","b"]]}"#;

    #[test]
    fn a_graph_round_trips_with_defaults() -> Result<()> {
        let mut g = Graph::parse(GOAL)?;
        let a = g.nodes.first().ok_or("no node")?;
        assert_eq!(
            (a.status, a.round, a.review),
            (NodeStatus::Todo, 0, Review::None)
        );
        let n = g.update(
            "a",
            Update {
                status: Some(NodeStatus::Working),
                round: Some(2),
                crew: Some("r-00bb22".parse()?),
                ..Update::default()
            },
        )?;
        assert_eq!((n.status, n.round), (NodeStatus::Working, 2));
        let n = g.update("b", Update::default())?;
        assert_eq!(n.crew.as_ref().map(RunId::as_str), Some("r-00aa11"));
        assert_eq!(Graph::parse(&serde_json::to_string(&g)?)?, g);
        Ok(())
    }

    #[test]
    fn a_graph_with_a_wrong_shape_is_refused() {
        for text in [
            r#"{"goal":"g","nodes":[{"id":"a"}]}"#,
            r#"{"goal":"g","nodes":[{"id":"a","check":"x","status":"passed"}]}"#,
            r#"{"goal":"g","nodes":[{"id":"a","check":"x"},{"id":"a","check":"y"}]}"#,
            r#"{"goal":"g","nodes":[{"id":"a","check":"x"}],"edges":[["a","z"]]}"#,
            r#"{"goal":"g","nodes":[],"extra":1}"#,
        ] {
            assert!(Graph::parse(text).is_err(), "{text}");
        }
    }
}
