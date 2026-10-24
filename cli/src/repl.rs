//! The interactive REPL.

use crate::highlight;
use crate::interrupt;
use crate::server;
use crate::view::{self, Style, str_field};
use nix::sys::termios::{FlushArg, tcflush};
use peephole_protocol::{Connection, listen, listening, readable, socket_path, state_dir};
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::FileHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Editor, Helper};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::cell::RefCell;
use std::io::{self, IsTerminal};
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(100);

const HELP: &str = "\
Type Rhai expressions or statements. Program variables are live: assignments
change the program. Tab completes, a grey hint previews the value of a path.

  .continue, .c   resume the program and wait for the next stop
  .quit, .q       resume the program and exit (also Ctrl-D)
  .where, .w      show the stop again
  .help           this text

Ctrl-C clears the line, or interrupts a running input.";

/// The program currently stopped, shared with the editor callbacks.
#[derive(Default)]
struct Program {
    conn: Option<Connection>,
    locals: Vec<String>,
}

impl Program {
    fn request(&mut self, message: &Value) -> Option<Value> {
        let reply = self.conn.as_mut()?.request(message);
        reply.ok().flatten()
    }
}

struct Ui {
    program: Rc<RefCell<Program>>,
}

impl Completer for Ui {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let reply = self
            .program
            .borrow_mut()
            .request(&json!({"complete": &line[..pos]}));
        let Some(reply) = reply else {
            return Ok((pos, Vec::new()));
        };
        let start = reply
            .get("start")
            .and_then(Value::as_u64)
            .and_then(|s| usize::try_from(s).ok())
            .unwrap_or(pos);
        let pairs = reply
            .get("completions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| Pair {
                display: str_field(c, "display").to_owned(),
                replacement: str_field(c, "text").to_owned(),
            })
            .collect();
        Ok((start.min(pos), pairs))
    }
}

impl Hinter for Ui {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _: &Context<'_>) -> Option<String> {
        if pos != line.len() || line.trim().is_empty() {
            return None;
        }
        let reply = self
            .program
            .borrow_mut()
            .request(&json!({"preview": line}))?;
        reply
            .get("value")
            .and_then(Value::as_str)
            .map(|value| format!("  = {value}"))
    }
}

impl Highlighter for Ui {
    fn highlight<'l>(&self, line: &'l str, _: usize) -> Cow<'l, str> {
        Cow::Owned(highlight::paint(line, &self.program.borrow().locals))
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("\x1b[2m{hint}\x1b[0m"))
    }

    fn highlight_char(&self, _: &str, _: usize, _: CmdKind) -> bool {
        true
    }
}

impl Validator for Ui {
    fn validate(&self, ctx: &mut ValidationContext<'_>) -> rustyline::Result<ValidationResult> {
        Ok(if highlight::incomplete(ctx.input()) {
            ValidationResult::Incomplete
        } else {
            ValidationResult::Valid(None)
        })
    }
}

impl Helper for Ui {}

struct Bound(std::path::PathBuf);

