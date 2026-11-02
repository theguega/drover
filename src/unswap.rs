// Before ACP, a cursor run got drover's hooks merged into its worktree's .cursor/hooks.json, and
// the swap was recorded in <state>/hooks/<run>.json. Those hooks run `drover hook`, which this
// binary no longer has: their exit 2 would deny every shell call in the worktree. So at start every
// recorded swap is given back, once, as the old code did at a run's end: the file as it was, or the
// user's edits without our entries, and the skip-worktree bit or the .git/info/exclude line gone.

use std::{
    io::{Error, ErrorKind, Result},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
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

fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

// `text` without the entries that call this run's hooks; unchanged text when there are none.
fn without_ours(text: &str, run: &str) -> Result<String> {
    let mut file: Value = serde_json::from_str(text).map_err(Error::other)?;
    let mut removed = false;
    let hooks = file.get_mut("hooks").and_then(Value::as_object_mut);
    for list in hooks
        .into_iter()
        .flat_map(|h| h.values_mut())
        .filter_map(Value::as_array_mut)
    {
        let before = list.len();
        list.retain(|e| {
            !e.get("command")
                .and_then(Value::as_str)
                .is_some_and(|c| c.contains(run))
        });
        removed |= list.len() != before;
    }
    if removed {
        serde_json::to_string_pretty(&file).map_err(Error::other)
    } else {
        Ok(text.into())
    }
}

fn git(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// Gives back the swap of `run`, recorded at `record`. Some(note) when the user's version stayed.
fn restore(record: &Path, run: &str) -> Result<Option<String>> {
    let Some(text) = read(record)? else {
        return Ok(None);
    };
    let swap: Swap = serde_json::from_str(&text).map_err(Error::other)?;
    let path = &swap.path;
    let note = match read(path)? {
        Some(c) if c == swap.ours => {
            match &swap.orig {
                Some(orig) => std::fs::write(path, orig)?,
                None => {
                    std::fs::remove_file(path)?;
                    // only an empty .cursor goes
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::remove_dir(parent);
                    }
                }
            }
            None
        }
        Some(c) => match without_ours(&c, run) {
            Ok(kept) => {
                if kept != c {
                    std::fs::write(path, kept)?;
                }
                Some(format!("{} had changed: kept yours", path.display()))
            }
            Err(e) => Some(format!("{}: {e}: left as it is", path.display())),
        },
        None => None,
    };
    if swap.skip_worktree {
        git(
            &swap.cwd,
            &["update-index", "--no-skip-worktree", "--", &swap.rel],
        );
    }
    std::fs::remove_file(record)?;
    if let Some(file) = &swap.exclude
        && let Some(text) = read(file)?
    {
        let line = format!("/{}", swap.rel);
        // runs of one repo shared the line: the last record takes it out
        let shared = std::fs::read_dir(record.parent().unwrap_or(Path::new(".")))?
            .flatten()
            .filter_map(|e| read(&e.path()).ok().flatten())
            .filter_map(|t| serde_json::from_str::<Swap>(&t).ok())
            .any(|s| s.exclude.as_ref() == Some(file));
        if !shared {
            let kept: Vec<&str> = text.lines().filter(|l| *l != line).collect();
            let tail = if kept.is_empty() { "" } else { "\n" };
            std::fs::write(file, format!("{}{tail}", kept.join("\n")))?;
        }
    }
    Ok(note)
}

// Every swap recorded in `dir`, given back; the dir goes once empty.
pub fn all(dir: &Path) {
    let records: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    for record in records {
        let run = record
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match restore(&record, &run) {
            Ok(Some(note)) => eprintln!("run {run}: {note}"),
            Ok(None) => {}
            Err(e) => eprintln!("run {run}: the old cursor hooks were not given back: {e}"),
        }
    }
    let _ = std::fs::remove_dir(dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    const THEIRS: &str =
        r#"{"version":1,"hooks":{"beforeShellExecution":[{"command":"./lint.sh"}]}}"#;

    // The repo as the old swap left it, and its record, in the format swap.rs wrote.
    fn swapped(d: &Path, run: &str, orig: Option<&str>, tracked: bool) -> Result<PathBuf> {
        let repo = d.join(run);
        std::fs::create_dir_all(repo.join(".cursor"))?;
        assert!(git(&repo, &["init", "-q"]));
        let file = repo.join(".cursor/hooks.json");
        if let Some(o) = orig {
            std::fs::write(&file, o)?;
        }
        if tracked {
            assert!(git(&repo, &["add", "."]));
            assert!(git(
                &repo,
                &[
                    "update-index",
                    "--skip-worktree",
                    "--",
                    ".cursor/hooks.json"
                ]
            ));
        }
        let exclude = repo.join(".git/info/exclude");
        if !tracked {
            std::fs::write(&exclude, "# git's own\n/.cursor/hooks.json\n")?;
        }
        // the repo's entries, then ours, as swap.rs merged them
        let hook =
            format!("'/bin/drover' hook --closed '127.0.0.1:7341/hooks/{run}' 599 || exit 2");
        let mut entries: Vec<Value> = orig
            .and_then(|o| serde_json::from_str::<Value>(o).ok())
            .and_then(|v| v.pointer("/hooks/beforeShellExecution").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        entries.push(serde_json::json!({"command": hook}));
        let ours = serde_json::json!({"version": 1, "hooks": {"beforeShellExecution": entries}})
            .to_string();
        std::fs::write(&file, &ours)?;
        let record = serde_json::json!({
            "cwd": repo, "rel": ".cursor/hooks.json", "path": file, "orig": orig, "ours": ours,
            "skip_worktree": tracked, "exclude": (!tracked).then_some(&exclude),
        });
        std::fs::create_dir_all(d.join("hooks"))?;
        std::fs::write(
            d.join("hooks").join(format!("{run}.json")),
            record.to_string(),
        )?;
        Ok(repo)
    }

    fn skip_worktree(repo: &Path) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["ls-files", "-v", ".cursor/hooks.json"])
            .output()
            .is_ok_and(|o| o.stdout.starts_with(b"S "))
    }

    #[test]
    fn old_swaps_are_given_back() -> Result<()> {
        let d = std::env::temp_dir().join(format!("drover-unswap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let tracked = swapped(&d, "r-00000a", Some(THEIRS), true)?;
        assert!(skip_worktree(&tracked));
        let alone = swapped(&d, "r-00000b", None, false)?;
        let edited = swapped(&d, "r-00000c", Some(THEIRS), false)?;
        let mut v: Value =
            serde_json::from_str(&std::fs::read_to_string(edited.join(".cursor/hooks.json"))?)
                .map_err(Error::other)?;
        if let Some(h) = v.pointer_mut("/hooks").and_then(Value::as_object_mut) {
            h.insert(
                "afterFileEdit".into(),
                serde_json::json!([{"command": "./fmt.sh"}]),
            );
        }
        std::fs::write(edited.join(".cursor/hooks.json"), v.to_string())?;

        all(&d.join("hooks"));
        assert_eq!(
            std::fs::read_to_string(tracked.join(".cursor/hooks.json"))?,
            THEIRS
        );
        assert!(!skip_worktree(&tracked));
        assert!(!alone.join(".cursor").exists());
        assert_eq!(
            std::fs::read_to_string(alone.join(".git/info/exclude"))?,
            "# git's own\n"
        );
        let kept = std::fs::read_to_string(edited.join(".cursor/hooks.json"))?;
        assert!(
            kept.contains("./fmt.sh")
                && kept.contains("./lint.sh")
                && !kept.contains("hooks/r-00000c")
        );
        assert_eq!(
            std::fs::read_to_string(edited.join(".git/info/exclude"))?,
            "# git's own\n"
        );
        assert!(!d.join("hooks").exists());
        let _ = std::fs::remove_dir_all(&d);
        Ok(())
    }
}
