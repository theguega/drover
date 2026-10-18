use std::process::Command;

fn git(args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn main() {
    println!(
        "cargo:rustc-env=DROVER_GIT_SHA={}",
        git(&["rev-parse", "--short", "HEAD"])
    );
    // a worktree's .git is a file, so ask git where HEAD is. On a branch HEAD only names the
    // ref, which is the file a commit rewrites.
    let head = git(&["rev-parse", "--git-path", "HEAD"]);
    if !head.is_empty() {
        println!("cargo:rerun-if-changed={head}");
    }
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    if !branch.is_empty() {
        println!(
            "cargo:rerun-if-changed={}",
            git(&["rev-parse", "--git-path", &branch])
        );
    }
    println!("cargo:rerun-if-changed=build.rs");
}
