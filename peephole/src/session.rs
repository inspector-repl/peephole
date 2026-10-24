//! Serves one stop: evaluates requests until the client continues.

use crate::node::{self, Node, Shared};
use crate::{Access, Local, Location, complete, render};
use peephole_protocol::{Connection, readable, socket_path};
use rhai::packages::{
    BasicArrayPackage, BasicMathPackage, CorePackage, LogicPackage, MoreStringPackage, Package,
};
use rhai::{Dynamic, Engine, EvalAltResult, Scope};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::fmt::Write;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::time::Duration;

const PREVIEW_WIDTH: usize = 60;
const MAX_OUTPUT_LINES: usize = 1000;
const RETRY: Duration = Duration::from_millis(100);
/// Operations between checks for an interrupt from the client.
const CHECK_EVERY: u64 = 1 << 12;

pub struct Variable {
    pub name: &'static str,
    pub type_name: String,
    /// Leaves live in the scope as copies, written back through this.
    pub node: Node,
}

pub struct Session {
    pub engine: Engine,
    pub scope: Scope<'static>,
    pub variables: Vec<Variable>,
    output: Rc<RefCell<String>>,
    /// Functions defined by earlier input.
    functions: rhai::AST,
    // Dropped last, nodes in `scope` point into memory it owns.
    _shared: Rc<Shared>,
}

/// Keeps a runaway `print` loop from flooding the terminal.
fn emit(output: &RefCell<String>, text: &str) {
    let mut output = output.borrow_mut();
    let lines = output.lines().count();
    if lines < MAX_OUTPUT_LINES {
        writeln!(output, "{text}").unwrap_or_default();
    } else if lines == MAX_OUTPUT_LINES {
        output.push_str("… more output left out\n");
    }
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
    engine.on_print(move |text| emit(&printed, text));
    let debugged = Rc::clone(output);
    engine.on_debug(move |text, _, _| emit(&debugged, text));
    node::register(&mut engine);
    engine
}

impl Session {
    pub(crate) fn new(locals: &mut [Local<'_>]) -> Self {
        let shared = Rc::new(Shared::default());
        let output = Rc::new(RefCell::new(String::new()));
        let mut scope = Scope::new();
        let mut variables = Vec::new();
        for local in locals {
            // SAFETY: the session and its nodes end before `inspect!`
            // returns and the borrow of the local ends.
            let node = unsafe {
                match &mut local.access {
                    Access::Mut(value) => Node::root(value.as_partial_reflect_mut(), &shared),
                    Access::Shared(value) => Node::root_shared(value.as_partial_reflect(), &shared),
                    Access::Copy(value) => Node::root_shared(value.as_partial_reflect(), &shared),
                }
            };
            let value = node::present(node.clone());
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
            functions: rhai::AST::empty(),
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
            "manifest_dir": location.manifest_dir,
            "line": location.line,
            "function": location.function,
            "pid": std::process::id(),
            "thread": std::thread::current().name().unwrap_or("?"),
            "cwd": std::env::current_dir().unwrap_or_default(),
            "language": "rhai",
            "locals": locals,
        })
    }

