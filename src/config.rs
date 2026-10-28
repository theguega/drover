// Where drover keeps things, and the config it reads once at start.
//
// ~/.config/drover/config.toml, every key optional:
//   host = "meerkat"                 # this machine's name, as herdr's machine label (default "host")
//   remotes = ["chicken", "dog"]     # ssh names whose herdr server already runs
//   repos_root = "~/.drover-workspace"   # given to drover-wt on this machine as REPOS_ROOT
//   v1_state = "~/Developer/drover/state.json"   # migrated into a new drover.db, once
//
// Only that flat subset of TOML is read: `key = "string"` and `key = ["a", "b"]`.

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
    pub v1_state: Option<PathBuf>,
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
            v1_state: None,
        };
        for (n, line) in text.lines().enumerate() {
            let line = line.split_once('#').map_or(line, |(l, _)| l).trim();
            if line.is_empty() {
                continue;
            }
            let bad = || {
                err!(
                    "line {}: expected key = \"value\" or key = [\"a\", \"b\"]",
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
                "v1_state" => c.v1_state = Some(expand(string(value).ok_or_else(bad)?)),
                k => bail!("line {}: unknown key {k}", n + 1),
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
            "# drover\nhost = \"meerkat\"\nremotes = [\"chicken\", \"dog\"]  # two\nv1_state = \"/x/state.json\"\n",
        )?;
        assert_eq!(c.host.as_str(), "meerkat");
        assert_eq!(
            c.remotes.iter().map(Machine::as_str).collect::<Vec<_>>(),
            ["chicken", "dog"]
        );
        assert_eq!(c.v1_state, Some("/x/state.json".into()));
        assert_eq!(Config::parse("")?.host.as_str(), "host");
        assert!(Config::parse("remotes = [\"-oProxyCommand=x\"]").is_err());
        assert!(Config::parse("hots = \"x\"").is_err());
        assert!(Config::parse("host = meerkat").is_err());
        Ok(())
    }
}
