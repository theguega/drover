// A harness that reads its hooks from a file in the run's cwd (cursor: .cursor/hooks.json) gets
// ours merged into the repo's own, for as long as the run lives. The file's owner is the repo's
// user, so nothing here may lose what is theirs:
// - what the file was, and what drover wrote, are kept in `<dir>/<run>.json`, outside the repo
// - installing again (before every process) rewrites the file only when it is not what drover
//   wrote; whatever it is then, minus our entries, becomes the file to give back
// - restoring gives the original back only if the file is still exactly what drover wrote; if it
//   changed, the user's version stays, without our entries, and the caller is told
// - the file stays out of `git add -A`: skip-worktree when the repo tracks it, else a line in
//   .git/info/exclude, shared by the runs of a repo, so the last of them takes it out
// A record left by a crash is repaired by `restore` once its run is over.

use std::{
    io::{Error, ErrorKind, Result},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Serialize, Deserialize)]
struct Swap {
    cwd: PathBuf,
    rel: String,
    path: PathBuf,
    // the file before drover, none if there was none
    orig: Option<String>,
    // what drover wrote
    ours: String,
    skip_worktree: bool,
    exclude: Option<PathBuf>,
}

fn record(dir: &Path, run: &str) -> PathBuf {
    dir.join(format!("{run}.json"))
}

fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn load(dir: &Path, run: &str) -> Result<Option<Swap>> {
    read(&record(dir, run))?
        .map(|t| serde_json::from_str(&t).map_err(Error::other))
        .transpose()
}

fn bad(path: &Path, what: &str) -> Error {
    Error::other(format!("{}: {what}", path.display()))
}

fn pretty(v: &Value) -> Result<String> {
    serde_json::to_string_pretty(v).map_err(Error::other)
}

// `text` without the entries that call this run's hooks. Unchanged text when there are none, so a
// repo's file keeps its bytes; an error when it is not JSON, which drover does not guess at.
fn without_ours(path: &Path, text: &str, run: &str) -> Result<String> {
    let mut file: Value = serde_json::from_str(text).map_err(|e| bad(path, &e.to_string()))?;
    let mut removed = false;
    let hooks = file.get_mut("hooks").and_then(Value::as_object_mut);
    for entries in hooks.into_iter().flat_map(|h| h.values_mut()) {
        if let Some(list) = entries.as_array_mut() {
            let before = list.len();
            list.retain(|e| {
                !e.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(run))
            });
            removed |= list.len() != before;
        }
    }
    if removed {
        pretty(&file)
    } else {
        Ok(text.into())
    }
}

fn merged(path: &Path, orig: Option<&str>, ours: &Value) -> Result<String> {
    let mut file: Value = match orig {
        Some(text) => serde_json::from_str(text).map_err(|e| bad(path, &e.to_string()))?,
        None => json!({}),
    };
    let file = file
        .as_object_mut()
        .ok_or_else(|| bad(path, "not an object"))?;
    if let Some(version) = ours.get("version") {
        file.entry("version").or_insert_with(|| version.clone());
    }
    let hooks = file
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| bad(path, "hooks is not an object"))?;
    for (event, entries) in ours
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let list = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| bad(path, &format!("{event} is not a list")))?;
        list.extend(entries.as_array().cloned().unwrap_or_default());
    }
    pretty(&Value::Object(file.clone()))
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn exclude_line(rel: &str) -> String {
    format!("/{rel}")
}

// Puts ours in `cwd/rel`, as described above. Idempotent: run it before every process.
pub fn install(dir: &Path, run: &str, cwd: &Path, rel: &str, ours: &Value) -> Result<()> {
    let path = cwd.join(rel);
    let known = load(dir, run)?;
    let current = read(&path)?;
    if let (Some(s), Some(c)) = (&known, &current)
        && *c == s.ours
    {
        return Ok(());
    }
    let orig = current.map(|c| without_ours(&path, &c, run)).transpose()?;
    let ours_text = merged(&path, orig.as_deref(), ours)?;
    let mut swap = known.unwrap_or_else(|| Swap {
        cwd: cwd.into(),
        rel: rel.into(),
        path: path.clone(),
        orig: None,
        ours: String::new(),
        skip_worktree: false,
        exclude: None,
    });
    swap.orig = orig;
    swap.ours = ours_text;
    if !swap.skip_worktree && swap.exclude.is_none() {
        let tracked = git(cwd, &["ls-files", "--error-unmatch", "--", rel]).is_some();
        if tracked {
            swap.skip_worktree =
                git(cwd, &["update-index", "--skip-worktree", "--", rel]).is_some();
        } else if let Some(file) = git(cwd, &["rev-parse", "--git-path", "info/exclude"]) {
            let file = cwd.join(file);
            let line = exclude_line(rel);
            let text = read(&file)?.unwrap_or_default();
            if !text.lines().any(|l| l == line) {
                let sep = if text.is_empty() || text.ends_with('\n') {
                    ""
                } else {
                    "\n"
                };
                if let Some(dir) = file.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&file, format!("{text}{sep}{line}\n"))?;
            }
            swap.exclude = Some(file);
        }
    }
    std::fs::create_dir_all(dir)?;
    // the record first: a crash after it leaves a file `restore` knows what to do with
    std::fs::write(record(dir, run), serde_json::to_string(&swap)?)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &swap.ours)
}

