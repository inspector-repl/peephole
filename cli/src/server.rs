//! `peephole eval|continue|stop`: one-shot commands for scripts.
//!
//! The first command starts a background server that owns the program
//! socket and keeps one program stopped between commands. Further programs
//! wait in the listen backlog.

use crate::view::{self, Style, str_field};
use peephole_protocol::{Connection, listen, listening, readable, socket_path, state_dir};
use serde_json::{Value, json};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const USAGE: &str = "\
usage: peephole                         interactive REPL
       peephole eval [-t S] [--json] [CODE | -]
       peephole continue [--json]
       peephole stop

eval waits up to S seconds (default 30) for a program to stop at
peephole::inspect!, then runs CODE in it, or stdin with '-'. Input still
running after S seconds is interrupted. Without CODE it describes the
stop. continue resumes the program. A background server, started by the
first command, keeps programs stopped in between. stop ends it and
resumes them.

Exit codes: 0 ok, 1 error in CODE, 2 no program stopped, 3 program
disconnected, 124 timed out.
";

const IDLE_LIMIT: Duration = Duration::from_mins(30);
const START_LIMIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
const REPLY_GRACE: Duration = Duration::from_secs(5);
const DEFAULT_TIMEOUT: f64 = 30.0;

const EXIT_OK: i32 = 0;
const EXIT_FAILED: i32 = 1;
const EXIT_NO_PROGRAM: i32 = 2;
const EXIT_DISCONNECTED: i32 = 3;
const EXIT_TIMED_OUT: i32 = 124; // like timeout(1)

fn control_path() -> PathBuf {
    socket_path().with_file_name("control")
}

fn log_path() -> PathBuf {
    state_dir().join("server.log")
}

fn pid_path() -> PathBuf {
    socket_path().with_file_name("server.pid")
}

pub fn running() -> bool {
    listening(&control_path())
}

fn status(text: &str) -> Value {
    json!({"status": text})
}

struct Held {
    conn: Connection,
    stop: Value,
    announced: bool,
}

/// Holds at most one stopped program.
struct Server {
    programs: UnixListener,
    control: UnixListener,
    held: Option<Held>,
}

impl Server {
    /// The held program, or the next one stopping within `timeout`.
    fn acquire(&mut self, timeout: Duration) -> io::Result<Option<&mut Held>> {
        if self.held.is_none() && readable(self.programs.as_raw_fd(), Some(timeout))? {
            let mut conn = Connection::new(self.programs.accept()?.0)?;
            if let Some(message) = conn.receive()? {
                let stop = message.get("stop").cloned().unwrap_or_default();
                self.held = Some(Held {
                    conn,
                    stop,
                    announced: false,
                });
            }
        }
        Ok(self.held.as_mut())
    }

