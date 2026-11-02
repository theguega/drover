// Where drover keeps things, and the config it reads once at start.
//
// ~/.config/drover/config.toml, every key optional:
//   host = "devbox"                  # this machine's name, as herdr's machine label (default "host")
//   remotes = ["mac", "gpu"]         # ssh names whose herdr server already runs
//   repos_root = "~/.drover-workspace"   # given to drover-wt on this machine as REPOS_ROOT
//   remote_repos_root = "/tmp/repos" # given to drover-wt on every remote, absolute (default: its own)
//   http_port = 7341                 # hooks, events and approvals, on 127.0.0.1 only
//   hook_timeout = 600               # seconds a tool call waits for your approve or deny
//   idle_kill = 900                  # seconds a headless process stays warm after its last turn
//   background_kill = 9000           # the same while it runs background tasks (default 10 × idle_kill)
//   reader_allow = ["notes find *"]  # Bash commands a reader may run, as Claude rules (default: none)
//
// Only that flat subset of TOML is read: `key = "string"`, `key = ["a", "b"]` and `key = 7`.
// A `#` or a `,` inside quotes is text. A key given twice is an error; an unknown one a warning.

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
    pub reader_allow: Vec<String>,
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
            reader_allow: Vec::new(),
        };
        let mut seen: Vec<(&str, usize)> = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = strip_comment(line).trim();
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
            let key = key.trim();
            if let Some((_, first)) = seen.iter().find(|(k, _)| *k == key) {
                bail!("line {}: {key} is already set on line {first}", n + 1);
            }
            seen.push((key, n + 1));
            match key {
                "host" => c.host = string(value).ok_or_else(bad)?.parse()?,
                "remotes" => {
                    c.remotes = list(value)
                        .ok_or_else(bad)?
                        .into_iter()
                        .map(|s| Ok(s.parse()?))
                        .collect::<Result<_>>()?;
                }
                "reader_allow" => {
                    let items = list(value).ok_or_else(bad)?;
                    // Claude reads a malformed Bash rule as bare Bash: every command allowed
                    if let Some(item) = items.iter().find(|i| !safe_rule(i)) {
                        bail!(
                            "line {}: reader_allow item {item:?} is not a command rule: no empty or wildcard-only item, no parentheses or control characters",
                            n + 1
                        );
                    }
                    c.reader_allow = items.into_iter().map(String::from).collect();
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
                k => match KEYS.iter().find(|known| distance(k, known) <= 2) {
                    Some(known) => eprintln!(
                        "config line {}: unknown key {k}, ignored: did you mean {known}?",
                        n + 1
                    ),
                    None => eprintln!("config line {}: unknown key {k}, ignored", n + 1),
                },
            }
        }
        Ok(c)
    }
}

fn string(v: &str) -> Option<&str> {
    v.strip_prefix('"')?.strip_suffix('"')
}

// Replaces the remotes line, or adds one; the rest of the file stays as it is.
pub fn set_remotes(path: &Path, remotes: &[&str]) -> Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    std::fs::write(path, with_remotes(&text, remotes))?;
    Ok(())
}

fn with_remotes(text: &str, remotes: &[&str]) -> String {
    let quoted: Vec<String> = remotes.iter().map(|r| format!("\"{r}\"")).collect();
    let line = format!("remotes = [{}]", quoted.join(", "));
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut found = false;
    for l in &mut lines {
        if l.split_once('=')
            .is_some_and(|(k, _)| k.trim() == "remotes")
        {
            l.clone_from(&line);
            found = true;
        }
    }
    if !found {
        lines.push(line);
    }
    lines.join("\n") + "\n"
}

// `["a", "b"]`; commas inside quotes belong to the item.
fn list(v: &str) -> Option<Vec<&str>> {
    let inner = v.strip_prefix('[')?.strip_suffix(']')?;
    let mut items = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in inner.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                items.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    items.push(&inner[start..]);
    items
        .into_iter()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(string)
        .collect()
}

// A `#` starts a comment unless it is inside a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

fn safe_rule(item: &str) -> bool {
    item.chars().any(|c| !c.is_whitespace() && c != '*')
        && !item
            .chars()
            .any(|c| matches!(c, '(' | ')') || c.is_control())
}