// Gives the file back, once nothing of the run is running. Some(note) when it had changed and the
// user's version stayed.
pub fn restore(dir: &Path, run: &str) -> Result<Option<String>> {
    let Some(swap) = load(dir, run)? else {
        return Ok(None);
    };
    let path = &swap.path;
    let mut note = None;
    match read(path)? {
        Some(c) if c == swap.ours => match &swap.orig {
            Some(orig) => std::fs::write(path, orig)?,
            None => {
                std::fs::remove_file(path)?;
                // only an empty .cursor goes
                if let Some(parent) = path.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
            }
        },
        Some(c) => {
            note = Some(format!(
                "{} changed during the run: kept yours",
                path.display()
            ));
            match without_ours(path, &c, run) {
                Ok(kept) if kept != c => std::fs::write(path, kept)?,
                Ok(_) => {}
                Err(e) => note = Some(format!("{e}: left as it is")),
            }
        }
        // gone: a file of drover's alone was cleaned away, or theirs was removed, which stays so
        None => {
            note = swap.orig.is_some().then(|| {
                format!(
                    "{} was removed during the run: left removed",
                    path.display()
                )
            });
        }
    }
    if swap.skip_worktree {
        git(
            &swap.cwd,
            &["update-index", "--no-skip-worktree", "--", &swap.rel],
        );
    }
    std::fs::remove_file(record(dir, run))?;
    if let Some(file) = &swap.exclude {
        // runs of one repo share the line
        let shared = std::fs::read_dir(dir)?
            .flatten()
            .filter_map(|e| read(&e.path()).ok().flatten())
            .filter_map(|t| serde_json::from_str::<Swap>(&t).ok())
            .any(|s| s.exclude.as_ref() == Some(file));
        if !shared && let Some(text) = read(file)? {
            let line = exclude_line(&swap.rel);
            let kept: Vec<&str> = text.lines().filter(|l| *l != line).collect();
            let tail = if kept.is_empty() { "" } else { "\n" };
            std::fs::write(file, format!("{}{tail}", kept.join("\n")))?;
        }
    }
    Ok(note)
}

