//! `serve --supervise`: the stdio proxy that swaps its worker on a rebuild.
//!
//! An MCP client owns the process it spawns, and reads the tool list once at
//! connect, so a newly built binary normally means reconnecting by hand. This
//! module keeps one stable process on the client's pipes and moves the half that
//! changes into a child. When the installed binary changes on disk, the child is
//! stopped, respawned from the same path, replayed the initialize exchange, and
//! announced with `notifications/tools/list_changed`, so the tools an agent sees
//! are the ones just built.
//!
//! The proxy parses two fields of each message and forwards the bytes otherwise
//! untouched: it routes, it does not interpret. A worker that dies mid-request
//! is answered on its behalf, so a client waits for nothing that will never come.
//!
//! A swap starts the replacement before retiring the incumbent, and keeps the
//! incumbent if the replacement cannot complete a handshake. An agent editing
//! constellation installs a broken build sooner or later, and when it does the
//! session must not notice. Only a worker that dies with no replacement to take
//! over degrades the session, and even then the next build recovers it.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::args::positional;

/// The interval at which the installed binary is stat-ed for a change.
const POLL_INTERVAL: Duration = Duration::from_millis(400);

/// The consecutive polls that must agree on a new stamp before it counts as a
/// finished write. An installer copies bytes over seconds, not atomically, so a
/// swap on the first differing stat would launch a half-written binary.
const SETTLE_POLLS_MIN: u32 = 2;

/// The time a worker gets to exit after its stdin closes, before it is killed.
const STOP_GRACE: Duration = Duration::from_millis(600);

/// The interval at which a stopping worker is checked for exit within [`STOP_GRACE`].
const STOP_POLL: Duration = Duration::from_millis(20);

/// The time a replayed initialize may take before the worker is called broken.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// The consecutive failed starts before the supervisor stops retrying and waits for
/// the next binary change instead. Applies only when nothing is serving: a swap
/// with a healthy incumbent gets one attempt, because failing it costs nothing.
const RESTART_ATTEMPTS_MAX: u32 = 3;

/// The pause between start attempts, so a crash loop cannot spin.
const RESTART_BACKOFF: Duration = Duration::from_millis(300);

/// The client messages held while a swap is in flight. Beyond this the client is
/// answered with an error rather than buffered without bound.
const QUEUE_MESSAGES_MAX: usize = 1024;

/// The in-flight request ids tracked for the answer-on-death guarantee. Beyond this
/// the oldest are forgotten: a bounded lie about ancient requests, never a
/// bounded memory leak.
const PENDING_REQUESTS_MAX: usize = 4096;

/// The id the supervisor's own initialize carries, so the worker's reply to it
/// is consumed here instead of reaching a client that already initialized once.
const SUPERVISOR_INITIALIZE_ID: &str = "constellation-supervisor/initialize";

/// The JSON-RPC error code the proxy answers with. Inside the implementation
/// defined range, so it cannot collide with a protocol-level code.
const PROXY_ERROR_CODE: i32 = -32_000;

/// The message a client is told when nothing is serving. Names the cure, because the
/// cure is always the next build.
const WORKER_UNAVAILABLE: &str = "constellation worker is unavailable; rebuild to recover";

/// The flag that marks the child half, kept stable forever: an old supervisor
/// spawning a new worker passes it, so renaming it would strand live sessions.
pub(crate) const WORKER_FLAG: &str = "--worker";

/// The flag that selects this mode.
pub(crate) const SUPERVISE_FLAG: &str = "--supervise";

/// A JSON-RPC message, read for only the two fields the proxy routes on.
/// Every other field is skipped without being materialized, so forwarding a
/// megabyte of tool output does not cost a parsed copy of it.
#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
}

/// The initialize exchange, replayed into every worker after the first.
#[derive(Clone, Default)]
struct Handshake {
    initialize: Option<serde_json::Value>,
    initialized: Option<String>,
}

/// The destination a client message has right now.
enum Link {
    /// A worker is running and reading.
    Up(ChildStdin),
    /// A swap is in flight; messages queue until it finishes.
    Restarting,
    /// The state where no worker could be started; requests are answered with an error.
    Degraded,
}

/// The events the control loop reacts to.
enum Event {
    BinaryChanged,
    WorkerExited(u64),
    ClientClosed,
}