    /// Writes variables assigned as a whole back into the program.
    fn store_variables(&mut self) -> Result<(), Box<EvalAltResult>> {
        for var in &mut self.variables {
            let Some(current) = self.scope.get_value::<Dynamic>(var.name) else {
                continue;
            };
            // What the scope held before the input ran.
            let before = node::present(var.node.clone());
            let unchanged = match (
                before.clone().try_cast::<Node>(),
                current.clone().try_cast::<Node>(),
            ) {
                (Some(old), Some(new)) => old.same(&new),
                (None, None) => {
                    before.type_name() == current.type_name()
                        && before.to_string() == current.to_string()
                }
                _ => false,
            };
            if unchanged {
                continue;
            }
            if let Err(err) = node::store(&mut var.node, &current) {
                self.scope.set_value(var.name, before);
                return Err(err);
            }
            // A whole struct was copied in, the scope keeps the program's.
            if node::leaf(var.node.get()?).is_none() {
                self.scope
                    .set_value(var.name, node::present(var.node.clone()));
            }
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
        self.evaluate(code)
    }

    fn evaluate(&mut self, code: &str) -> Value {
        self.output.borrow_mut().clear();
        let result = self
            .engine
            .compile_with_scope(&self.scope, code)
            .map_err(Into::into)
            .and_then(|ast| {
                let value = self
                    .engine
                    .eval_ast_with_scope::<Dynamic>(&mut self.scope, &self.functions.merge(&ast))?;
                // Functions stay defined for later input, like variables.
                self.functions = self.functions.merge(&ast.clone_functions_only());
                Ok(value)
            })
            .and_then(|value| {
                if let Some(mut node) = value.clone().try_cast::<Node>() {
                    node.get()?;
                }
                self.store_variables().map(|()| value)
            });
        let output = std::mem::take(&mut *self.output.borrow_mut());
        match result {
            Ok(value) => {
                // `None` reads as `()`, which would print nothing.
                let shown = if value.is_unit() && complete::is_path(code) {
                    "None".to_owned()
                } else {
                    render::result(&value)
                };
                json!({"value": shown, "output": output, "error": false})
            }
            Err(err) => {
                let line = err.position().line().filter(|_| code.contains('\n'));
                let mut text = match *err {
                    EvalAltResult::ErrorTerminated(..) => "interrupted".to_owned(),
                    EvalAltResult::ErrorFunctionNotFound(ref signature, _)
                        if signature.contains("(()") =>
                    {
                        let name = signature.split(' ').next().unwrap_or_default();
                        format!("`{name}` was called on `()`, which is how None reads")
                    }
                    EvalAltResult::ErrorFunctionNotFound(ref signature, _)
                        if signature.contains("(value") =>
                    {
                        not_callable(signature)
                    }
                    _ => complete::message(*err),
                };
                if let Some(line) = line {
                    write!(text, " (line {line})").unwrap_or_default();
                }
                json!({"value": text, "output": output, "error": true})
            }
        }
    }
}

/// Rhai's own list and map methods only work on Rhai values.
const RESIZING: &[&str] = &[
    "push", "pop", "shift", "insert", "remove", "clear", "append", "truncate", "extend", "retain",
    "drain", "splice", "dedup", "chop",
];

fn not_callable(signature: &str) -> String {
    let name = signature.split(' ').next().unwrap_or_default();
    if RESIZING.contains(&name) {
        format!(
            "`{name}` only works on Rhai values. Assign a whole list instead, e.g. \
             items = items.to_array() + [x]"
        )
    } else if crate::skipped().contains(&name) {
        format!(
            "`{name}` can't be called from the REPL: its arguments \
             must be owned and implement Reflect, e.g. String, not &str."
        )
    } else {
        format!(
            "no method `{name}`. Program methods are callable after \
             #[peephole::methods] on their impl block, or \
             peephole::expose(\"{name}\", Type::{name})."
        )
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
        } else if let Some(text) = request.get("ls").and_then(Value::as_str) {
            complete::ls(session, text)
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_reflect::Reflect;
    use std::collections::HashMap;

    #[derive(Reflect, Debug, PartialEq)]
    enum Status {
        Pending,
        Shipped { tracking: String },
        Split(u32, u32),
    }

    #[derive(Reflect, Debug, PartialEq)]
    struct Item {
        name: String,
        qty: u32,
        tags: Vec<String>,
        size: (u8, u8),
    }

    #[test]
    fn compares_and_remembers() {
        let mut status = Status::Shipped {
            tracking: "XY".into(),
        };
        let other = Status::Pending;
        let mut items = vec![Status::Pending, Status::Pending];
        let coupon: Option<String> = None;
        let mut locals = [
            Local::writable("status", &mut status),
            Local::shared("other", &other),
            Local::writable("items", &mut items),
            Local::shared("coupon", &coupon),
        ];
        let mut session = Session::new(&mut locals);
        let mut value = |code: &str| {
            let reply = session.evaluate(code);
            assert_eq!(reply["error"], false, "{code}: {reply}");
            reply["value"].as_str().unwrap().to_owned()
        };
        assert_eq!(value("items[0] == items[1]"), "true");
        assert_eq!(value("items[0] != status"), "true");
        assert_eq!(value("other == \"Pending\""), "true");
        assert_eq!(value("\"Pending\" == other"), "true");
        assert_eq!(value(r#"status == #{Shipped: #{tracking: "XY"}}"#), "true");
        assert_eq!(value("status == 1"), "false");
        value("fn twice(x) { x * 2 }");
        assert_eq!(value("twice(2)"), "4");
        value("let a = items[0]; items = []");
        let output = session.evaluate("for x in range(0, 5000) { print(x) }");
        assert!(output["output"].as_str().unwrap().lines().count() < 1100);

        assert_eq!(
            error(&mut session, "status = items"),
            "cannot assign Vec<Status> to Status"
        );
        assert_eq!(
            error(&mut session, "items = other"),
            "cannot assign Status to Vec<Status>"
        );
        assert_eq!(
            error(&mut session, "a"),
            "stale reference: a later assignment may have moved this value. Evaluate the expression again."
        );
        assert_eq!(
            error(&mut session, "coupon.len()"),
            "`len` was called on `()`, which is how None reads"
        );
    }

    #[test]
    fn replaces_whole_values() {
        let mut status = Status::Pending;
        let mut item = Item {
            name: "a".into(),
            qty: 1,
            tags: vec!["x".into()],
            size: (1, 1),
        };
        let mut items = vec![Status::Pending, Status::Split(1, 2)];
        let mut locals = [
            Local::writable("status", &mut status),
            Local::writable("item", &mut item),
            Local::writable("items", &mut items),
        ];
        let mut session = Session::new(&mut locals);
        let mut ok = |code: &str| {
            let reply = session.evaluate(code);
            assert_eq!(reply["error"], false, "{code}: {reply}");
        };
        ok(r#"status = #{Shipped: #{tracking: "XY"}}"#);
        ok(r#"item = #{name: "b", qty: 2, tags: ["y", "z"], size: [3, 4]}"#);
        ok("items[0] = items[1]");
        ok("items[1] = \"Pending\"");
        assert_eq!(
            error(&mut session, "item = #{name: \"c\"}"),
            "missing field `qty` for Item"
        );
        assert_eq!(
            error(
                &mut session,
                "item = #{name: \"c\", qty: 1, tags: [], size: [1, 1], x: 1}"
            ),
            "no field `x` on Item"
        );
        assert_eq!(
            error(&mut session, "status = \"Gone\""),
            "no variant `Gone` on Status"
        );
        assert_eq!(
            error(&mut session, "status = #{Split: [1]}"),
            "expected 2 values for Split, got 1"
        );
        drop(session);
        assert_eq!(
            status,
            Status::Shipped {
                tracking: "XY".into()
            }
        );
        assert_eq!(
            item,
            Item {
                name: "b".into(),
                qty: 2,
                tags: vec!["y".into(), "z".into()],
                size: (3, 4)
            }
        );
        assert_eq!(items, [Status::Split(1, 2), Status::Pending]);
    }

    #[derive(Reflect)]
    struct Order {
        id: u64,
        name: String,
        items: Vec<u32>,
        status: Status,
    }

    fn error(session: &mut Session, code: &str) -> String {
        let reply = session.evaluate(code);
        assert_eq!(reply["error"], true, "{code} succeeded: {reply}");
        reply["value"].as_str().unwrap().to_owned()
    }

    #[test]
    fn explains_errors_plainly() {
        let mut order = Order {
            id: 7,
            name: "a".into(),
            items: vec![1],
            status: Status::Pending,
        };
        let names: HashMap<String, i64> = HashMap::from([("bob".into(), 1)]);
        let mut attempts = 1_u32;
        let mut locals = [
            Local::writable("order", &mut order),
            Local::shared("names", &names),
            Local::writable("attempts", &mut attempts),
        ];
        let mut session = Session::new(&mut locals);

        assert_eq!(complete::ls(&session, "attempts")["type"], "u32");
        assert_eq!(complete::ls(&session, "order.name")["type"], "String");
        assert_eq!(
            complete::ls(&session, "names")["type"],
            "HashMap<String, i64>"
        );

        assert_eq!(
            error(&mut session, "order.status = 1"),
            "cannot assign i64 to Status"
        );
        assert_eq!(
            error(&mut session, r#"names["carol"]"#),
            "no key \"carol\" in HashMap<String, i64>"
        );
        assert!(
            error(&mut session, "names.bob = 2").contains("`let mut`"),
            "read-only hint"
        );
        assert_eq!(
            error(&mut session, "order.items.push(2)"),
            "`push` only works on Rhai values. Assign a whole list instead, e.g. \
             items = items.to_array() + [x]"
        );
        assert_eq!(
            error(&mut session, "order.id = 18446744073709551615"),
            "1.8446744073709552e19 is too large, the REPL's integers are i64"
        );
        assert_eq!(
            error(&mut session, "let x = 1;\norder.nope"),
            "no field `nope` on Order. Did you mean `name`? (line 2)"
        );
    }
}
