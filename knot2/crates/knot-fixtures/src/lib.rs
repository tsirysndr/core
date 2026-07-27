use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub const AUTHOR_NAME: &str = "nel";
pub const AUTHOR_EMAIL: &str = "nel@oyster.cafe";
pub const PINNED_DATE: &str = "2026-01-01T00:00:00 +0000";

pub fn command(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "true")
        .env("GIT_AUTHOR_NAME", AUTHOR_NAME)
        .env("GIT_AUTHOR_EMAIL", AUTHOR_EMAIL)
        .env("GIT_COMMITTER_NAME", AUTHOR_NAME)
        .env("GIT_COMMITTER_EMAIL", AUTHOR_EMAIL);
    command
}

pub fn command_at(cwd: &Path, stamp: &str) -> Command {
    let mut command = command(cwd);
    command
        .env("GIT_AUTHOR_DATE", stamp)
        .env("GIT_COMMITTER_DATE", stamp);
    command
}

pub fn available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

pub fn run(cwd: &Path, args: &[&str]) -> (bool, String) {
    let out = command_at(cwd, PINNED_DATE)
        .args(args)
        .output()
        .expect("git is available");
    (out.status.success(), combined(&out))
}

pub fn must(cwd: &Path, args: &[&str]) -> String {
    let out = command_at(cwd, PINNED_DATE)
        .args(args)
        .output()
        .expect("git is available");
    assert!(out.status.success(), "git {args:?}:\n{}", combined(&out));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn feed(cwd: &Path, args: &[&str], stdin: &[u8]) -> (bool, String) {
    let mut child = command_at(cwd, PINNED_DATE)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git is available");
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(stdin)
        .expect("the write to git's stdin succeeds");
    let out = child.wait_with_output().expect("git exits");
    (out.status.success(), combined(&out))
}

pub fn fsck(bare: &Path) -> Result<(), String> {
    match run(
        bare,
        &["fsck", "--no-dangling", "--no-reflogs", "--no-progress"],
    ) {
        (true, _) => Ok(()),
        (false, report) => Err(report),
    }
}

pub fn commit(work: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(work.join(file), contents).expect("fixture file is writable");
    must(work, &["add", "-A"]);
    must(work, &["commit", "-q", "-m", message]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_helper_pins_its_dates_so_the_same_sequence_yields_the_same_oid() {
        if !available() {
            return;
        }
        let oids = || {
            let dir = tempfile::tempdir().unwrap();
            must(dir.path(), &["init", "-q", "-b", "main"]);
            commit(dir.path(), "README.md", "kelp\n", "initial");
            let tree = must(dir.path(), &["hash-object", "-t", "tree", "-w", "--stdin"]);
            let (ok, from_stdin) = feed(dir.path(), &["commit-tree", &tree, "-F", "-"], b"empty\n");
            assert!(ok, "{from_stdin}");
            (
                must(dir.path(), &["rev-parse", "HEAD"]),
                from_stdin.trim().to_string(),
            )
        };
        assert_eq!(
            oids(),
            oids(),
            "an unpinned committer date would make every differential run disagree, \
             so a fixture writing through stdin pins the same dates as one that doesn't"
        );
    }
}