/// The state the threads share. One mutex covers the link and its queue together,
/// so a message can never be written to a worker that is being torn down, nor
/// queued after the queue was drained. No thread holds two of these at once.
struct Shared {
    link: Mutex<(Link, Vec<String>)>,
    handshake: Mutex<Handshake>,
    pending: Mutex<Vec<Request>>,
}

/// A request a worker has been given and not yet answered, kept whole so a
/// replacement can be handed it rather than the client being told it failed.
struct Request {
    id: String,
    line: String,
}

impl Shared {
    fn new() -> Self {
        Self {
            link: Mutex::new((Link::Restarting, Vec::new())),
            handshake: Mutex::new(Handshake::default()),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// The record of a request the worker owes an answer to.
    fn remember(&self, id: String, line: String) {
        assert!(!id.is_empty(), "an outstanding request is keyed by a non-empty id");

        let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());

        if pending.len() >= PENDING_REQUESTS_MAX {
            pending.remove(0);
        }

        pending.push(Request { id, line });

        assert!(
            pending.len() <= PENDING_REQUESTS_MAX,
            "the pending list forgets its oldest rather than growing past {PENDING_REQUESTS_MAX}",
        );
    }

    /// The record of a request the worker has answered, forgotten.
    fn settle(&self, id: &str) {
        assert!(!id.is_empty(), "an answered request is keyed by a non-empty id");

        let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());

        if let Some(position) = pending.iter().position(|known| known.id == id) {
            pending.remove(position);
        }

        debug_assert!(
            !pending.iter().any(|known| known.id == id),
            "one id is outstanding at most once, so settling it leaves none behind",
        );
    }

    /// The unanswered requests taken whole, to hand to a replacement worker or,
    /// when there is no replacement, to answer on the dead worker's behalf.
    fn drain_pending(&self) -> Vec<Request> {
        let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());

        let taken = std::mem::take(&mut *pending);

        assert!(pending.is_empty(), "a drain leaves nothing outstanding");
        assert!(taken.len() <= PENDING_REQUESTS_MAX, "a drain yields at most the bounded list");

        taken
    }
}

/// The supervisor: `constellation serve --supervise [db]`. Runs until the client
/// closes its side, keeping a worker alive underneath and replacing it whenever
/// the binary it was started from changes.
pub(crate) fn supervise_command(rest: &[String]) -> Result<()> {
    let executable = std::env::current_exe()
        .context("the supervisor cannot locate its own executable to respawn")?;

    let shared = Arc::new(Shared::new());
    let (events, inbox) = channel::<Event>();
    let (replies, handshakes) = channel::<String>();

    thread::Builder::new()
        .name("constellation-client".to_string())
        .spawn({
            let shared = Arc::clone(&shared);
            let events = events.clone();

            move || client_loop(&shared, &events)
        })
        .context("the client reader thread failed to start")?;

    thread::Builder::new()
        .name("constellation-watch".to_string())
        .spawn({
            let executable = executable.clone();
            let events = events.clone();

            move || watch_binary(&executable, &events)
        })
        .context("the binary watcher thread failed to start")?;

    let mut supervisor = Supervisor {
        active: None,
        arguments: worker_arguments(rest),
        events,
        executable,
        generations: 0,
        handshakes,
        replies,
        shared,
    };

    supervisor.run(&inbox)
}

/// The arguments a worker is started with: the database this supervisor was
/// given, if any, and the flag that keeps the child from supervising in turn.
fn worker_arguments(rest: &[String]) -> Vec<String> {
    let mut arguments = vec!["serve".to_string(), WORKER_FLAG.to_string()];

    if let Some(database) = positional(rest) {
        arguments.push(database.clone());
    }

    arguments
}

/// A worker process, the generation that identifies it (so an exit reported by
/// a worker that has already been replaced is recognized as stale), and the
/// thread draining its output.
struct Worker {
    child: Child,
    generation: u64,
    reader: Option<thread::JoinHandle<()>>,
}

/// The control half: owns the child processes and is the only thing that starts
/// or stops one, so two events can never restart the worker concurrently.
struct Supervisor {
    active: Option<Worker>,
    arguments: Vec<String>,
    events: Sender<Event>,
    executable: PathBuf,
    generations: u64,
    handshakes: Receiver<String>,
    replies: Sender<String>,
    shared: Arc<Shared>,
}

