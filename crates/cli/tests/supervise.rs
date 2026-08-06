//! The supervisor's contract, exercised against the real binary: a rebuild is
//! picked up without the client reconnecting, and a binary that cannot start
//! leaves the session alive to be fixed by the next build.
//!
//! Each test runs a *copy* of the binary out of a temporary directory, because
//! the swap under test is a file replacement at the path the supervisor spawns
//! from, and the workspace's own artifact must not be touched to prove it.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

/// The time a reply may take. Generous: a cold worker builds a tokio runtime and
/// opens a store, and CI machines are slower than a developer's.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// The time a swap may take from the moment the binary is replaced: two settle
/// polls, a worker stop, a spawn, and a replayed handshake.
const SWAP_TIMEOUT: Duration = Duration::from_secs(45);

/// The time a session is watched for damage after a broken build is installed.
/// Comfortably longer than the poll, the settle, and the failed start attempt
/// that follows it, so the window under observation is the one that matters.
const BROKEN_OBSERVE: Duration = Duration::from_secs(8);

/// A client of the supervisor, speaking the newline-delimited JSON-RPC the stdio
/// transport uses, with every read bounded so a hung server fails the test
/// instead of hanging it.
struct Client {
    child: Child,
    messages: Receiver<String>,
    seen: Vec<serde_json::Value>,
    stdin: ChildStdin,
}

impl Client {
    /// A supervisor spawned from `executable`, rooted in `directory`, with the
    /// database discovery environment cleared so the worker serves its
    /// no-index surface and the test needs no fixture project.
    fn start(executable: &Path, directory: &Path) -> Self {
        let mut child = Command::new(executable)
            .arg("serve")
            .arg("--supervise")
            .current_dir(directory)
            .env_remove("CONSTELLATION_DB")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the supervisor starts");

        let stdin = child.stdin.take().expect("the supervisor exposes stdin");
        let stdout = child.stdout.take().expect("the supervisor exposes stdout");
        let stderr = child.stderr.take().expect("the supervisor exposes stderr");

        // The supervisor explains a refused swap on stderr; the harness captures
        // it so a failing assertion is reported next to the reason for it.
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                println!("supervisor: {line}");
            }
        });

        let (sender, messages) = channel();

        thread::spawn(move || {
            let reader = BufReader::new(stdout);

            for line in reader.lines() {
                let Ok(line) = line else {
                    return;
                };

                if sender.send(line).is_err() {
                    return;
                }
            }
        });

        Self { child, messages, seen: Vec::new(), stdin }
    }

    /// The messages the session has produced, in arrival order. Kept so a test
    /// can assert about what was *not* sent as well as what was.
    fn transcript(&mut self) -> &[serde_json::Value] {
        while let Ok(line) = self.messages.try_recv() {
            if let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) {
                self.seen.push(message);
            }
        }

        &self.seen
    }

    /// The number of responses the session produced for one request id. More than one
    /// is a protocol violation: a client that sent an id once is answered once.
    fn answers_for(&mut self, id: u64) -> usize {
        self.transcript()
            .iter()
            .filter(|message| message["id"] == serde_json::json!(id))
            .count()
    }

    /// The MCP handshake completed, as a client does once per session.
    fn handshake(&mut self) {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "supervise-test", "version": "1"},
            },
        }));

        let initialized = self.wait_for(|message| message["id"] == 1, REPLY_TIMEOUT);

        assert!(
            initialized["result"]["serverInfo"].is_object(),
            "initialize is answered by the worker: {initialized}",
        );

        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }));
    }

    /// The tool names the server currently advertises.
    fn tool_names(&mut self, id: u64) -> Vec<String> {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/list",
            "params": {},
        }));

        let listing = self.wait_for(|message| message["id"] == id, REPLY_TIMEOUT);

        listing["result"]["tools"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect()
    }

    /// A request, and whatever came back for it, error or result.
    fn request(&mut self, id: u64, method: &str) -> serde_json::Value {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": {},
        }));

        self.wait_for(|message| message["id"] == id, REPLY_TIMEOUT)
    }

    fn send(&mut self, message: &serde_json::Value) {
        writeln!(self.stdin, "{message}").expect("the supervisor accepts a message");
        self.stdin.flush().expect("the supervisor accepts a message");
    }

    /// The first message satisfying `wanted`, discarding the ones before it.
    fn wait_for(
        &mut self,
        wanted: impl Fn(&serde_json::Value) -> bool,
        timeout: Duration,
    ) -> serde_json::Value {
        let deadline = Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());

            assert!(!remaining.is_zero(), "timed out waiting for a message");

            match self.messages.recv_timeout(remaining) {
                Ok(line) => {
                    let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
                        continue;
                    };

                    self.seen.push(message.clone());

                    if wanted(&message) {
                        return message;
                    }
                }
                Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for a message"),
                Err(RecvTimeoutError::Disconnected) => panic!("the supervisor exited"),
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The binary under test, copied into `directory` so the test can replace it the
/// way an install does.
fn install_copy(directory: &Path) -> PathBuf {
    let source = PathBuf::from(env!("CARGO_BIN_EXE_constellation"));
    let installed = directory.join(source.file_name().expect("the artifact is a file"));

    std::fs::copy(&source, &installed).expect("the binary copies into the temp directory");

    installed
}

