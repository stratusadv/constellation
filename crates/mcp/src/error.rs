//! The server's error type, and the guards that keep one bad call from
//! taking the process down.
//!
//! A panicking handler or a poisoned lock must not end the session: the agent
//! on the other end has no way to restart the server, so a failed tool call
//! returns an error and the next call still works.

use std::sync::Mutex;

use constellation_store::StoreError;
use rmcp::ErrorData;
use thiserror::Error;
use tokio::task::block_in_place;

/// The errors the MCP server can return at startup or while serving.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serve error: {0}")]
    Serve(String),
}

/// The lock guard on `mutex`, recovered if a previous holder panicked. The
/// server's state stays structurally valid across a caught panic (a rolled-back
/// SQLite transaction, an unchanged cache), so one panicking request must not
/// poison the lock for every request after it.
pub(crate) fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The error used when a handler panics, caught so a panic becomes a normal
/// error response instead of an unanswered request (a client hang) or a process
/// abort. The panic message still reaches stderr through the default hook.
#[cold]
#[inline(never)]
pub(crate) fn panic_error() -> ErrorData {
    ErrorData::internal_error(
        "constellation: internal error while handling the request (see server stderr)",
        None,
    )
}

/// A blocking action run without stalling the async runtime. On a
/// multi-threaded runtime this is `block_in_place` (the worker hands its other
/// tasks off while it blocks, so the event loop keeps serving); off a runtime,
/// or on a single-threaded one, it runs the work directly so handlers stay
/// callable outside `serve`.
pub(crate) fn run_blocking<T>(action: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};

    match Handle::try_current() {
        Ok(handle) if matches!(handle.runtime_flavor(), RuntimeFlavor::MultiThread) => {
            block_in_place(action)
        }
        _ => action(),
    }
}

/// The reply every tool returns when the server has no database, i.e. it was
/// launched (typically via a global MCP registration) outside any indexed
/// project: a clear "nothing here" message instead of a hard failure to connect.
pub(crate) const NO_INDEX_MESSAGE: &str =
    "no constellation index for this working directory (not an indexed Django project). \
     Run `constellation init` here, or open a project that has a .constellation/index.db.";