impl Supervisor {
    /// The first worker started, then events served until the client disconnects.
    fn run(&mut self, inbox: &Receiver<Event>) -> Result<()> {
        self.reload();

        for event in inbox {
            match event {
                Event::BinaryChanged => {
                    eprintln!("constellation supervisor: reloading, the binary changed");

                    self.reload();
                }
                Event::WorkerExited(generation) if self.is_active(generation) => {
                    eprintln!("constellation supervisor: reloading, the worker exited");

                    self.retire();
                    self.reload();
                }
                Event::WorkerExited(_) => {}
                Event::ClientClosed => break,
            }
        }

        self.retire();

        Ok(())
    }

    /// Whether a reported exit belongs to the worker currently serving.
    fn is_active(&self, generation: u64) -> bool {
        assert!(generation > 0, "a worker generation is assigned from one, never zero");
        assert!(generation <= self.generations, "no worker outruns the generation counter");

        self.active.as_ref().is_some_and(|worker| worker.generation == generation)
    }

    /// A worker brought in from the installed binary. The candidate is fully
    /// started and handshaken before the incumbent is retired, so a build that
    /// cannot run costs the session nothing: the incumbent keeps serving and the
    /// next change tries again. Only with nothing serving does this retry, and
    /// only then can it end degraded.
    fn reload(&mut self) {
        let serving = self.active.is_some();
        let attempts_max = if serving { 1 } else { RESTART_ATTEMPTS_MAX };
        let mut attempt: u32 = 0;

        assert!(attempts_max > 0, "a reload always makes at least one attempt");

        while attempt < attempts_max {
            attempt += 1;

            match self.start_candidate() {
                Ok(candidate) => {
                    self.promote(candidate);

                    return;
                }
                Err(error) => {
                    eprintln!("constellation supervisor: worker start failed: {error:#}");

                    if attempt < attempts_max {
                        thread::sleep(RESTART_BACKOFF);
                    }
                }
            }
        }

        if serving {
            eprintln!("constellation supervisor: keeping the running worker; rebuild to retry");

            return;
        }

        self.degrade();
    }

    /// A spawned worker handed the recorded handshake, without touching the link.
    /// The returned candidate is ready to serve; a failure leaves nothing behind
    /// but a killed process.
    fn start_candidate(&mut self) -> Result<Candidate> {
        assert!(
            self.arguments.iter().any(|argument| argument == WORKER_FLAG),
            "a spawned child always carries {WORKER_FLAG}, so a supervisor never supervises itself",
        );
        assert!(self.generations < u64::MAX, "the generation counter must not wrap");

        let mut child = spawn_worker(&self.executable, &self.arguments)?;

        let stdin = child.stdin.take().context("the worker exposes no stdin");
        let stdout = child.stdout.take().context("the worker exposes no stdout");

        self.generations += 1;

        let generation = self.generations;
        let mut worker = Worker { child, generation, reader: None };

        let (Ok(stdin), Ok(stdout)) = (stdin, stdout) else {
            stop_worker(&mut worker);

            anyhow::bail!("the worker exposes no pipes to proxy");
        };

        let shared = Arc::clone(&self.shared);
        let events = self.events.clone();
        let replies = self.replies.clone();

        let reader = thread::Builder::new()
            .name(format!("constellation-worker-{generation}"))
            .spawn(move || worker_loop(stdout, &shared, generation, &events, &replies));

        match reader {
            Ok(handle) => worker.reader = Some(handle),
            Err(_) => {
                stop_worker(&mut worker);

                anyhow::bail!("the worker reader thread failed to start");
            }
        }

        match self.replay(stdin) {
            Ok((stdin, replayed)) => Ok(Candidate { replayed, stdin, worker }),
            Err(error) => {
                stop_worker(&mut worker);

                Err(error)
            }
        }
    }