/// An installed binary replaced the way `cargo xtask install` does: displace the
/// running image first (Windows will not overwrite it in place), then copy. The
/// displaced name is unique per call, because the previous one is still the
/// running image and cannot be deleted or overwritten.
fn reinstall(installed: &Path, bytes: &[u8]) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("a clock after the epoch")
        .as_nanos();

    let displaced = installed.with_extension(format!("{stamp}.displaced"));

    if installed.exists() {
        std::fs::rename(installed, &displaced).expect("the running image is displaced");
    }

    std::fs::write(installed, bytes).expect("the new binary is written");

    // `write` creates a fresh file at the process's default mode, which does not
    // carry the execute bit the displaced image had. On Unix the supervisor then
    // fails to spawn the binary this just installed, with a permission error that
    // reads like a swap bug rather than a fixture one. Windows has no execute bit,
    // so nothing there needs restoring.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::Permissions::from_mode(0o755);

        std::fs::set_permissions(installed, mode).expect("the new binary is executable");
    }
}

#[test]
fn a_rebuilt_binary_swaps_the_worker_without_a_reconnect() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let installed = install_copy(directory.path());
    let bytes = std::fs::read(&installed).expect("the binary is readable");

    let mut client = Client::start(&installed, directory.path());
    client.handshake();

    let before = client.tool_names(2);

    assert!(before.contains(&"explore".to_string()), "the worker serves tools: {before:?}");

    reinstall(&installed, &bytes);

    let changed = client.wait_for(
        |message| message["method"] == "notifications/tools/list_changed",
        SWAP_TIMEOUT,
    );

    assert_eq!(changed["method"], "notifications/tools/list_changed");

    // The session is the same one: no re-initialize, and the replayed handshake
    // left the new worker able to answer.
    let after = client.tool_names(3);

    assert_eq!(before, after, "the swapped worker serves the same tool surface");

    // A swap replays what the retired worker never answered. It must not replay
    // what it did answer: two responses to one id is a protocol violation, and
    // the queue and the outstanding list once both held the same message.
    for id in [1, 2, 3] {
        assert_eq!(client.answers_for(id), 1, "id {id} is answered exactly once");
    }

    let announcements = client
        .transcript()
        .iter()
        .filter(|message| message["method"] == "notifications/tools/list_changed")
        .count();

    assert_eq!(announcements, 1, "one swap announces itself once");
}

#[test]
fn a_broken_build_never_disturbs_the_running_session() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let installed = install_copy(directory.path());
    let bytes = std::fs::read(&installed).expect("the binary is readable");

    let mut client = Client::start(&installed, directory.path());
    client.handshake();

    let before = client.tool_names(2);

    assert!(!before.is_empty(), "the worker serves tools before the break");

    // What an agent editing constellation eventually installs: something that
    // cannot run. The incumbent worker must keep serving through it.
    reinstall(&installed, b"not an executable");

    let deadline = Instant::now() + BROKEN_OBSERVE;
    let mut id = 3;

    while Instant::now() < deadline {
        let answer = client.request(id, "tools/list");

        assert!(
            answer["error"].is_null(),
            "the incumbent serves through a broken install: {answer}",
        );

        id += 1;

        thread::sleep(Duration::from_millis(500));
    }

    // And the build that fixes it swaps in, on the same session.
    reinstall(&installed, &bytes);

    client.wait_for(
        |message| message["method"] == "notifications/tools/list_changed",
        SWAP_TIMEOUT,
    );

    let after = client.tool_names(id);

    assert_eq!(before, after, "the fixed build takes over the live session");
}

#[test]
fn a_worker_that_dies_answers_its_caller_and_is_replaced() {
    let directory = tempfile::tempdir().expect("a temporary directory");
    let installed = install_copy(directory.path());

    let mut client = Client::start(&installed, directory.path());
    client.handshake();

    let before = client.tool_names(2);

    assert!(!before.is_empty(), "the worker serves tools before it dies");

    kill_workers(&installed);

    // The supervisor outlives its worker: the session keeps its handshake and
    // the next call is served by the replacement, with no reconnect.
    let after = client.tool_names(3);

    assert_eq!(before, after, "a replacement worker serves the same session");
}

/// The worker processes started from `installed`, killed the way a panic or an
/// out-of-memory kill would, without touching the supervisor that spawned them.
///
/// Scoped to this test's own copy of the binary: the tests run concurrently, and
/// killing every `--worker` on the machine would take down another test's
/// session, or a developer's live one.
#[cfg(windows)]
fn kill_workers(installed: &Path) {
    let image = installed.file_name().expect("the artifact is a file").to_string_lossy();
    let directory = installed.parent().expect("the artifact has a parent").display().to_string();

    let filter = format!(
        "Get-CimInstance Win32_Process -Filter \"Name = '{image}'\" | \
         Where-Object {{ $_.CommandLine -like '*--worker*' -and \
         $_.CommandLine -like '*{directory}*' }} | \
         ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force }}",
    );

    let killed = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &filter])
        .output();

    assert!(killed.is_ok(), "the worker processes are killable");
}

#[cfg(not(windows))]
fn kill_workers(installed: &Path) {
    let image = installed.display().to_string();

    let killed = Command::new("pkill").args(["-f", &format!("{image} serve --worker")]).output();

    assert!(killed.is_ok(), "the worker processes are killable");
}
