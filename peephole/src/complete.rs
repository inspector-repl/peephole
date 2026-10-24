//! Completion and live previews, both from read-only path lookups.

use crate::node::{self, Node, Step};
use crate::render;
use crate::session::Session;
use bevy_reflect::{PartialReflect, ReflectRef};
use rhai::Dynamic;
use serde_json::{Value, json};

const LS_WIDTH: usize = 60;
const LS_ITEMS: usize = 20;

const KEYWORDS: &[&str] = &[
    "let", "const", "if", "else", "for", "in", "while", "loop", "break", "continue", "return",
    "fn", "true", "false", "print", "debug",
];

/// Node methods from `node::register` that fit `value`.
fn methods(value: &dyn PartialReflect) -> Vec<(&'static str, &'static str)> {
    let mut out = vec![("type_name", "type_name()")];
    match value.reflect_ref() {
        ReflectRef::List(_) | ReflectRef::Array(_) | ReflectRef::Set(_) => out.extend([
            ("len", "len()"),
            ("to_array", "to_array()"),
            ("map", "map(|x| …)"),
            ("filter", "filter(|x| …)"),
        ]),
        ReflectRef::Map(_) => out.extend([
            ("len", "len()"),
            ("keys", "keys()"),
            ("to_array", "to_array()"),
            ("map", "map(|x| …)"),
            ("filter", "filter(|x| …)"),
        ]),
        ReflectRef::Struct(_) => out.push(("keys", "keys()")),
        ReflectRef::Enum(_) => out.push(("variant", "variant()")),
        _ => {}
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Parses `name(.member|[index]|["key"])*`.
fn parse(expr: &str) -> Option<(&str, Vec<Step<'_>>)> {
    let root_end = expr.find(|c: char| !is_ident(c)).unwrap_or(expr.len());
    let root = &expr[..root_end];
    if root.is_empty() || root.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let mut steps = Vec::new();
    let mut rest = &expr[root_end..];
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('.') {
            let end = tail.find(|c: char| !is_ident(c)).unwrap_or(tail.len());
            if end == 0 {
                return None;
            }
            steps.push(Step::Member(&tail[..end]));
            rest = &tail[end..];
        } else if let Some(tail) = rest.strip_prefix("[\"") {
            let end = tail.find("\"]")?;
            steps.push(Step::Member(&tail[..end]));
            rest = &tail[end + 2..];
        } else {
            let tail = rest.strip_prefix('[')?;
            let end = tail.find(']')?;
            steps.push(Step::Index(tail[..end].trim().parse().ok()?));
            rest = &tail[end + 1..];
        }
    }
    Some((root, steps))
}

/// Whether `code` is a plain path like `order.items[0].name`.
pub fn is_path(code: &str) -> bool {
    parse(code.trim()).is_some()
}

/// Looks up a path without running code, so it has no side effects.
/// Also returns the Rust type, which leaves lose as Rhai values.
fn lookup(session: &Session, expr: &str) -> Result<(Dynamic, String), String> {
    let (root, steps) =
        parse(expr.trim()).ok_or("expected a variable or a path like order.items[0]")?;
    let mut value = session
        .scope
        .get_value::<Dynamic>(root)
        .ok_or_else(|| format!("no variable `{root}`"))?;
    let mut ty = session
        .variables
        .iter()
        .find(|var| var.name == root)
        .map_or_else(|| value.type_name().to_owned(), |var| var.type_name.clone());
    for step in steps {
        let mut node = value
            .try_cast::<Node>()
            .ok_or("only program values have fields")?;
        let mut child = node::child(&mut node, &step).map_err(|err| message(*err))?;
        ty = render::short_type(
            child
                .get()
                .map_err(|err| message(*err))?
                .reflect_type_path(),
        );
        value = node::present(child);
    }
    Ok((value, ty))
}

/// An error without Rhai's "Runtime error:" prefix and position.
pub fn message(mut err: rhai::EvalAltResult) -> String {
    let _ = err.take_position();
    match err {
        rhai::EvalAltResult::ErrorRuntime(message, _) => message.to_string(),
        other => other.to_string(),
    }
}

pub fn path(session: &Session, expr: &str) -> Option<Dynamic> {
    lookup(session, expr).ok().map(|(value, _)| value)
}

/// Everything about a value at once: its type, fields with previews, and
/// the methods that apply. Without `expr`, the stop's locals.
pub fn ls(session: &Session, expr: &str) -> Value {
    let entry = |name: &str, ty: &str, value: &Dynamic| {
        let preview = if value.is_unit() && ty.starts_with("Option<") {
            "None".to_owned()
        } else {
            render::preview(value, LS_WIDTH)
        };
        json!({"name": name, "type": ty, "preview": preview})
    };
    if expr.trim().is_empty() {
        let fields: Vec<Value> = session
            .variables
            .iter()
            .map(|var| {
                let value = session
                    .scope
                    .get_value::<Dynamic>(var.name)
                    .unwrap_or_default();
                entry(var.name, &var.type_name, &value)
            })
            .collect();
        let functions: Vec<String> = exposed()
            .into_iter()
            .map(|(name, _)| format!("{name}()"))
            .collect();
        return json!({"fields": fields, "functions": functions});
    }
    let (value, ty) = match lookup(session, expr) {
        Ok(found) => found,
        Err(err) => return json!({"error": err}),
    };
    let Some(mut node) = value.clone().try_cast::<Node>() else {
        return json!({"type": ty, "value": render::preview(&value, LS_WIDTH)});
    };
    let Ok(reflected) = node.get() else {
        return json!({"error": "stale reference"});
    };
    let type_path = reflected.reflect_type_path().to_owned();
    let mut shown = json!({"type": render::short_type(&type_path)});
    let len = match reflected.reflect_ref() {
        ReflectRef::List(l) => Some(l.len()),
        ReflectRef::Array(a) => Some(a.len()),
        ReflectRef::Map(m) => Some(m.len()),
        ReflectRef::Set(s) => Some(s.len()),
        _ => None,
    };
    if let Some(len) = len {
        shown["len"] = len.into();
    }
    if let ReflectRef::Enum(e) = reflected.reflect_ref() {
        shown["variant"] = e.variant_name().into();
    }
    let mut methods: Vec<String> = methods(reflected)
        .into_iter()
        .map(|(_, shown)| shown.to_owned())
        .collect();
    methods.extend(
        exposed()
            .into_iter()
            .filter(|(_, first)| *first == type_path)
            .map(|(name, _)| format!("{name}()")),
    );
    let mut children: Vec<(String, String, Option<i64>)> = node::members(reflected)
        .into_iter()
        .map(|(name, ty)| (name, ty, None))
        .collect();
    let elements: Vec<&dyn PartialReflect> = match reflected.reflect_ref() {
        ReflectRef::List(l) => l.iter().take(LS_ITEMS).collect(),
        ReflectRef::Array(a) => a.iter().take(LS_ITEMS).collect(),
        _ => Vec::new(),
    };
    children.extend(elements.into_iter().zip(0..).map(|(element, i)| {
        let ty = render::short_type(element.reflect_type_path());
        (format!("[{i}]"), ty, Some(i))
    }));
    let fields: Vec<Value> = children
        .iter()
        .map(|(name, ty, index)| {
            let step = index.map_or(Step::Member(name), Step::Index);
            let child = node::step(&mut node, &step).unwrap_or_default();
            entry(name, ty, &child)
        })
        .collect();
    shown["fields"] = fields.into();
    shown["methods"] = methods.into();
    shown
}

/// The value of `text` if it is a plain path that goes past a variable.
pub fn preview(session: &Session, text: &str, width: usize) -> Option<String> {
    let text = text.trim();
    if !text.contains(['.', '[']) {
        return None;
    }
    path(session, text).map(|value| render::preview(&value, width))
}

/// The path being typed at the end of `line`.
fn trailing_expr(line: &str) -> &str {
    let mut depth = 0usize;
    let mut start = line.len();
    for (i, c) in line.char_indices().rev() {
        match c {
            ']' => depth += 1,
            '[' if depth > 0 => depth -= 1,
            '"' if depth > 0 => {}
            c if is_ident(c) || c == '.' => {}
            _ if depth > 0 => {}
            _ => break,
        }
        start = i;
    }
    &line[start..]
}

fn exposed() -> Vec<(String, String)> {
    crate::functions()
        .iter()
        .map(|(name, function)| (name.clone(), crate::receiver(function).to_owned()))
        .collect()
}

pub fn complete(session: &Session, line: &str) -> Value {
    let expr = trailing_expr(line);
    let mut out: Vec<(String, String)> = Vec::new();
    let partial = if let Some(dot) = expr.rfind('.') {
        let partial = &expr[dot + 1..];
        if let Some(mut node) = path(session, &expr[..dot]).and_then(Dynamic::try_cast::<Node>)
            && let Ok(value) = node.get()
        {
            let type_path = value.reflect_type_path().to_owned();
            out.extend(
                node::members(value)
                    .into_iter()
                    .map(|(name, ty)| (name.clone(), format!("{name}: {ty}"))),
            );
            out.extend(
                methods(value)
                    .into_iter()
                    .map(|(name, shown)| (name.to_owned(), shown.to_owned())),
            );
            out.extend(
                exposed()
                    .into_iter()
                    .filter(|(_, first)| *first == type_path)
                    .map(|(name, _)| (name.clone(), format!("{name}()"))),
            );
        }
        partial
    } else {
        for var in &session.variables {
            out.push((
                var.name.to_owned(),
                format!("{}: {}", var.name, var.type_name),
            ));
        }
        for name in session.scope.iter_raw().map(|(name, ..)| name) {
            if !out.iter().any(|(known, _)| known == name) {
                out.push((name.to_owned(), name.to_owned()));
            }
        }
        out.extend(
            exposed()
                .into_iter()
                .map(|(name, _)| (name.clone(), format!("{name}()"))),
        );
        out.extend(KEYWORDS.iter().map(|k| ((*k).to_owned(), (*k).to_owned())));
        expr
    };
    out.retain(|(text, _)| text.starts_with(partial));
    out.dedup_by(|a, b| a.0 == b.0);
    let completions: Vec<Value> = out
        .into_iter()
        .map(|(text, display)| json!({"text": text, "display": display}))
        .collect();
    json!({"start": line.len() - partial.len(), "completions": completions})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Local;
    use bevy_reflect::Reflect;

    #[derive(Reflect)]
    struct Item {
        name: String,
        qty: u32,
    }

    #[derive(Reflect)]
    struct Order {
        id: u64,
        items: Vec<Item>,
        coupon: Option<String>,
    }

    #[test]
    fn lists_members_with_previews() {
        let mut order = Order {
            id: 7,
            items: vec![Item {
                name: "apple".into(),
                qty: 3,
            }],
            coupon: None,
        };
        let mut locals = [Local::writable("order", &mut order)];
        let session = Session::new(&mut locals);

        let shown = ls(&session, "order");
        assert_eq!(shown["type"], "Order");
        assert_eq!(
            shown["fields"][0],
            json!({"name": "id", "type": "u64", "preview": "7"})
        );
        assert_eq!(
            shown["fields"][1]["preview"],
            r#"[Item { name: "apple", qty: 3 }]"#
        );
        assert!(
            shown["methods"]
                .as_array()
                .unwrap()
                .contains(&json!("keys()"))
        );

        let items = ls(&session, "order.items");
        assert_eq!(items["type"], "Vec<Item>");
        assert_eq!(items["len"], 1);
        assert!(
            items["methods"]
                .as_array()
                .unwrap()
                .contains(&json!("len()"))
        );

        assert_eq!(
            items["fields"][0],
            json!({"name": "[0]", "type": "Item", "preview": r#"Item { name: "apple", qty: 3 }"#})
        );
        assert_eq!(shown["fields"][2]["preview"], "None");

        let top = ls(&session, "");
        assert_eq!(top["fields"][0]["name"], "order");
        assert!(top["functions"].is_array());
        assert_eq!(
            ls(&session, "order.nope")["error"],
            "no field `nope` on Order"
        );
    }

    #[test]
    fn finds_the_expression_being_typed() {
        assert_eq!(
            trailing_expr("let x = order.items[0].na"),
            "order.items[0].na"
        );
        assert_eq!(trailing_expr("f(names[\"bob\"].le"), "names[\"bob\"].le");
        assert_eq!(trailing_expr("1 + ord"), "ord");
    }

    #[test]
    fn parses_paths() {
        let (root, steps) = parse(r#"order.items[-1]["name"]"#).unwrap();
        assert_eq!(root, "order");
        assert!(matches!(
            steps.as_slice(),
            [Step::Member("items"), Step::Index(-1), Step::Member("name")]
        ));
        assert!(parse("order.total()").is_none());
        assert!(parse("1.5").is_none());
    }
}
