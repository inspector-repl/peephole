//! Values as Rhai source that assigning back reproduces, for `.edit`.

use crate::node::leaf;
use crate::render::short_type;
use bevy_reflect::enums::VariantType;
use bevy_reflect::{PartialReflect, ReflectRef};
use rhai::Dynamic;
use std::fmt::Write;

const WIDTH: usize = 80;
const INDENT: usize = 4;

enum Part {
    Text(String),
    /// `open`, entries with an optional `key: `, `close`.
    Group(&'static str, Vec<(Option<String>, Part)>, &'static str),
}

pub fn literal(value: &dyn PartialReflect) -> Result<String, String> {
    let mut out = String::new();
    write(&part(value)?, 0, &mut out);
    Ok(out)
}

pub fn dynamic(value: &Dynamic) -> Result<String, String> {
    if value.is_unit() {
        return Ok("()".to_owned());
    }
    if let Ok(text) = value.clone().into_string() {
        return Ok(string(&text));
    }
    if let Ok(c) = value.as_char() {
        return Ok(match c {
            '\'' => "'\\''".to_owned(),
            '\\' => "'\\\\'".to_owned(),
            c => format!("'{c}'"),
        });
    }
    if let Ok(float) = value.as_float() {
        return Ok(if float.is_nan() {
            "(0.0 / 0.0)".to_owned()
        } else if float.is_infinite() {
            format!("({}1.0 / 0.0)", if float < 0.0 { "-" } else { "" })
        } else {
            format!("{float:?}")
        });
    }
    if value.is_int() || value.is_bool() {
        return Ok(value.to_string());
    }
    Err(format!(
        "{} can't be written as a Rhai value",
        value.type_name()
    ))
}

fn string(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn key(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_');
    if plain { name.to_owned() } else { string(name) }
}

fn list<'a>(items: impl Iterator<Item = &'a dyn PartialReflect>) -> Result<Part, String> {
    let items = items
        .map(|item| Ok((None, part(item)?)))
        .collect::<Result<_, String>>()?;
    Ok(Part::Group("[", items, "]"))
}

fn part(value: &dyn PartialReflect) -> Result<Part, String> {
    if let Some(leaf) = leaf(value) {
        return dynamic(&leaf).map(Part::Text);
    }
    let unsupported = || {
        format!(
            "{} can't be written as a Rhai value",
            short_type(value.reflect_type_path())
        )
    };
    Ok(match value.reflect_ref() {
        ReflectRef::Struct(fields) => Part::Group(
            "#{",
            (0..fields.field_len())
                .map(|at| {
                    let name = fields.name_at(at).unwrap_or_default();
                    let field = fields.field_at(at).ok_or_else(unsupported)?;
                    Ok((Some(key(name)), part(field)?))
                })
                .collect::<Result<_, String>>()?,
            "}",
        ),
        ReflectRef::TupleStruct(fields) => list(fields.iter_fields())?,
        ReflectRef::Tuple(fields) => list(fields.iter_fields())?,
        ReflectRef::List(items) => list(items.iter())?,
        ReflectRef::Array(items) => list(items.iter())?,
        ReflectRef::Enum(variant)
            if value
                .reflect_type_path()
                .starts_with("core::option::Option<") =>
        {
            match variant.field_at(0) {
                Some(inner) => part(inner)?,
                None => Part::Text("()".to_owned()),
            }
        }
        ReflectRef::Enum(variant) => {
            let name = variant.variant_name();
            let fields = match variant.variant_type() {
                VariantType::Unit => return Ok(Part::Text(string(name))),
                VariantType::Struct => Part::Group(
                    "#{",
                    variant
                        .iter_fields()
                        .map(|field| {
                            Ok((
                                Some(key(field.name().unwrap_or_default())),
                                part(field.value())?,
                            ))
                        })
                        .collect::<Result<_, String>>()?,
                    "}",
                ),
                VariantType::Tuple => list(variant.iter_fields().map(|field| field.value()))?,
            };
            Part::Group("#{", vec![(Some(key(name)), fields)], "}")
        }
        _ => return Err(unsupported()),
    })
}

fn flat(part: &Part) -> String {
    match part {
        Part::Text(text) => text.clone(),
        Part::Group(open, items, close) => {
            let items: Vec<String> = items
                .iter()
                .map(|(key, item)| match key {
                    Some(key) => format!("{key}: {}", flat(item)),
                    None => flat(item),
                })
                .collect();
            format!("{open}{}{close}", items.join(", "))
        }
    }
}

fn write(part: &Part, indent: usize, out: &mut String) {
    let line = flat(part);
    let Part::Group(open, items, close) = part else {
        out.push_str(&line);
        return;
    };
    if indent + line.len() <= WIDTH || items.is_empty() {
        out.push_str(&line);
        return;
    }
    out.push_str(open);
    for (key, item) in items {
        out.push('\n');
        out.push_str(&" ".repeat(indent + INDENT));
        if let Some(key) = key {
            out.push_str(key);
            out.push_str(": ");
        }
        write(item, indent + INDENT, out);
        out.push(',');
    }
    out.push('\n');
    out.push_str(&" ".repeat(indent));
    out.push_str(close);
}