    /// The incumbent retired and the candidate put on the link, handed every
    /// request the incumbent never answered and then whatever queued while the
    /// swap ran, in that order, because that is the order the client sent them.
    /// A caller whose worker died mid-call waits longer; it is not told its call
    /// failed.
    fn promote(&mut self, candidate: Candidate) {
        let Candidate { replayed, stdin, worker } = candidate;
        let previous = self.active.as_ref().map(|worker| worker.generation);

        assert!(
            worker.generation == self.generations,
            "a promoted worker carries the newest generation, never a stale candidate",
        );
        assert!(
            previous.is_none_or(|generation| generation < worker.generation),
            "a promoted generation is strictly newer than the one it replaces",
        );

        // Only a worker that replaces one announces itself. The first worker of
        // a session changes no tool list the client has read yet, and a client
        // that has not finished initializing is owed no notifications at all.
        let replaced = previous.is_some();

        self.retire();

        let unanswered = self.shared.drain_pending();

        self.active = Some(worker);

        // Delivering re-records them, so a request handed on stays outstanding
        // and a second death would replay it again rather than lose its caller.
        self.open(stdin, &unanswered);

        if replaced && replayed {
            emit_line(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#);
        }
    }

    /// The recorded initialize exchange, if the client has completed one.
    fn handshake(&self) -> Handshake {
        let handshake = self.shared.handshake.lock().unwrap_or_else(|error| error.into_inner());

        handshake.clone()
    }

    /// The handshake replayed into a fresh worker. Returns the worker's stdin and
    /// whether anything was replayed, which is false for the first worker, whose
    /// handshake the client itself is about to perform.
    fn replay(&mut self, mut stdin: ChildStdin) -> Result<(ChildStdin, bool)> {
        let handshake = self.handshake();

        let Some(initialize) = handshake.initialize else {
            return Ok((stdin, false));
        };

        while self.handshakes.try_recv().is_ok() {}

        let request = supervisor_initialize(initialize);

        stdin.write_all(request.as_bytes()).context("replaying initialize failed")?;
        stdin.flush().context("replaying initialize failed")?;

        match self.handshakes.recv_timeout(HANDSHAKE_TIMEOUT) {
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {
                anyhow::bail!("the worker never answered initialize")
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("the worker closed during startup")
            }
        }

        if let Some(initialized) = &handshake.initialized {
            stdin.write_all(initialized.as_bytes()).context("replaying initialized failed")?;
            stdin.flush().context("replaying initialized failed")?;
        }

        Ok((stdin, true))
    }

    /// The link opened on a ready worker: first the requests the previous worker
    /// left unanswered, then everything queued during the swap, in the order the
    /// client sent them.
    fn open(&self, stdin: ChildStdin, unanswered: &[Request]) {
        assert!(self.active.is_some(), "the link opens only onto a worker the supervisor owns");
        assert!(
            unanswered.len() <= PENDING_REQUESTS_MAX,
            "the handed-on requests come from the bounded pending list",
        );

        let mut link = self.shared.link.lock().unwrap_or_else(|error| error.into_inner());
        let (state, queue) = &mut *link;

        *state = Link::Up(stdin);

        let queued = std::mem::take(queue);

        assert!(queued.len() <= QUEUE_MESSAGES_MAX, "the swap queue never grew past its bound");

        let Link::Up(worker) = state else {
            return;
        };

        for request in unanswered {
            if deliver(worker, &request.line, &self.shared).is_err() {
                return;
            }
        }

        for message in queued {
            if deliver(worker, &message, &self.shared).is_err() {
                return;
            }
        }
    }

    /// The link closed and the worker currently serving, if any, stopped.
    fn retire(&mut self) {
        let retiring = self.active.as_ref().map(|worker| worker.generation);

        {
            let mut link = self.shared.link.lock().unwrap_or_else(|error| error.into_inner());

            link.0 = Link::Restarting;
        }

        if let Some(worker) = &mut self.active {
            stop_worker(worker);
        }

        self.active = None;

        assert!(
            retiring.is_none_or(|generation| !self.is_active(generation)),
            "a retired worker is no longer the active generation, so its exit reads as stale",
        );
    }

    /// The link marked unusable, so requests are answered instead of queued for a
    /// worker that is not coming back until the binary changes again.
    fn degrade(&mut self) {
        assert!(self.active.is_none(), "the link degrades only with no worker left serving");

        self.fail_pending(WORKER_UNAVAILABLE);

        let mut link = self.shared.link.lock().unwrap_or_else(|error| error.into_inner());
        let (state, queue) = &mut *link;

        *state = Link::Degraded;

        let queued = std::mem::take(queue);

        assert!(queued.len() <= QUEUE_MESSAGES_MAX, "the swap queue never grew past its bound");

        drop(link);

        for message in queued {
            if let Some(answer) = local_answer(&message, WORKER_UNAVAILABLE) {
                emit_line(&answer);
            }
        }
    }

    /// The answer to every request no worker will ever answer, so no call hangs
    /// forever. Reached only when the session has degraded; a swap hands them on
    /// instead.
    fn fail_pending(&self, reason: &str) {
        assert!(!reason.is_empty(), "a failed request is answered with a stated reason");

        for request in self.shared.drain_pending() {
            emit_line(&error_response(&request.id, reason));
        }
    }
}

/// A started, handshaken worker that has not taken over the link yet.
struct Candidate {
    replayed: bool,
    stdin: ChildStdin,
    worker: Worker,
}

/// The worker stopped: its stdin is already dropped or about to be, so a healthy
/// worker exits on its own; one that will not is killed after the grace period.
///
/// The reader thread is joined before returning, because a worker that answers
/// during its grace period has written bytes its reader has not yet turned into
/// a settled request. Draining the pending list before that thread finishes
/// would replay a request the client has already been answered, and two
/// responses to one id is a protocol violation, not a slow retry.
fn stop_worker(worker: &mut Worker) {
    let deadline = Instant::now() + STOP_GRACE;
    let mut exited = false;

    while Instant::now() < deadline {
        match worker.child.try_wait() {
            Ok(Some(_)) => {
                exited = true;

                break;
            }
            Ok(None) => thread::sleep(STOP_POLL),
            Err(_) => break,
        }
    }

    if !exited {
        let _ = worker.child.kill();
        let _ = worker.child.wait();
    }

    if let Some(reader) = worker.reader.take() {
        let _ = reader.join();
    }

    assert!(
        worker.reader.is_none(),
        "a stopped worker's reader is joined, so a drain cannot race a request it is settling",
    );
}

/// The client half: every line from the client's stdin, recorded if it is part
/// of the handshake or a request, then handed to the worker.
fn client_loop(shared: &Arc<Shared>, events: &Sender<Event>) {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();

    loop {
        line.clear();

        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        if line.trim().is_empty() {
            continue;
        }

        record_handshake(shared, &line);
        forward_to_worker(shared, &line);
    }

    let _ = events.send(Event::ClientClosed);
}

/// The handshake remembered, to be replayed into every worker after the first.
///
/// A request is deliberately not recorded here: it becomes outstanding when a
/// worker is actually handed it, in [`deliver`]. Recording it on arrival made a
/// message that was still queued also look outstanding, and the first swap then
/// delivered it twice, once from the queue and once from the replay.
fn record_handshake(shared: &Arc<Shared>, line: &str) {
    let Ok(envelope) = serde_json::from_str::<Envelope>(line) else {
        return;
    };

    match envelope.method.as_deref() {
        Some("initialize") => {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                let mut handshake =
                    shared.handshake.lock().unwrap_or_else(|error| error.into_inner());

                handshake.initialize = Some(value);
            }
        }
        Some("notifications/initialized") => {
            let mut handshake = shared.handshake.lock().unwrap_or_else(|error| error.into_inner());

            handshake.initialized = Some(line.to_string());
        }
        _ => {}
    }
}