// The runs that left a swap.
pub fn left(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(String::from)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REL: &str = ".cursor/hooks.json";
    const THEIRS: &str =
        r#"{"version":1,"hooks":{"beforeShellExecution":[{"command":"./lint.sh"}]}}"#;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Result<Self> {
            let d = std::env::temp_dir().join(format!("drover-swap-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(d.join("repo"))?;
            git(&d.join("repo"), &["init", "-q"]).ok_or_else(|| Error::other("git init"))?;
            Ok(Self(d))
        }
        fn repo(&self) -> PathBuf {
            self.0.join("repo")
        }
        fn swaps(&self) -> PathBuf {
            self.0.join("swaps")
        }
        fn file(&self) -> PathBuf {
            self.repo().join(REL)
        }
        fn put(&self, text: &str) -> Result<()> {
            std::fs::create_dir_all(self.repo().join(".cursor"))?;
            std::fs::write(self.file(), text)
        }
        fn install(&self, run: &str) -> Result<()> {
            let ours = json!({"version": 1, "hooks": {"beforeShellExecution": [{"command": format!("drover hook x/hooks/{run}")}]}});
            install(&self.swaps(), run, &self.repo(), REL, &ours)
        }
        fn restore(&self, run: &str) -> Result<Option<String>> {
            restore(&self.swaps(), run)
        }
        fn commands(&self) -> Result<Vec<String>> {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(self.file())?)?;
            Ok(v.pointer("/hooks/beforeShellExecution")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|h| h.get("command")?.as_str().map(String::from))
                .collect())
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn no_file_of_theirs_ours_alone_and_gone_after() -> Result<()> {
        let d = Dir::new("none")?;
        d.install("r-1")?;
        assert_eq!(d.commands()?, ["drover hook x/hooks/r-1"]);
        assert_eq!(d.restore("r-1")?, None);
        assert!(!d.repo().join(".cursor").exists());
        assert!(left(&d.swaps()).is_empty());
        Ok(())
    }

    // Their hooks keep running beside ours; installing again adds ours once; they get their bytes.
    #[test]
    fn theirs_are_kept_and_given_back_as_they_were() -> Result<()> {
        let d = Dir::new("theirs")?;
        d.put(THEIRS)?;
        d.install("r-1")?;
        d.install("r-1")?;
        assert_eq!(d.commands()?, ["./lint.sh", "drover hook x/hooks/r-1"]);
        assert_eq!(d.restore("r-1")?, None);
        assert_eq!(std::fs::read_to_string(d.file())?, THEIRS);
        Ok(())
    }

    // A file called hooks.json.drover is theirs too, and nothing here touches it.
    #[test]
    fn a_file_next_to_it_is_left_alone() -> Result<()> {
        let d = Dir::new("sibling")?;
        d.put(THEIRS)?;
        let sibling = d.repo().join(".cursor/hooks.json.drover");
        std::fs::write(&sibling, "mine")?;
        d.install("r-1")?;
        d.restore("r-1")?;
        assert_eq!(std::fs::read_to_string(&sibling)?, "mine");
        assert_eq!(std::fs::read_to_string(d.file())?, THEIRS);
        Ok(())
    }

    // Edited during the run, by the user or the agent: their edit stays, ours goes, and it is said.
    #[test]
    fn an_edit_during_the_run_is_kept() -> Result<()> {
        let d = Dir::new("edit")?;
        d.put(THEIRS)?;
        d.install("r-1")?;
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(d.file())?)?;
        if let Some(hooks) = v.get_mut("hooks").and_then(Value::as_object_mut) {
            hooks.insert("afterFileEdit".into(), json!([{"command": "./fmt.sh"}]));
        }
        std::fs::write(d.file(), serde_json::to_string(&v)?)?;
        let note = d.restore("r-1")?;
        assert!(note.is_some_and(|n| n.contains("kept yours")));
        let v: Value = serde_json::from_str(&std::fs::read_to_string(d.file())?)?;
        assert_eq!(
            v.pointer("/hooks/afterFileEdit/0/command"),
            Some(&json!("./fmt.sh"))
        );
        assert_eq!(
            v.pointer("/hooks/beforeShellExecution/0/command"),
            Some(&json!("./lint.sh"))
        );
        assert_eq!(v.pointer("/hooks/beforeShellExecution/1"), None);
        Ok(())
    }

    // Removed or replaced mid-run (git clean, git checkout .): the next install puts ours back
    // beside whatever is there now, and that is what comes back at the end.
    #[test]
    fn install_repairs_a_swap_that_was_undone() -> Result<()> {
        let d = Dir::new("undone")?;
        d.put(THEIRS)?;
        d.install("r-1")?;
        std::fs::remove_file(d.file())?;
        d.install("r-1")?;
        assert_eq!(d.commands()?, ["drover hook x/hooks/r-1"]);
        d.put(r#"{"version":1,"hooks":{"stop":[{"command":"./new.sh"}]}}"#)?;
        d.install("r-1")?;
        assert_eq!(d.commands()?, ["drover hook x/hooks/r-1"]);
        d.restore("r-1")?;
        assert_eq!(
            std::fs::read_to_string(d.file())?,
            r#"{"version":1,"hooks":{"stop":[{"command":"./new.sh"}]}}"#
        );
        Ok(())
    }

    // A file that is not JSON is not guessed at: no install, and nothing changed.
    #[test]
    fn a_file_that_is_not_json_is_refused() -> Result<()> {
        let d = Dir::new("junk")?;
        d.put("not json")?;
        assert!(d.install("r-1").is_err());
        assert_eq!(std::fs::read_to_string(d.file())?, "not json");
        Ok(())
    }

    // `git add -A` never takes the file: untracked, it is excluded until the last run of the repo
    // ends; tracked, it is skip-worktree.
    #[test]
    fn git_add_all_leaves_it_out() -> Result<()> {
        let d = Dir::new("git")?;
        let staged =
            |d: &Dir| git(&d.repo(), &["diff", "--cached", "--name-only"]).unwrap_or_default();
        d.install("r-1")?;
        d.install("r-2")?;
        git(&d.repo(), &["add", "-A"]);
        assert_eq!(staged(&d), "");
        d.restore("r-1")?;
        git(&d.repo(), &["add", "-A"]);
        assert_eq!(staged(&d), "", "r-2 still runs");
        d.restore("r-2")?;
        let exclude = std::fs::read_to_string(d.repo().join(".git/info/exclude"))?;
        assert!(!exclude.contains("hooks.json"));

        d.put(THEIRS)?;
        git(&d.repo(), &["add", "-A"]);
        git(
            &d.repo(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "x",
            ],
        );
        d.install("r-3")?;
        git(&d.repo(), &["add", "-A"]);
        assert_eq!(staged(&d), "");
        d.restore("r-3")?;
        assert_eq!(
            git(&d.repo(), &["status", "--porcelain"]).unwrap_or_default(),
            ""
        );
        Ok(())
    }

    // After a crash the record is what is left of the run; restoring it undoes the swap.
    #[test]
    fn a_crash_is_repaired_from_the_record() -> Result<()> {
        let d = Dir::new("crash")?;
        d.put(THEIRS)?;
        d.install("r-1")?;
        assert_eq!(left(&d.swaps()), ["r-1"]);
        d.restore("r-1")?;
        assert_eq!(std::fs::read_to_string(d.file())?, THEIRS);
        Ok(())
    }
}