const KEYS: [&str; 9] = [
    "host",
    "remotes",
    "repos_root",
    "remote_repos_root",
    "http_port",
    "hook_timeout",
    "idle_kill",
    "background_kill",
    "reader_allow",
];

// Levenshtein, for "did you mean".
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev.get(j).copied().unwrap_or(0) + usize::from(ca != *cb);
            let del = prev.get(j + 1).copied().unwrap_or(0) + 1;
            let ins = cur.last().copied().unwrap_or(0) + 1;
            cur.push(sub.min(del).min(ins));
        }
        prev = cur;
    }
    prev.last().copied().unwrap_or(0)
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
            "# drover\nhost = \"devbox\"\nremotes = [\"mac\", \"gpu\"]  # two\nhttp_port = 7342\n",
        )?;
        assert_eq!((c.http_port, c.hook_timeout), (7342, 600));
        assert!(Config::parse("http_port = \"7342\"").is_err());
        assert_eq!(c.host.as_str(), "devbox");
        assert_eq!(
            c.remotes.iter().map(Machine::as_str).collect::<Vec<_>>(),
            ["mac", "gpu"]
        );
        assert_eq!(Config::parse("")?.host.as_str(), "host");
        assert!(Config::parse("remotes = [\"-oProxyCommand=x\"]").is_err());
        assert!(Config::parse("host = devbox").is_err());
        assert_eq!(
            Config::parse("remote_repos_root = \"/tmp/r\"")?.remote_repos_root,
            Some("/tmp/r".into())
        );
        assert!(Config::parse("remote_repos_root = \"~/r\"").is_err());
        let c = Config::parse("reader_allow = [\"notes find *\", \"notes get *\"]")?;
        assert_eq!(c.reader_allow, ["notes find *", "notes get *"]);
        assert!(Config::parse("reader_allow = [notes]").is_err());
        Ok(())
    }

    #[test]
    fn remotes_line_is_replaced_or_added() {
        let text = "host = \"devbox\"\nremotes = [\"mac\"]  # two\nhttp_port = 7342\n";
        assert_eq!(
            with_remotes(text, &["mac", "gpu"]),
            "host = \"devbox\"\nremotes = [\"mac\", \"gpu\"]\nhttp_port = 7342\n"
        );
        assert_eq!(
            with_remotes("host = \"devbox\"\n", &["gpu"]),
            "host = \"devbox\"\nremotes = [\"gpu\"]\n"
        );
        assert_eq!(with_remotes("", &[]), "remotes = []\n");
    }

    // A key this version does not read, such as journal_cmd in an older config, warns and starts anyway.
    #[test]
    fn an_unknown_key_is_ignored() -> Result<()> {
        let c = Config::parse("journal_cmd = \"notes log\"\nhttp_port = 7342\n")?;
        assert_eq!(c, Config::parse("http_port = 7342")?);
        Ok(())
    }

    #[test]
    fn a_reader_rule_that_claude_reads_as_bare_bash_is_refused() {
        for bad in [
            "",
            " ",
            "*",
            "* *",
            "true)",
            "(true",
            "true) x",
            "a(b)",
            "a\tb\u{7}",
        ] {
            let e = Config::parse(&format!("reader_allow = [\"ok *\", \"{bad}\"]"))
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(
                e.to_string().starts_with("line 1: reader_allow item"),
                "{bad:?}: {e}"
            );
        }
        assert!(Config::parse("reader_allow = [\"echo *\", \"git log\"]").is_ok());
    }

    #[test]
    fn a_duplicate_key_names_both_lines() {
        let e = Config::parse("http_port = 1\n\nhttp_port = 2")
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert_eq!(e, "line 3: http_port is already set on line 1");
    }

    #[test]
    fn close_keys_are_found() {
        assert_eq!(distance("reader_alow", "reader_allow"), 1);
        assert_eq!(distance("hook_timeout", "hook_timeout"), 0);
        assert!(KEYS.iter().all(|k| distance("retired", k) > 2));
    }

    #[test]
    fn quotes_hide_comments_and_commas() -> Result<()> {
        let c = Config::parse("repos_root = \"/a # b\" # real\nreader_allow = [\"a, b\", \"c\"]")?;
        assert_eq!(c.repos_root, Some("/a # b".into()));
        assert_eq!(c.reader_allow, ["a, b", "c"]);
        Ok(())
    }
}