/// The message handed to a worker and, when it is a request, recorded as one the
/// worker now owes an answer for. The two happen together so that what is
/// outstanding is exactly what a worker has been given and has not answered.
fn deliver(worker: &mut ChildStdin, line: &str, shared: &Shared) -> std::io::Result<()> {
    write_line(worker, line)?;

    if let Some(id) = request_id(line) {
        shared.remember(id, line.to_string());
    }

    Ok(())
}

/// The id of a client request, or `None` for a notification, a response, or a
/// line that is not JSON at all.
fn request_id(line: &str) -> Option<String> {
    let envelope = serde_json::from_str::<Envelope>(line).ok()?;
    let (id, _) = (envelope.id?, envelope.method?);

    Some(id.to_string())
}

/// The routing of one client message: handed to the worker, queued while a swap
/// is in flight, or answered here when there is no worker to answer it.
fn forward_to_worker(shared: &Arc<Shared>, line: &str) {
    let mut link = shared.link.lock().unwrap_or_else(|error| error.into_inner());
    let (state, queue) = &mut *link;

    match state {
        Link::Up(worker) => {
            if deliver(worker, line, shared).is_err() {
                *state = Link::Restarting;
                queue.push(line.to_string());
            }
        }
        Link::Restarting if queue.len() < QUEUE_MESSAGES_MAX => queue.push(line.to_string()),
        Link::Restarting | Link::Degraded => {
            drop(link);

            if let Some(answer) = local_answer(line, WORKER_UNAVAILABLE) {
                emit_line(&answer);
            }
        }
    }
}

