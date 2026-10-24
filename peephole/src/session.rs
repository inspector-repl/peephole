//! Serves one stop: evaluates requests until the client continues.

use crate::node::{self, Node, Shared};
use crate::{Local, Location, complete, render};
use peephole_protocol::{Connection, readable, socket_path};
use rhai::packages::{
    BasicArrayPackage, BasicMathPackage, CorePackage, LogicPackage, MoreStringPackage, Package,
};
use rhai::{Dynamic, Engine, EvalAltResult, Scope};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::time::Duration;

const PREVIEW_WIDTH: usize = 60;
const RETRY: Duration = Duration::from_millis(100);
/// Operations between checks for an interrupt from the client.
const CHECK_EVERY: u64 = 1 << 12;

pub struct Variable {
    pub name: &'static str,
    pub type_name: String,
    /// The program's variable, also for leaves that live in the scope as
    /// copies and are written back after each input.
    pub node: Node,
}

pub struct Session {
    pub engine: Engine,
    pub scope: Scope<'static>,
    pub variables: Vec<Variable>,
    output: Rc<RefCell<String>>,
    // Nodes in `scope` point into memory `shared` keeps alive, so it goes last.
    _shared: Rc<Shared>,
}

fn engine(output: &Rc<RefCell<String>>) -> Engine {
    let mut engine = Engine::new_raw();
    for package in [
        CorePackage::new().as_shared_module(),
        LogicPackage::new().as_shared_module(),
        MoreStringPackage::new().as_shared_module(),
        BasicArrayPackage::new().as_shared_module(),
        BasicMathPackage::new().as_shared_module(),
    ] {
        engine.register_global_module(package);
    }
    let printed = Rc::clone(output);
    engine.on_print(move |text| {
        let mut out = printed.borrow_mut();
        out.push_str(text);
        out.push('\n');
    });
    let debugged = Rc::clone(output);
    engine.on_debug(move |text, _, _| {
        let mut out = debugged.borrow_mut();
        out.push_str(text);
        out.push('\n');
    });
    node::register(&mut engine);
    engine
}

impl Session {
    fn new(locals: &mut [Local<'_>]) -> Self {
        let shared = Rc::new(Shared::default());
        let output = Rc::new(RefCell::new(String::new()));
        let mut scope = Scope::new();
        let mut variables = Vec::new();
        for local in locals {
            // SAFETY: the session, and with it every node, ends before
            // `inspect!` returns and the borrow of the local ends.
            let node = unsafe { Node::root(local.value.as_partial_reflect_mut(), &shared) };
            let value = node::leaf(local.value.as_partial_reflect())
                .unwrap_or_else(|| Dynamic::from(node.clone()));
            scope.push_dynamic(local.name, value);
            variables.push(Variable {
                name: local.name,
                type_name: render::short_type(local.type_name),
                node,
            });
        }
        Self {
            engine: engine(&output),
            scope,
            variables,
            output,
            _shared: shared,
        }
    }

    fn describe(&self, location: &Location) -> Value {
        let locals: Vec<Value> = self
            .variables
            .iter()
            .map(|var| {
                let value = self
                    .scope
                    .get_value::<Dynamic>(var.name)
                    .unwrap_or_default();
                json!({
                    "name": var.name,
                    "type": var.type_name,
                    "preview": render::preview(&value, PREVIEW_WIDTH),
                })
            })
            .collect();
        json!({
            "file": location.file,
            "line": location.line,
            "function": location.function,
            "pid": std::process::id(),
            "thread": std::thread::current().name().unwrap_or("?"),
            "cwd": std::env::current_dir().unwrap_or_default(),
            "language": "rhai",
            "locals": locals,
        })
    }

    /// Copies leaf variables, which live in the scope by value, back into
    /// the program.
    fn store_leaves(&mut self) -> Result<(), Box<EvalAltResult>> {
        for var in &mut self.variables {
            let Some(current) = self.scope.get_value::<Dynamic>(var.name) else {
                continue;
            };
            if current.is::<Node>() {
                continue;
            }
            node::store(&mut var.node, &current)?;
        }
        Ok(())
    }

    fn eval(&mut self, code: &str, conn: &Connection) -> Value {
        let fd = conn.as_raw_fd();
        self.engine.on_progress(move |operations| {
            let check = operations % CHECK_EVERY == 0;
            (check && readable(fd, Some(Duration::ZERO)).unwrap_or(true))
                .then(|| "interrupted".into())
        });
        self.output.borrow_mut().clear();
        let result = self
            .engine
            .eval_with_scope::<Dynamic>(&mut self.scope, code)
            .and_then(|value| self.store_leaves().map(|()| value));
        let output = std::mem::take(&mut *self.output.borrow_mut());
        match result {
            Ok(value) => json!({"value": render::result(&value), "output": output, "error": false}),
            Err(err) => {
                let text = match *err {
                    EvalAltResult::ErrorTerminated(..) => "interrupted".to_owned(),
                    other => other.to_string(),
                };
                json!({"value": text, "output": output, "error": true})
            }
        }
    }
}

fn connect() -> Option<Connection> {
    let wait = std::env::var_os("PEEPHOLE_WAIT").is_some_and(|v| !v.is_empty());
    loop {
        match Connection::connect(&socket_path()) {
            Ok(conn) => return Some(conn),
            Err(_) if wait => std::thread::sleep(RETRY),
            Err(_) => return None,
        }
    }
}

fn serve(conn: &mut Connection, location: &Location, session: &mut Session) -> std::io::Result<()> {
    conn.send(&json!({"stop": session.describe(location)}))?;
    while let Some(request) = conn.receive()? {
        let reply = if request.get("continue").is_some() {
            conn.send(&json!({"continued": true}))?;
            return Ok(());
        } else if let Some(code) = request.get("eval").and_then(Value::as_str) {
            session.eval(code, conn)
        } else if let Some(text) = request.get("complete").and_then(Value::as_str) {
            complete::complete(session, text)
        } else if let Some(text) = request.get("preview").and_then(Value::as_str) {
            complete::preview(session, text, PREVIEW_WIDTH)
                .map_or_else(|| json!({}), |value| json!({"value": value}))
        } else if request.get("describe").is_some() {
            json!({"stop": session.describe(location)})
        } else if request.get("interrupt").is_some() {
            continue; // arrived after the input finished
        } else {
            json!({"value": "unknown request", "error": true})
        };
        conn.send(&reply)?;
    }
    Ok(())
}

pub fn run(location: &Location, locals: &mut [Local<'_>]) {
    let Some(mut conn) = connect() else {
        return;
    };
    let mut session = Session::new(locals);
    // A client that goes away just lets the program continue.
    let _ = serve(&mut conn, location, &mut session);
}
