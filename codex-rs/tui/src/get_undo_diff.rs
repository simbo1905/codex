//! Compute a diff between the current working tree and a ghost-snapshot commit.
//!
//! A ghost snapshot is a detached git commit created by the undo feature at
//! the start of each turn (see `codex-git-utils`). Given a commit SHA we can
//! ask git to produce a coloured diff of what has changed since that snapshot
//! was taken, giving the user a precise picture of every file the agent
//! touched in that turn.

use std::io;
use std::process::Stdio;
use tokio::process::Command;

/// Diff the current working tree against the ghost-snapshot commit identified
/// by `sha`.
///
/// Returns `(true, diff_text)` when inside a git repo, or `(false, "")` when
/// not.  The diff text uses ANSI colour codes suitable for display in the
/// pager overlay.
pub(crate) async fn get_undo_diff(sha: &str) -> io::Result<(bool, String)> {
    // Confirm we are inside a git repo first.
    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match inside {
        Ok(s) if s.success() => {}
        Ok(_) => return Ok((false, String::new())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((false, String::new())),
        Err(e) => return Err(e),
    }

    // `git diff --color <sha>` compares the snapshot tree to the working tree,
    // including staged changes.  Exit status 1 means differences found (normal).
    let output = Command::new("git")
        .args(["diff", "--color", sha])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    if output.status.success() || output.status.code() == Some(1) {
        Ok((true, String::from_utf8_lossy(&output.stdout).into_owned()))
    } else {
        Err(io::Error::other(format!(
            "git diff --color {sha} failed with status {}",
            output.status
        )))
    }
}
