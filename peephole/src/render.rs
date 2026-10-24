//! Text for values: one line for previews, indented when that's too wide.

use crate::node::{Node, leaf};
use bevy_reflect::{PartialReflect, ReflectRef};
use rhai::Dynamic;
use std::fmt::Write;

const MAX_ITEMS: usize = 20;
const MAX_DEPTH: usize = 6;
const WIDTH: usize = 80;
const INDENT: &str = "  ";

/// `alloc::vec::Vec<demo::Item>` → `Vec<Item>`.
#[must_use]
pub fn short_type(path: &str) -> String {
    let mut out = String::new();
    let mut segment = String::new();
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ':' && chars.peek() == Some(&':') {
            chars.next();
            segment.clear();
        } else if c.is_alphanumeric() || c == '_' {
            segment.push(c);
        } else {
            out.push_str(&segment);
            segment.clear();
            out.push(c);
        }
    }
    out + segment.as_str()
}

enum Doc {
    Leaf(String),
    Group {
        open: String,
        close: &'static str,
        items: Vec<(Option<String>, Doc)>,
        more: usize,
    },
}

fn quoted(value: &Dynamic) -> String {
    if value.is_string() {
        format!("{:?}", value.clone().into_string().unwrap_or_default())
    } else if value.is_char() {
        format!("{:?}", value.as_char().unwrap_or_default())
    } else if value.is_unit() {
        "()".into()
    } else {
        value.to_string()
    }
}

fn group<'a>(
    open: String,
    close: &'static str,
    items: impl IntoIterator<Item = (Option<String>, &'a dyn PartialReflect)>,
    depth: usize,
) -> Doc {
    let items: Vec<_> = items.into_iter().collect();
    let len = items.len();
    if depth >= MAX_DEPTH && len > 0 {
        return Doc::Leaf(format!("{open}…{close}"));
    }
    Doc::Group {
        open,
        close,
        items: items
            .into_iter()
            .take(MAX_ITEMS)
            .map(|(label, value)| (label, doc(value, depth + 1)))
            .collect(),
        more: len.saturating_sub(MAX_ITEMS),
    }
}

fn doc(value: &dyn PartialReflect, depth: usize) -> Doc {
    if let Some(leaf) = leaf(value) {
        return Doc::Leaf(quoted(&leaf));
    }
    let name = value.reflect_short_type_path();
    let name = short_type(name.split('<').next().unwrap_or(name));
    match value.reflect_ref() {
        ReflectRef::Struct(s) => group(
            format!("{name} {{"),
            "}",
            (0..s.field_len())
                .filter_map(|i| Some((s.name_at(i).map(str::to_owned), s.field_at(i)?))),
            depth,
        ),
        ReflectRef::TupleStruct(t) => group(
            format!("{name}("),
            ")",
            t.iter_fields().map(|v| (None, v)),
            depth,
        ),
        ReflectRef::Tuple(t) => group("(".into(), ")", t.iter_fields().map(|v| (None, v)), depth),
        ReflectRef::List(l) => group("[".into(), "]", l.iter().map(|v| (None, v)), depth),
        ReflectRef::Array(a) => group("[".into(), "]", a.iter().map(|v| (None, v)), depth),
        ReflectRef::Set(s) => group("{".into(), "}", s.iter().map(|v| (None, v)), depth),
        ReflectRef::Map(m) => group(
            "{".into(),
            "}",
            m.iter().map(|(k, v)| (Some(flat(k)), v)),
            depth,
        ),
        ReflectRef::Enum(e) => {
            let variant = e.variant_name();
            if e.field_len() == 0 {
                Doc::Leaf(variant.to_owned())
            } else if e.name_at(0).is_some() {
                group(
                    format!("{variant} {{"),
                    "}",
                    (0..e.field_len())
                        .filter_map(|i| Some((e.name_at(i).map(str::to_owned), e.field_at(i)?))),
                    depth,
                )
            } else {
                group(
                    format!("{variant}("),
                    ")",
                    e.iter_fields().map(|f| (None, f.value())),
                    depth,
                )
            }
        }
        _ => Doc::Leaf(format!("{value:?}")),
    }
}

fn separator(label: Option<&String>, open: &str) -> String {
    match label {
        // Map keys are values, struct fields are names.
        Some(label) if open.ends_with('{') && open.len() == 1 => format!("{label} => "),
        Some(label) => format!("{label}: "),
        None => String::new(),
    }
}