    fn evaluate(&mut self, request: &Value) -> io::Result<Value> {
        let code = str_field(request, "eval").to_owned();
        let timeout = Duration::from_millis(
            request
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        let start = Instant::now();
        let Some(held) = self.acquire(timeout)? else {
            return Ok(status("none"));
        };
        let mut reply = json!({});
        if !held.announced || code.trim().is_empty() {
            reply["stop"] = held.stop.clone();
            held.announced = true;
        }
        if code.trim().is_empty() {
            reply["status"] = "ok".into();
            return Ok(reply);
        }
        held.conn.send(&json!({"eval": code}))?;
        let mut timed_out = false;
        if !held
            .conn
            .wait(Some(timeout.saturating_sub(start.elapsed())))?
        {
            timed_out = true;
            held.conn.send(&json!({"interrupt": true}))?;
            if !held.conn.wait(Some(REPLY_GRACE))? {
                // Stuck outside the script engine, e.g. in a native call.
                self.held = None;
                reply["status"] = "timeout".into();
                return Ok(reply);
            }
        }
        let Some(result) = held.conn.receive()? else {
            self.held = None;
            reply["status"] = "disconnected".into();
            return Ok(reply);
        };
        reply["value"] = result.get("value").cloned().unwrap_or_default();
        reply["output"] = result.get("output").cloned().unwrap_or_default();
        let failed = result
            .get("error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        reply["status"] = match (timed_out, failed) {
            (true, _) => "timeout",
            (false, true) => "error",
            (false, false) => "ok",
        }
        .into();
        Ok(reply)
    }

    fn resume(&mut self) -> io::Result<Value> {
        let Some(held) = self.acquire(Duration::ZERO)? else {
            return Ok(status("none"));
        };
        let _ = held.conn.request(&json!({"continue": true}));
        self.held = None;
        Ok(json!({"status": "ok", "continued": true}))
    }

    /// Exiting closes every socket, which resumes the held program.
    fn run(&mut self) -> io::Result<()> {
        while readable(self.control.as_raw_fd(), Some(IDLE_LIMIT))? {
            let mut client = Connection::new(self.control.accept()?.0)?;
            let Ok(Some(request)) = client.receive() else {
                continue;
            };
            if request.get("shutdown").is_some() {
                let _ = client.send(&status("ok"));
                return Ok(());
            }
            let reply = if request.get("continue").is_some() {
                self.resume()
            } else {
                self.evaluate(&request)
            };
            let reply = reply.unwrap_or_else(|err| {
                self.held = None;
                json!({"status": "disconnected", "value": err.to_string()})
            });
            let _ = client.send(&reply);
        }
        Ok(())
    }
}

struct Files;

impl Drop for Files {
    fn drop(&mut self) {
        for path in [socket_path(), control_path(), pid_path()] {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn serve() -> i32 {
    if listening(&socket_path()) {
        eprintln!("another REPL listens on {}", socket_path().display());
        return EXIT_FAILED;
    }
    let result = (|| -> io::Result<()> {
        let programs = listen(&socket_path())?;
        let control = listen(&control_path())?;
        let _files = Files;
        std::fs::write(pid_path(), format!("{}\n", std::process::id()))?;
        Server {
            programs,
            control,
            held: None,
        }
        .run()
    })();
    match result {
        Ok(()) => EXIT_OK,
        Err(err) => {
            eprintln!("peephole server: {err}");
            EXIT_FAILED
        }
    }
}

fn start_server() -> io::Result<()> {
    std::fs::create_dir_all(state_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__server")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn()?;
    Ok(())
}

fn server() -> io::Result<Connection> {
    if let Ok(conn) = Connection::connect(&control_path()) {
        return Ok(conn);
    }
    start_server()?;
    let deadline = Instant::now() + START_LIMIT;
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        if let Ok(conn) = Connection::connect(&control_path()) {
            return Ok(conn);
        }
    }
    Err(io::Error::other(format!(
        "the background server did not start. Is an interactive peephole running? See {}",
        log_path().display()
    )))
}

fn exit_code(status: &str) -> i32 {
    match status {
        "ok" => EXIT_OK,
        "error" => EXIT_FAILED,
        "none" => EXIT_NO_PROGRAM,
        "disconnected" => EXIT_DISCONNECTED,
        _ => EXIT_TIMED_OUT,
    }
}

fn report(reply: &Value, json: bool) -> i32 {
    let status = str_field(reply, "status");
    if json {
        println!("{reply}");
        return exit_code(status);
    }
    if let Some(stop) = reply.get("stop") {
        print!("{}", view::render(stop, &Style { color: false }));
    }
    print!("{}", str_field(reply, "output"));
    let value = str_field(reply, "value").trim_end();
    if !value.is_empty() {
        if status == "ok" {
            println!("{value}");
        } else {
            eprintln!("{value}");
        }
    }
    match status {
        "ok" if reply.get("continued").is_some() => println!("continued"),
        "none" => eprintln!("no program stopped at peephole::inspect!"),
        "disconnected" => eprintln!("program disconnected"),
        "timeout" => eprintln!("timed out"),
        _ => {}
    }
    exit_code(status)
}

fn send(request: &Value, timeout: Duration, json: bool) -> io::Result<i32> {
    let mut conn = server()?;
    conn.send(request)?;
    let reply = if conn.wait(Some(timeout))? {
        conn.receive()?
    } else {
        None
    };
    Ok(report(&reply.unwrap_or_else(|| status("timeout")), json))
}

fn eval(args: &[String]) -> io::Result<i32> {
    let mut seconds = DEFAULT_TIMEOUT;
    let mut json = false;
    let mut code = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-t" | "--timeout" => {
                seconds = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| io::Error::other("-t needs a number of seconds"))?;
            }
            "--json" => json = true,
            "-" => {
                let mut text = String::new();
                io::stdin().read_to_string(&mut text)?;
                code.push(text);
            }
            _ => code.push(arg.clone()),
        }
    }
    let timeout = Duration::from_secs_f64(seconds.max(0.0));
    let request = json!({
        "eval": code.join(" "),
        "timeout_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
    });
    send(&request, timeout + REPLY_GRACE, json)
}

fn stop() -> io::Result<i32> {
    if !running() {
        return Ok(EXIT_OK);
    }
    let mut conn = Connection::connect(&control_path())?;
    conn.send(&json!({"shutdown": true}))?;
    if !(conn.wait(Some(START_LIMIT))? && conn.receive()?.is_some()) {
        // Busy in a native call. A live control socket means the pid is current.
        let pid: i32 = std::fs::read_to_string(pid_path())?
            .trim()
            .parse()
            .map_err(io::Error::other)?;
        // SAFETY: plain syscall.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + START_LIMIT;
    while running() {
        if Instant::now() > deadline {
            eprintln!("the server did not exit");
            return Ok(EXIT_FAILED);
        }
        std::thread::sleep(POLL);
    }
    drop(Files);
    Ok(EXIT_OK)
}

pub fn run(command: &str, args: &[String]) -> io::Result<i32> {
    match command {
        "eval" => eval(args),
        "continue" => send(
            &json!({"continue": true}),
            Duration::from_secs_f64(DEFAULT_TIMEOUT),
            args.iter().any(|a| a == "--json"),
        ),
        "stop" => stop(),
        _ => {
            eprint!("{USAGE}");
            Ok(EXIT_FAILED)
        }
    }
}
