// Where drover keeps things, and the config it reads once at start.
//
// ~/.config/drover/config.toml, every key optional:
//   host = "meerkat"                 # this machine's name, as herdr's machine label (default "host")
//   remotes = ["chicken", "dog"]     # ssh names whose herdr server already runs
//   repos_root = "~/.drover-workspace"   # given to drover-wt on this machine as REPOS_ROOT
//   remote_repos_root = "/tmp/repos" # given to drover-wt on every remote, absolute (default: its own)
//   http_port = 7341                 # hooks, events and approvals, on 127.0.0.1 only
//   hook_timeout = 600               # seconds a tool call waits for your approve or deny
//   idle_kill = 900                  # seconds a headless process stays warm after its last turn
//   background_kill = 9000           # the same while it runs background tasks (default 10 × idle_kill)
//
// Only that flat subset of TOML is read: `key = "string"`, `key = ["a", "b"]` and `key = 7`.

use std::path::{Path, PathBuf};

use crate::{Result, run::Machine};

pub struct Paths {
    pub state: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    // --state-dir, else $XDG_STATE_HOME/drover, else ~/.local/state/drover. The config likewise.
    pub fn new(state: Option<PathBuf>, config: Option<PathBuf>) -> Result<Self> {
        let home = PathBuf::from(std::env::var("HOME").map_err(|_| err!("HOME is not set"))?);
        let xdg = |var: &str, fallback: &str| {
            std::env::var_os(var)
                .filter(|v| !v.is_empty())
                .map_or_else(|| home.join(fallback), PathBuf::from)
                .join("drover")
        };
        Ok(Self {
            state: state.unwrap_or_else(|| xdg("XDG_STATE_HOME", ".local/state")),
            config: config.unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("config.toml")),
        })
    }

    #[must_use]
    pub fn socket(&self) -> PathBuf {
        self.state.join("drover.sock")
    }

    #[must_use]
    pub fn db(&self) -> PathBuf {
        self.state.join("drover.db")
    }
}

#[derive(Debug, PartialEq)]
pub struct Config {
    pub host: Machine,
    pub remotes: Vec<Machine>,
    pub repos_root: Option<PathBuf>,
    pub remote_repos_root: Option<PathBuf>,
    pub http_port: u16,
    pub hook_timeout: u64,
    pub idle_kill: u64,
    pub background_kill: Option<u64>,
}

impl Config {
    // A missing file is the defaults: this machine alone, named "host".
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| err!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::parse(""),
            Err(e) => Err(err!("{}: {e}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut c = Self {
            host: "host".parse()?,
            remotes: Vec::new(),
            repos_root: None,
            remote_repos_root: None,
            http_port: 7341,
            hook_timeout: 600,
            idle_kill: 900,
            background_kill: None,
        };
        for (n, line) in text.lines().enumerate() {
            let line = line.split_once('#').map_or(line, |(l, _)| l).trim();
            if line.is_empty() {
                continue;
            }
            let bad = || {
                err!(
                    "line {}: expected key = \"value\", key = [\"a\", \"b\"] or key = 7",
                    n + 1
                )
            };
            let (key, value) = line.split_once('=').ok_or_else(bad)?;
            let value = value.trim();
            match key.trim() {
                "host" => c.host = string(value).ok_or_else(bad)?.parse()?,
                "remotes" => {
                    let list = value
                        .strip_prefix('[')
                        .and_then(|v| v.strip_suffix(']'))
                        .ok_or_else(bad)?;
                    c.remotes = list
                        .split(',')
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(|v| string(v).ok_or_else(bad).and_then(|s| Ok(s.parse()?)))
                        .collect::<Result<_>>()?;
                }
                "repos_root" => c.repos_root = Some(expand(string(value).ok_or_else(bad)?)),
                // a path on the remotes, so not expanded here
                "remote_repos_root" => {
                    let root = string(value).ok_or_else(bad)?;
                    if !root.starts_with('/') {
                        bail!(
                            "line {}: remote_repos_root is a path on the remotes: give it from /",
                            n + 1
                        );
                    }
                    c.remote_repos_root = Some(root.into());
                }
                "http_port" => c.http_port = value.parse().map_err(|_| bad())?,
                "hook_timeout" => c.hook_timeout = value.parse().map_err(|_| bad())?,
                "idle_kill" => c.idle_kill = value.parse().map_err(|_| bad())?,
                "background_kill" => c.background_kill = Some(value.parse().map_err(|_| bad())?),
                // a key an older or newer drover reads must not stop this one from starting
                k => eprintln!("config line {}: unknown key {k}, ignored", n + 1),
            }
        }
        Ok(c)
    }
}

fn string(v: &str) -> Option<&str> {
    v.strip_prefix('"')?.strip_suffix('"')
}

fn expand(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_flat_subset() -> Result<()> {
        let c = Config::parse(
            "# drover\nhost = \"meerkat\"\nremotes = [\"chicken\", \"dog\"]  # two\nhttp_port = 7342\n",
        )?;
        assert_eq!((c.http_port, c.hook_timeout), (7342, 600));
        assert!(Config::parse("http_port = \"7342\"").is_err());
        assert_eq!(c.host.as_str(), "meerkat");
        assert_eq!(
            c.remotes.iter().map(Machine::as_str).collect::<Vec<_>>(),
            ["chicken", "dog"]
        );
        assert_eq!(Config::parse("")?.host.as_str(), "host");
        assert!(Config::parse("remotes = [\"-oProxyCommand=x\"]").is_err());
        assert!(Config::parse("host = meerkat").is_err());
        assert_eq!(
            Config::parse("remote_repos_root = \"/tmp/r\"")?.remote_repos_root,
            Some("/tmp/r".into())
        );
        assert!(Config::parse("remote_repos_root = \"~/r\"").is_err());
        Ok(())
    }

    // An older config's key, such as v1_state, warns and starts anyway.
    #[test]
    fn an_unknown_key_is_ignored() -> Result<()> {
        let c = Config::parse("v1_state = \"/x/state.json\"\nhttp_port = 7342\n")?;
        assert_eq!(c, Config::parse("http_port = 7342")?);
        Ok(())
    }
}
