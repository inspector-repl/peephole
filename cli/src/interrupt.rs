//! Ctrl-C outside the line editor stops the running input, not the client.

use nix::sys::signal::{SigHandler, Signal, signal};
use std::sync::atomic::{AtomicBool, Ordering};

static PENDING: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: std::ffi::c_int) {
    PENDING.store(true, Ordering::SeqCst);
}

/// rustyline replaces the handler while it reads a line, so call this
/// again before each wait.
pub fn install() {
    PENDING.store(false, Ordering::SeqCst);
    // SAFETY: the handler only stores to an atomic.
    let _ = unsafe { signal(Signal::SIGINT, SigHandler::Handler(on_sigint)) };
}

/// Whether Ctrl-C was pressed since the last call.
pub fn take() -> bool {
    PENDING.swap(false, Ordering::SeqCst)
}
