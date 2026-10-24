//! Drives the demo program through `peephole eval|continue|stop`.

use peephole_protocol::{Connection, listen};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn demo() -> &'static Path {
    static DEMO: OnceLock<PathBuf> = OnceLock::new();
    DEMO.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "peephole", "--example", "demo"])
            .status()
            .unwrap();
        assert!(status.success());
        Path::new(env!("CARGO_BIN_EXE_peephole"))
            .with_file_name("examples")
            .join("demo")
    })
}

/// A private state directory, so tests don't share sockets.
struct State(PathBuf);

impl State {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "peephole-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn peephole(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_peephole"))
            .args(args)
            .env("XDG_STATE_HOME", &self.0)
            .output()
            .unwrap()
    }

    fn program(&self) -> Child {
        Command::new(demo())
            .env("XDG_STATE_HOME", &self.0)
            .env("PEEPHOLE_WAIT", "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let _ = self.peephole(&["stop"]);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr)
}

/// Waits for `child` and returns its stdout, failing after a while.
fn finish(mut child: Child) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the program did not continue");
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn evaluates_until_continued() {
    let state = State::new();
    let program = state.program();

    let first = state.peephole(&["eval", "order.id"]);
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    let shown = text(&first);
    assert!(
        shown.contains("stopped at peephole/examples/demo.rs:"),
        "{shown}"
    );
    assert!(shown.contains("demo::checkout"), "{shown}");
    assert!(shown.contains("▶"), "source excerpt missing: {shown}");
    assert!(shown.trim_end().ends_with('7'), "{shown}");

    assert_eq!(
        state
            .peephole(&["eval", "order.items[0].qty = 10"])
            .status
            .code(),
        Some(0)
    );
    let total = state.peephole(&["eval", "order.total()"]);
    assert_eq!(String::from_utf8_lossy(&total.stdout).trim(), "6.5");

    let typo = state.peephole(&["eval", "order.itms"]);
    assert_eq!(typo.status.code(), Some(1));
    assert!(
        text(&typo).contains("Did you mean `items`?"),
        "{}",
        text(&typo)
    );

    assert_eq!(
        state.peephole(&["eval", "attempts += 1"]).status.code(),
        Some(0)
    );
    let resumed = state.peephole(&["continue"]);
    assert_eq!(resumed.status.code(), Some(0));
    assert_eq!(
        finish(program).trim(),
        "qty=10 attempts=2 total=6.5 coupon=None"
    );
}

/// Evaluates `code` and returns stdout, or stderr if it failed.
fn eval(state: &State, code: &str) -> (Option<i32>, String) {
    let output = state.peephole(&["eval", code]);
    (output.status.code(), text(&output).trim().to_owned())
}

#[test]
fn handles_read_only_values_options_and_methods() {
    let state = State::new();
    let program = state.program();
    assert_eq!(eval(&state, "").0, Some(0));

    let (code, shown) = eval(&state, "names.bob = 5");
    assert_eq!(code, Some(1), "{shown}");
    assert!(shown.contains("read-only"), "{shown}");
    assert_eq!(eval(&state, "names.bob"), (Some(0), "2".into()));

    // Methods on properties of read-only values write the property back.
    let shipped = r#"history.filter(|e| e.status.variant() == "Shipped").len()"#;
    assert_eq!(eval(&state, shipped), (Some(0), "1".into()));

    assert_eq!(eval(&state, "order.coupon"), (Some(0), "None".into()));
    let coupon = r#"order.coupon = "SAVE"; order.coupon + "!""#;
    assert_eq!(eval(&state, coupon), (Some(0), r#""SAVE!""#.into()));
    assert_eq!(eval(&state, "order.coupon = ()").0, Some(0));

    let (code, shown) = eval(&state, "order.secret_code()");
    assert_eq!(code, Some(1));
    assert!(
        shown.contains(r#"peephole::expose("secret_code""#),
        "{shown}"
    );

    state.peephole(&["continue"]);
    assert!(finish(program).trim_end().ends_with("coupon=None"));
}

#[test]
fn reports_no_program() {
    let state = State::new();
    let output = state.peephole(&["eval", "-t", "0.2", "1"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
}

#[test]
fn interrupts_endless_input() {
    let state = State::new();
    let program = state.program();
    let output = state.peephole(&["eval", "-t", "0.5", "loop {}"]);
    assert_eq!(output.status.code(), Some(124), "{}", text(&output));
    assert!(text(&output).contains("interrupted"), "{}", text(&output));

    let after = state.peephole(&["eval", "order.id"]);
    assert_eq!(String::from_utf8_lossy(&after.stdout).trim(), "7");
    state.peephole(&["continue"]);
    finish(program);
}

#[test]
fn stop_resumes_programs() {
    let state = State::new();
    let program = state.program();
    let json = state.peephole(&["eval", "--json"]);
    let reply: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(reply["status"], "ok");
    assert_eq!(reply["stop"]["locals"][0]["name"], "order");
    assert_eq!(state.peephole(&["stop"]).status.code(), Some(0));
    assert!(finish(program).starts_with("qty=3 "));
}

#[test]
fn completes_and_previews() {
    let state = State::new();
    let listener = listen(&state.0.join("peephole/sock")).unwrap();
    let program = state.program();
    let mut conn = Connection::new(listener.accept().unwrap().0).unwrap();
    let stop = conn.receive().unwrap().unwrap();
    assert_eq!(stop["stop"]["locals"][1]["type"], "HashMap<String, i64>");

    let reply = conn
        .request(&json!({"complete": "let x = order.items[0].q"}))
        .unwrap()
        .unwrap();
    assert_eq!(reply["start"], 23);
    assert_eq!(
        reply["completions"],
        json!([{"text": "qty", "display": "qty: u32"}])
    );
    let reply = conn
        .request(&json!({"complete": "order.to"}))
        .unwrap()
        .unwrap();
    assert_eq!(reply["completions"][0]["text"], "total");

    let reply = conn
        .request(&json!({"preview": "order.items[-1].name"}))
        .unwrap()
        .unwrap();
    assert_eq!(reply["value"], "\"pear\"");
    let reply = conn
        .request(&json!({"preview": "order.total()"}))
        .unwrap()
        .unwrap();
    assert_eq!(reply, json!({}));

    conn.request(&json!({"continue": true})).unwrap();
    finish(program);
}
