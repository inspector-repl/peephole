//! Wire protocol between a program stopped at `inspect!` and the client.
//!
//! Messages are JSON objects, each terminated by a NUL byte. The client
//! listens on [`socket_path`]. A program connects when it reaches
//! `inspect!`, sends `{"stop": …}` and then answers requests until it gets
//! `{"continue": true}` or the connection closes.

use serde_json::Value;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `$XDG_STATE_HOME/peephole`, falling back to `~/.local/state/peephole`.
#[must_use]
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|dir| !dir.is_empty())
        .map_or_else(
            || {
                let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
                Path::new(&home).join(".local/state")
            },
            PathBuf::from,
        );
    base.join("peephole")
}

/// Where the client listens for programs. `PEEPHOLE_SOCKET` overrides it.
#[must_use]
pub fn socket_path() -> PathBuf {
    std::env::var_os("PEEPHOLE_SOCKET")
        .filter(|path| !path.is_empty())
        .map_or_else(|| state_dir().join("sock"), PathBuf::from)
}

#[must_use]
pub fn listening(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Binds `path` in a directory only the current user can enter, replacing
/// a socket file nobody listens on anymore.
pub fn listen(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if listening(path) {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("something already listens on {}", path.display()),
        ));
    }
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    UnixListener::bind(path)
}

/// Waits until `fd` is readable or closed. `None` waits forever.
pub fn readable(fd: RawFd, timeout: Option<Duration>) -> io::Result<bool> {
    let millis = timeout.map_or(-1, |t| i32::try_from(t.as_millis()).unwrap_or(i32::MAX));
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `poll` points to one valid pollfd.
        let ready = unsafe { libc::poll(&raw mut poll, 1, millis) };
        if ready >= 0 {
            return Ok(ready > 0);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

pub struct Connection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Connection {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        Ok(Self {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
        })
    }

    pub fn connect(path: &Path) -> io::Result<Self> {
        Self::new(UnixStream::connect(path)?)
    }

    pub fn send(&mut self, message: &Value) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(0);
        self.writer.write_all(&bytes)
    }

    /// The next message, or `None` once the peer hung up.
    pub fn receive(&mut self) -> io::Result<Option<Value>> {
        let mut bytes = Vec::new();
        self.reader.read_until(0, &mut bytes)?;
        if bytes.pop() != Some(0) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    pub fn request(&mut self, message: &Value) -> io::Result<Option<Value>> {
        self.send(message)?;
        self.receive()
    }

    /// Whether a message or hang-up arrives within `timeout`.
    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
        if !self.reader.buffer().is_empty() {
            return Ok(true);
        }
        readable(self.writer.as_raw_fd(), timeout)
    }
}

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        self.writer.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_messages() {
        let (left, right) = UnixStream::pair().unwrap();
        let mut left = Connection::new(left).unwrap();
        let mut right = Connection::new(right).unwrap();
        left.send(&json!({"eval": "a\0b"})).unwrap();
        left.send(&json!({"continue": true})).unwrap();
        assert!(right.wait(Some(Duration::ZERO)).unwrap());
        assert_eq!(right.receive().unwrap(), Some(json!({"eval": "a\0b"})));
        assert_eq!(right.receive().unwrap(), Some(json!({"continue": true})));
        drop(left);
        assert_eq!(right.receive().unwrap(), None);
    }
}