/// The worker half: every line the worker writes, minus the replies to the
/// supervisor's own initialize, which never belong to the client.
fn worker_loop(
    stdout: ChildStdout,
    shared: &Arc<Shared>,
    generation: u64,
    events: &Sender<Event>,
    replies: &Sender<String>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    loop {
        line.clear();

        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        if line.trim().is_empty() {
            continue;
        }

        if let Ok(envelope) = serde_json::from_str::<Envelope>(&line) {
            let id = envelope.id.as_ref().map(std::string::ToString::to_string);

            if id.as_deref() == Some(supervisor_id_literal().as_str()) {
                let _ = replies.send(line.clone());

                continue;
            }

            if let (Some(id), None) = (id, envelope.method) {
                shared.settle(&id);
            }
        }

        emit_line(line.trim_end_matches('\n'));
    }

    let _ = events.send(Event::WorkerExited(generation));
}

/// The installed binary's stamp, polled until it changes and settles, which is
/// the signal that a rebuild has landed and the worker is stale.
fn watch_binary(path: &Path, events: &Sender<Event>) {
    let mut baseline = stamp(path);
    let mut candidate: Option<(u64, u128)> = None;
    let mut agreed: u32 = 0;

    loop {
        assert!(agreed < SETTLE_POLLS_MIN, "a settled stamp is consumed and its counter reset");

        thread::sleep(POLL_INTERVAL);

        let current = stamp(path);

        if current.is_none() || current == baseline {
            candidate = None;
            agreed = 0;

            continue;
        }

        if current == candidate {
            agreed += 1;
        } else {
            candidate = current;
            agreed = 1;
        }

        if agreed >= SETTLE_POLLS_MIN {
            baseline = current;
            candidate = None;
            agreed = 0;

            if events.send(Event::BinaryChanged).is_err() {
                return;
            }
        }
    }
}

/// A file's identity for change detection: its length and modification time.
/// `None` while the file is absent, which is what an installer's rename-then-copy
/// looks like for an instant and must not be read as a change.
fn stamp(path: &Path) -> Option<(u64, u128)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let since_epoch = modified.duration_since(UNIX_EPOCH).ok()?;

    Some((metadata.len(), since_epoch.as_nanos()))
}

/// The worker process, wired for proxying: pipes on stdin and stdout, and the
/// inherited stderr the client already shows its user.
fn spawn_worker(executable: &Path, arguments: &[String]) -> Result<Child> {
    Command::new(executable)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning the worker {} failed", executable.display()))
}

/// The supervisor's own initialize: the client's request with its id replaced, so
/// the worker's answer is recognizable here and is never mistaken for the
/// client's own.
fn supervisor_initialize(mut initialize: serde_json::Value) -> String {
    if let Some(object) = initialize.as_object_mut() {
        object.insert(
            "id".to_string(),
            serde_json::Value::String(SUPERVISOR_INITIALIZE_ID.to_string()),
        );
    }

    format!("{initialize}\n")
}

/// The supervisor's initialize id as it appears once serialized, quotes included,
/// which is the form the worker echoes back.
fn supervisor_id_literal() -> String {
    format!("\"{SUPERVISOR_INITIALIZE_ID}\"")
}

/// The answer the proxy owes a client message when no worker can take it, or
/// `None` for a message nothing is waiting on: a notification carries no id, and
/// a line that is not a request is not the proxy's to answer.
fn local_answer(line: &str, reason: &str) -> Option<String> {
    let id = request_id(line)?;

    Some(error_response(&id, reason))
}