impl Drop for Bound {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

enum Next {
    Stay,
    Resume,
    Exit,
}

fn evaluate(program: &mut Program, code: &str, style: &Style) -> Next {
    let Some(conn) = program.conn.as_mut() else {
        return Next::Resume;
    };
    interrupt::install();
    let mut interrupted = false;
    let reply = conn.send(&json!({"eval": code})).and_then(|()| {
        while !conn.wait(Some(POLL))? {
            if interrupt::take() && !interrupted {
                interrupted = true;
                conn.send(&json!({"interrupt": true}))?;
            }
        }
        conn.receive()
    });
    let Ok(Some(reply)) = reply else {
        eprintln!("{}", style.paint("31", "program disconnected"));
        program.conn = None;
        return Next::Resume;
    };
    print!("{}", str_field(&reply, "output"));
    let value = str_field(&reply, "value");
    if reply.get("error").and_then(Value::as_bool).unwrap_or(false) {
        eprintln!("{}", style.paint("31", value));
    } else if !value.is_empty() {
        println!(
            "{}",
            if style.color {
                highlight::paint(value, &[])
            } else {
                value.to_owned()
            }
        );
    }
    Next::Stay
}

fn command(program: &mut Program, stop: &Value, line: &str, style: &Style) -> Next {
    match line.trim() {
        "" => Next::Stay,
        ".continue" | ".c" => Next::Resume,
        ".quit" | ".q" => Next::Exit,
        ".where" | ".w" => {
            // Fresh previews, earlier input may have changed values.
            let current = program.request(&json!({"describe": true}));
            let current = current
                .as_ref()
                .and_then(|reply| reply.get("stop"))
                .unwrap_or(stop);
            print!("{}", view::render(current, style));
            Next::Stay
        }
        ".help" | ".h" => {
            println!("{HELP}");
            Next::Stay
        }
        other if other.starts_with('.') && !other.contains(' ') => {
            eprintln!("unknown command {other}, see .help");
            Next::Stay
        }
        _ => evaluate(program, line, style),
    }
}

fn resume(program: &mut Program) {
    if let Some(mut conn) = program.conn.take() {
        let _ = conn.request(&json!({"continue": true}));
    }
}

/// Waits for the next program, or `None` after Ctrl-C.
fn accept(listener: &std::os::unix::net::UnixListener) -> io::Result<Option<Connection>> {
    interrupt::install();
    loop {
        if readable(listener.as_raw_fd(), Some(POLL))? {
            return Connection::new(listener.accept()?.0).map(Some);
        }
        if interrupt::take() {
            return Ok(None);
        }
    }
}

pub fn run() -> io::Result<i32> {
    if server::running() {
        eprintln!(
            "the background server holds {}. End it with `peephole stop`.",
            socket_path().display()
        );
        return Ok(1);
    }
    let path = socket_path();
    if listening(&path) {
        eprintln!("another peephole already listens on {}", path.display());
        return Ok(1);
    }
    let listener = listen(&path)?;
    let _bound = Bound(path.clone());
    let style = Style {
        color: io::stdout().is_terminal(),
    };
    let program = Rc::new(RefCell::new(Program::default()));
    let mut editor: Editor<Ui, FileHistory> = Editor::new().map_err(io::Error::other)?;
    editor.set_helper(Some(Ui {
        program: Rc::clone(&program),
    }));
    let history = state_dir().join("history");
    let _ = editor.load_history(&history);

    loop {
        eprintln!(
            "{}",
            style.paint("2", "waiting for a program to reach peephole::inspect!(…)")
        );
        let Some(mut conn) = accept(&listener)? else {
            break;
        };
        let Ok(Some(message)) = conn.receive() else {
            continue;
        };
        let stop = message.get("stop").cloned().unwrap_or_default();
        // Keys typed while waiting were meant for something else.
        let _ = tcflush(io::stdin(), FlushArg::TCIFLUSH);
        print!("{}", view::render(&stop, &style));
        {
            let mut current = program.borrow_mut();
            current.locals = stop
                .get("locals")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|l| str_field(l, "name").to_owned())
                .collect();
            current.conn = Some(conn);
        }
        let prompt = format!(
            "{}:{}> ",
            std::path::Path::new(str_field(&stop, "file"))
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
            stop.get("line").and_then(Value::as_u64).unwrap_or(0)
        );
        let next = loop {
            let line = match editor.readline(&prompt) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => continue,
                Err(ReadlineError::Eof) => break Next::Exit,
                Err(err) => return Err(io::Error::other(err)),
            };
            let _ = editor.add_history_entry(line.as_str());
            match command(&mut program.borrow_mut(), &stop, &line, &style) {
                Next::Stay => {}
                next => break next,
            }
        };
        resume(&mut program.borrow_mut());
        let _ = editor.save_history(&history);
        if matches!(next, Next::Exit) {
            break;
        }
    }
    Ok(0)
}
