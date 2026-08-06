//! Running git as a subprocess, bounded.
//!
//! Two bounds, both of which the server needs and neither of which
//! `Command::output()` provides.
//!
//! A wall-clock deadline, because git is not always fast and is not always
//! finite: a repository with a smudge filter, a credential helper waiting on a
//! terminal that is not there, or a network remote can leave the process
//! parked. Inside `serve` that is a worker thread that never comes back.
//!
//! An output cap, because `git diff <base>` against a long-lived branch is
//! bounded only by the size of the branch. `output()` buffers all of it into
//! the server's address space before anyone can decide it is too much.
//!
//! Neither bound is meant to fire in ordinary use. They exist so that the
//! failure, when it comes, is a tool result the agent can read rather than a
//! hung session or an allocation the size of the repository.

use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The bytes one git invocation may return before its output is cut.
///
/// Far past any diff worth reading and far below anything that threatens the
/// process. A cut result is reported rather than silently shortened, because a
/// truncated diff read as a whole one is a review that misses a file.
pub const GIT_OUTPUT_BYTES_MAX: usize = 32 * 1024 * 1024;

/// The wall-clock bound on one git invocation.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The interval at which a running child is checked against its deadline.
const GIT_POLL: Duration = Duration::from_millis(10);

/// The fail-fast bound on deadline polls, derived from the two constants above
/// so it cannot drift out of step with them.
const GIT_POLLS_MAX: u64 = (GIT_TIMEOUT.as_millis() / GIT_POLL.as_millis()) as u64 + 64;

/// The read chunk. One page-ish read per syscall over a pipe.
const GIT_CHUNK_BYTES: usize = 64 * 1024;

/// The fail-fast bound on read iterations, derived from the output cap and the
/// smallest useful read.
const GIT_READS_MAX: u64 = (GIT_OUTPUT_BYTES_MAX / GIT_CHUNK_BYTES) as u64 + 64;

// The two loop bounds above are derived from the limits they protect, so their
// relationship holds before the program runs rather than being checked once it
// is already running. A timeout or chunk size edited to something that makes a
// bound unreachable fails to compile.
const _: () = {
    assert!(GIT_POLLS_MAX > 64, "the poll bound covers the whole timeout");
    assert!(GIT_READS_MAX > 64, "the read bound covers the whole output cap");
};

/// The output of one bounded git invocation.
pub struct GitRun {
    pub stdout: String,
    /// Whether the output hit [`GIT_OUTPUT_BYTES_MAX`] and was cut. A caller
    /// that renders the result must say so rather than present a partial answer
    /// as a complete one.
    pub truncated: bool,
}

/// The output of `git -C root <arguments>`, or `None` when git is missing, the
/// path is not a repository, the command failed, or it passed [`GIT_TIMEOUT`].
///
/// Every argument is passed as an argument, never through a shell. Callers
/// taking a revision from outside must still check its shape: git reads a
/// leading dash as an option, which no amount of argument passing changes.
pub fn run_git(root: &Path, arguments: &[&str]) -> Option<GitRun> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        // No inherited stdin: a git that decides to prompt should fail rather
        // than wait for a terminal the server does not have.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;

    // Drain on its own thread. A child whose output fills the pipe buffer blocks
    // in write and never exits, so a parent that waits before reading waits
    // forever: the deadline below would fire on every large diff rather than
    // only on a genuinely stuck git.
    let reader = std::thread::spawn(move || read_capped(stdout));

    let status = wait_by(&mut child, Instant::now() + GIT_TIMEOUT);
    let run = reader.join().ok()?;

    match status {
        Some(status) if status.success() => Some(run),
        _ => None,
    }
}

/// The child's exit status, or `None` once `deadline` passes, in which case it
/// is killed and reaped so no zombie outlives the call.
fn wait_by(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    let mut polls: u64 = 0;

    loop {
        polls += 1;

        assert!(polls <= GIT_POLLS_MAX, "the deadline wait stays bounded");

        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();

            return None;
        }

        std::thread::sleep(GIT_POLL);
    }
}

/// The child's stdout read up to [`GIT_OUTPUT_BYTES_MAX`].
///
/// Returning early on the cap drops `stdout`, which closes the read end. The
/// child's next write then fails and it exits, so a cut does not leave a
/// process writing into a pipe nobody is reading.
fn read_capped(mut stdout: ChildStdout) -> GitRun {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0_u8; GIT_CHUNK_BYTES];
    let mut truncated = false;
    let mut reads: u64 = 0;

    loop {
        reads += 1;

        assert!(reads <= GIT_READS_MAX, "the capped read stays bounded");

        let Ok(read) = stdout.read(&mut chunk) else {
            break;
        };

        if read == 0 {
            break;
        }

        let room = GIT_OUTPUT_BYTES_MAX.saturating_sub(buffer.len());

        if read >= room {
            buffer.extend_from_slice(&chunk[..room]);
            truncated = true;

            break;
        }

        buffer.extend_from_slice(&chunk[..read]);
    }

    assert!(buffer.len() <= GIT_OUTPUT_BYTES_MAX, "the captured output respects its cap");

    GitRun { stdout: String::from_utf8_lossy(&buffer).into_owned(), truncated }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::run_git;

    #[test]
    fn a_path_that_is_not_a_repository_fails_rather_than_hangs() {
        let directory = tempfile::tempdir().unwrap();

        assert!(
            run_git(directory.path(), &["rev-parse", "HEAD"]).is_none(),
            "a non-repository is a failed run, not a wait",
        );
    }

    #[test]
    fn a_missing_directory_fails_rather_than_hangs() {
        assert!(run_git(Path::new("/nonexistent-constellation-root"), &["status"]).is_none());
    }
}
