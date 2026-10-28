// A run is one agent session: where it runs, who drives it, and how far it got. Every name
// here is checked once, at the edge, and stored as the same word it parses from.

use std::{
    fmt,
    hash::{BuildHasher, Hasher, RandomState},
    path::PathBuf,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::herdr::{PaneId, SessionId, WorkspaceId};

// An enum stored and printed as one lowercase word, and parsed back from it.
macro_rules! words {
    ($(#[$meta:meta])* $name:ident { $($variant:ident = $word:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $word),+
                }
            }

            #[must_use]
            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($word => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

words!(HarnessName {
    Claude = "claude",
    Cursor = "cursor",
    Codex = "codex",
    Opencode = "opencode",
    Pi = "pi",
});

words!(Role {
    Task = "task",
    Mate = "mate",
    Crew = "crew",
    Chat = "chat",
});

words!(Profile {
    Reader = "reader",
    Actor = "actor",
});

words!(Phase {
    Pending = "pending",
    Starting = "starting",
    Working = "working",
    Needs = "needs",
    Idle = "idle",
    Done = "done",
    Failed = "failed",
});

impl Phase {
    // Has an agent, or is about to: what the event watcher follows.
    #[must_use]
    pub fn live(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Working | Self::Needs | Self::Idle
        )
    }

    #[must_use]
    pub fn ended(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

// `r-` and six hex digits.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId(String);

impl RunId {
    #[must_use]
    pub fn random() -> Self {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        Self(format!("r-{:06x}", hasher.finish() & 0xff_ffff))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RunId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let hex = s.strip_prefix("r-").unwrap_or_default();
        if hex.len() == 6 && hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
            Ok(Self(s.into()))
        } else {
            Err(format!("{s:?} is not a run id: r- and six hex digits"))
        }
    }
}

impl TryFrom<String> for RunId {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<RunId> for String {
    fn from(r: RunId) -> Self {
        r.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// A machine's name: the host's own, or an ssh name. It reaches ssh as one argument.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Machine(String);

impl Machine {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Machine {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        let ok = !s.is_empty()
            && !s.starts_with('-')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-@".contains(c));
        if ok {
            Ok(Self(s.into()))
        } else {
            Err(format!("{s:?} is not a machine name"))
        }
    }
}

impl TryFrom<String> for Machine {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<Machine> for String {
    fn from(m: Machine) -> Self {
        m.0
    }
}

impl fmt::Display for Machine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// A pane run has no pane while it waits for approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Pane { pane: Option<PaneId> },
    Headless,
}

impl Mode {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pane { .. } => "pane",
            Self::Headless => "headless",
        }
    }

    #[must_use]
    pub fn pane(&self) -> Option<&PaneId> {
        match self {
            Self::Pane { pane } => pane.as_ref(),
            Self::Headless => None,
        }
    }
}

// Who may type into a run: you, or the run that asked for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    User,
    Run(RunId),
}

impl Owner {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            id => id.parse().ok().map(Self::Run),
        }
    }

    // `from` is the run a request came from, None for you.
    #[must_use]
    pub fn is(&self, from: Option<&RunId>) -> bool {
        match (self, from) {
            (Self::User, None) => true,
            (Self::Run(owner), Some(from)) => owner == from,
            _ => false,
        }
    }
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User => f.write_str("user"),
            Self::Run(r) => r.fmt(f),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost_usd: f64,
}

impl std::ops::AddAssign<&Usage> for Usage {
    fn add_assign(&mut self, u: &Usage) {
        self.input += u.input;
        self.output += u.output;
        self.cache_read += u.cache_read;
        self.cache_write += u.cache_write;
        self.cost_usd += u.cost_usd;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub id: RunId,
    pub machine: Machine,
    pub cwd: PathBuf, // a worktree; empty until a pending run is approved
    pub harness: HarnessName,
    pub mode: Mode,
    pub role: Role,
    pub parent: Option<RunId>,
    pub profile: Profile,
    pub owner: Owner,
    pub session: Option<SessionId>,
    pub phase: Phase,
    pub usage: Usage,
    pub created_at: i64,
    pub ended_at: Option<i64>,
}

impl Run {
    #[must_use]
    pub fn workspace(&self) -> Option<WorkspaceId> {
        self.mode.pane()?.workspace()
    }
}

#[must_use]
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().try_into().unwrap_or(i64::MAX))
}

// Unix seconds as UTC, `2026-10-19T16:12:04Z`. Days to a civil date as in Hinnant's `civil_from_days`.
#[must_use]
pub fn rfc3339(secs: i64) -> String {
    let (days, t) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t % 3600 / 60,
        t % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_print_as_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_792_426_324), "2026-10-19T16:12:04Z");
    }

    #[test]
    fn ids_and_names_are_checked() {
        assert!(RunId::random().as_str().parse::<RunId>().is_ok());
        assert!("r-1a2b3c".parse::<RunId>().is_ok());
        assert!("r-1A2B3C".parse::<RunId>().is_err());
        assert!("r-1a2b3".parse::<RunId>().is_err());
        assert!("meerkat".parse::<Machine>().is_ok());
        assert!("-oProxyCommand=x".parse::<Machine>().is_err());
        assert!("a b".parse::<Machine>().is_err());
    }

    #[test]
    fn words_round_trip() {
        for p in [Phase::Pending, Phase::Needs, Phase::Failed] {
            assert_eq!(Phase::parse(p.as_str()), Some(p));
        }
        assert_eq!(Owner::parse("user"), Some(Owner::User));
        assert!(Owner::parse("r-00aa11").is_some_and(|o| o.to_string() == "r-00aa11"));
        assert_eq!(Owner::parse("mate"), None);
    }

    #[test]
    fn only_the_owner_types() -> Result<(), String> {
        let mate: RunId = "r-00aa11".parse()?;
        let other: RunId = "r-00aa22".parse()?;
        assert!(Owner::User.is(None));
        assert!(!Owner::User.is(Some(&mate)));
        assert!(Owner::Run(mate.clone()).is(Some(&mate)));
        assert!(!Owner::Run(mate).is(Some(&other)));
        Ok(())
    }
}