/// A JSON-RPC error response, with the id inlined as the client wrote it, so a
/// string id stays a string and a numeric one stays numeric.
fn error_response(id: &str, message: &str) -> String {
    assert!(!id.is_empty(), "an error response carries the id the client wrote");
    assert!(!message.is_empty(), "an error response states a reason");

    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    let error = format!(r#"{{"code":{PROXY_ERROR_CODE},"message":"{escaped}"}}"#);

    format!(r#"{{"jsonrpc":"2.0","id":{id},"error":{error}}}"#)
}

/// The message written to the worker, newline-terminated as the transport requires.
fn write_line(worker: &mut ChildStdin, line: &str) -> std::io::Result<()> {
    worker.write_all(line.trim_end_matches('\n').as_bytes())?;
    worker.write_all(b"\n")?;

    worker.flush()
}

/// The message written to the client. Taking the stdout lock for the whole line
/// keeps two threads from interleaving halves of two messages.
fn emit_line(line: &str) {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::{
        Envelope, SUPERVISOR_INITIALIZE_ID, WORKER_FLAG, WORKER_UNAVAILABLE, error_response,
        local_answer, supervisor_id_literal, supervisor_initialize, worker_arguments,
    };

    #[test]
    fn a_worker_inherits_the_database_and_never_supervises() {
        let arguments = worker_arguments(&["--supervise".to_string(), "db.sqlite".to_string()]);

        assert_eq!(arguments, vec!["serve", WORKER_FLAG, "db.sqlite"]);

        let bare = worker_arguments(&["--supervise".to_string()]);

        assert_eq!(bare, vec!["serve", WORKER_FLAG]);
    }

    #[test]
    fn a_replayed_initialize_carries_the_supervisor_id() {
        let original = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": "2024-11-05"},
        });

        let replayed = supervisor_initialize(original);
        let parsed: serde_json::Value = serde_json::from_str(replayed.trim()).unwrap();

        assert_eq!(parsed["id"], SUPERVISOR_INITIALIZE_ID);
        assert_eq!(parsed["params"]["protocolVersion"], "2024-11-05");
        assert!(replayed.ends_with('\n'), "the transport is newline delimited");
    }

    #[test]
    fn a_worker_reply_to_the_supervisor_is_recognized_by_its_id() {
        let reply = serde_json::json!({
            "jsonrpc": "2.0",
            "id": SUPERVISOR_INITIALIZE_ID,
            "result": {},
        })
        .to_string();

        let envelope: Envelope = serde_json::from_str(&reply).unwrap();
        let id = envelope.id.map(|id| id.to_string()).unwrap();

        assert_eq!(id, supervisor_id_literal());
    }

    #[test]
    fn an_error_response_preserves_the_id_type_and_escapes_the_reason() {
        let numeric = error_response("7", "worker restarted");
        let parsed: serde_json::Value = serde_json::from_str(&numeric).unwrap();

        assert_eq!(parsed["id"], 7);
        assert_eq!(parsed["error"]["code"], -32_000);

        let quoted = error_response("\"abc\"", "he said \"no\"");
        let parsed: serde_json::Value = serde_json::from_str(&quoted).unwrap();

        assert_eq!(parsed["id"], "abc");
        assert_eq!(parsed["error"]["message"], "he said \"no\"");
    }

    #[test]
    fn a_notification_carries_no_id_and_is_never_answered() {
        let notification = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let envelope: Envelope = serde_json::from_str(notification).unwrap();

        assert!(envelope.id.is_none(), "a notification has no id to answer");
        assert_eq!(envelope.method.as_deref(), Some("notifications/initialized"));
    }

    #[test]
    fn only_a_request_is_answered_when_no_worker_can_take_it() {
        let request = r#"{"jsonrpc":"2.0","id":4,"method":"tools/list","params":{}}"#;
        let answer = local_answer(request, WORKER_UNAVAILABLE).expect("a request is answered");
        let parsed: serde_json::Value = serde_json::from_str(&answer).unwrap();

        assert_eq!(parsed["id"], 4);
        assert_eq!(parsed["error"]["message"], WORKER_UNAVAILABLE);

        let notification = r#"{"jsonrpc":"2.0","method":"notifications/cancelled"}"#;

        assert!(
            local_answer(notification, WORKER_UNAVAILABLE).is_none(),
            "nothing waits on a notification",
        );

        assert!(
            local_answer("not json at all", WORKER_UNAVAILABLE).is_none(),
            "an unreadable line is forwarded verbatim, never answered",
        );
    }
}