fn write_flat(doc: &Doc, out: &mut String) {
    match doc {
        Doc::Leaf(text) => out.push_str(text),
        Doc::Group {
            open,
            close,
            items,
            more,
        } => {
            out.push_str(open);
            let spaced = open.ends_with('{');
            if spaced && !items.is_empty() {
                out.push(' ');
            }
            for (i, (label, item)) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&separator(label.as_ref(), open));
                write_flat(item, out);
            }
            if *more > 0 {
                let _ = write!(out, ", … {more} more");
            }
            if spaced && !items.is_empty() {
                out.push(' ');
            }
            out.push_str(close);
        }
    }
}

fn write_pretty(doc: &Doc, indent: usize, out: &mut String) {
    let mut line = String::new();
    write_flat(doc, &mut line);
    let Doc::Group {
        open,
        close,
        items,
        more,
    } = doc
    else {
        out.push_str(&line);
        return;
    };
    if indent * INDENT.len() + line.chars().count() <= WIDTH || items.is_empty() {
        out.push_str(&line);
        return;
    }
    out.push_str(open);
    out.push('\n');
    for (label, item) in items {
        out.push_str(&INDENT.repeat(indent + 1));
        out.push_str(&separator(label.as_ref(), open));
        write_pretty(item, indent + 1, out);
        out.push_str(",\n");
    }
    if *more > 0 {
        let _ = writeln!(out, "{}… {more} more", INDENT.repeat(indent + 1));
    }
    out.push_str(&INDENT.repeat(indent));
    out.push_str(close);
}

#[must_use]
pub fn flat(value: &dyn PartialReflect) -> String {
    let mut out = String::new();
    write_flat(&doc(value, 0), &mut out);
    out
}

/// One line of at most `max` characters.
#[must_use]
pub fn preview(value: &Dynamic, max: usize) -> String {
    let text = match value.clone().try_cast::<Node>() {
        Some(mut node) => node.get().map_or_else(|err| err.to_string(), flat),
        None => dynamic(value, false),
    };
    if text.chars().count() <= max {
        return text;
    }
    text.chars()
        .take(max.saturating_sub(1))
        .chain(['…'])
        .collect()
}

/// A script result as the REPL shows it, empty for `()`.
#[must_use]
pub fn result(value: &Dynamic) -> String {
    if value.is_unit() {
        return String::new();
    }
    dynamic(value, true)
}

fn dynamic(value: &Dynamic, pretty: bool) -> String {
    if let Some(mut node) = value.clone().try_cast::<Node>() {
        return match node.get() {
            Ok(v) if pretty => {
                let mut out = String::new();
                write_pretty(&doc(v, 0), 0, &mut out);
                out
            }
            Ok(v) => flat(v),
            Err(err) => err.to_string(),
        };
    }
    if value.is_array() {
        let items: Vec<String> = value
            .clone()
            .into_array()
            .unwrap_or_default()
            .iter()
            .map(|v| dynamic(v, false))
            .collect();
        return format!("[{}]", items.join(", "));
    }
    if value.is_map() {
        let map = value.clone().try_cast::<rhai::Map>().unwrap_or_default();
        let items: Vec<String> = map
            .iter()
            .map(|(k, v)| format!("{k}: {}", dynamic(v, false)))
            .collect();
        return format!("#{{{}}}", items.join(", "));
    }
    quoted(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_reflect::Reflect;
    use std::collections::BTreeMap;

    #[derive(Reflect)]
    struct Item {
        name: String,
        qty: u32,
    }

    #[derive(Reflect)]
    enum Status {
        Pending,
        Shipped { tracking: String },
    }

    #[test]
    fn shortens_types() {
        assert_eq!(
            short_type("alloc::vec::Vec<demo::model::Item>"),
            "Vec<Item>"
        );
        assert_eq!(
            short_type("std::collections::hash::map::HashMap<alloc::string::String, i64>"),
            "HashMap<String, i64>"
        );
    }

    #[test]
    fn renders_values() {
        let items = vec![Item {
            name: "apple".into(),
            qty: 3,
        }];
        assert_eq!(flat(&items), r#"[Item { name: "apple", qty: 3 }]"#);
        assert_eq!(flat(&Status::Pending), "Pending");
        assert_eq!(
            flat(&Status::Shipped {
                tracking: "X1".into()
            }),
            r#"Shipped { tracking: "X1" }"#
        );
        assert_eq!(flat(&Some(1u8)), "Some(1)");
        let map: BTreeMap<String, i64> = [("a".into(), 1)].into();
        assert_eq!(flat(&map), r#"{ "a" => 1 }"#);
    }

    #[test]
    fn limits_long_lists() {
        let long: Vec<u32> = (0..25).collect();
        assert!(flat(&long).ends_with(", 19, … 5 more]"));
    }
}
