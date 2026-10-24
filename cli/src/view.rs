//! The stop header, source excerpt and locals, with or without colour.

use serde_json::Value;
use std::fmt::Write;
use std::path::{Path, PathBuf};

const CONTEXT: usize = 3;

pub struct Style {
    pub color: bool,
}

impl Style {
    pub fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

pub fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// `file!()` is relative to where rustc ran: the crate or its workspace
/// root. The client's own directory comes last, it may hold another
/// project with the same file names.
fn source(stop: &Value) -> Option<PathBuf> {
    let file = Path::new(str_field(stop, "file"));
    if file.is_absolute() {
        return Some(file.to_path_buf());
    }
    let mut candidates = Vec::new();
    for key in ["manifest_dir", "cwd"] {
        let mut dir = Some(Path::new(str_field(stop, key)));
        while let Some(base) = dir.filter(|d| !d.as_os_str().is_empty()) {
            candidates.push(base.join(file));
            dir = base.parent();
        }
    }
    candidates.push(file.to_path_buf());
    candidates.into_iter().find(|path| path.is_file())
}

pub fn render(stop: &Value, style: &Style) -> String {
    let file = str_field(stop, "file");
    let line = stop.get("line").and_then(Value::as_u64).unwrap_or(0);
    let mut out = style.paint(
        "1",
        &format!(
            "stopped at {file}:{line} in {} · pid {} · thread {}",
            str_field(stop, "function"),
            stop.get("pid").and_then(Value::as_u64).unwrap_or(0),
            str_field(stop, "thread"),
        ),
    );
    out.push('\n');
    if let Some(text) = source(stop).and_then(|path| std::fs::read_to_string(path).ok()) {
        let line = usize::try_from(line).unwrap_or(0);
        let first = line.saturating_sub(CONTEXT).max(1);
        for (number, code) in text
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l))
            .skip(first - 1)
            .take(2 * CONTEXT + 1)
        {
            let marker = if number == line {
                style.paint("33", "▶")
            } else {
                " ".into()
            };
            let number = style.paint("2", &format!("{number:>4} │"));
            let _ = writeln!(out, "{marker}{number} {code}");
        }
    }
    let locals = stop
        .get("locals")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !locals.is_empty() {
        out.push_str("locals:\n");
        let name_width = locals
            .iter()
            .map(|l| str_field(l, "name").len())
            .max()
            .unwrap_or(0);
        let type_width = locals
            .iter()
            .map(|l| str_field(l, "type").chars().count())
            .max()
            .unwrap_or(0);
        for local in &locals {
            let name = format!("{:<name_width$}", str_field(local, "name"));
            let ty = format!("{:<type_width$}", str_field(local, "type"));
            let _ = writeln!(
                out,
                "  {}  {}  {}",
                style.paint("1", &name),
                style.paint("36", &ty),
                str_field(local, "preview")
            );
        }
    }
    out
}

/// The reply to `.ls`: a header with the type, then fields and methods.
pub fn render_ls(reply: &Value, style: &Style) -> String {
    let mut out = String::new();
    if let Some(err) = reply.get("error").and_then(Value::as_str) {
        return style.paint("31", err) + "\n";
    }
    let ty = str_field(reply, "type");
    if !ty.is_empty() {
        let mut header = style.paint("36", ty);
        if let Some(variant) = reply.get("variant").and_then(Value::as_str) {
            let _ = write!(header, "::{variant}");
        }
        if let Some(len) = reply.get("len").and_then(Value::as_u64) {
            let _ = write!(header, " · {len} items");
        }
        if let Some(value) = reply.get("value").and_then(Value::as_str) {
            let _ = write!(header, " = {value}");
        }
        let _ = writeln!(out, "{header}");
    }
    let fields = reply
        .get("fields")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let name_width = fields
        .iter()
        .map(|f| str_field(f, "name").len())
        .max()
        .unwrap_or(0);
    let type_width = fields
        .iter()
        .map(|f| str_field(f, "type").chars().count())
        .max()
        .unwrap_or(0);
    for field in &fields {
        let name = format!("{:<name_width$}", str_field(field, "name"));
        let ty = format!("{:<type_width$}", str_field(field, "type"));
        let _ = writeln!(
            out,
            "  {}  {}  {}",
            style.paint("1", &name),
            style.paint("36", &ty),
            str_field(field, "preview")
        );
    }
    for key in ["methods", "functions"] {
        let names: Vec<&str> = reply
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !names.is_empty() {
            let label = style.paint("2", &format!("{key}:"));
            let _ = writeln!(out, "{label} {}", names.join("  "));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_ls() {
        let reply = json!({
            "type": "Vec<Item>", "len": 2,
            "fields": [{"name": "id", "type": "u64", "preview": "7"},
                       {"name": "items", "type": "Vec<Item>", "preview": "[]"}],
            "methods": ["len()", "map(|x| …)"],
        });
        assert_eq!(
            render_ls(&reply, &Style { color: false }),
            "Vec<Item> · 2 items\n  id     u64        7\n  items  Vec<Item>  []\nmethods: len()  map(|x| …)\n"
        );
        let top = json!({"fields": [], "functions": ["total()"]});
        assert_eq!(
            render_ls(&top, &Style { color: false }),
            "functions: total()\n"
        );
        let missing = json!({"error": "no variable `x`"});
        assert_eq!(
            render_ls(&missing, &Style { color: false }),
            "no variable `x`\n"
        );
    }

    #[test]
    fn prefers_the_programs_crate_over_the_clients_directory() {
        let root = std::env::temp_dir().join(format!("peephole-view-{}", std::process::id()));
        let program = root.join("workspace/member");
        std::fs::create_dir_all(program.join("src")).unwrap();
        std::fs::write(program.join("src/main.rs"), "fn main() {}\n").unwrap();
        // `file!()` is relative to the workspace root here.
        let stop = json!({"file": "member/src/main.rs", "manifest_dir": program, "cwd": "/"});
        assert_eq!(
            source(&stop),
            Some(root.join("workspace/member/src/main.rs"))
        );
        let stop = json!({"file": "src/main.rs", "manifest_dir": program, "cwd": "/"});
        assert_eq!(source(&stop), Some(program.join("src/main.rs")));
        std::fs::remove_dir_all(root).unwrap();
    }
}
