//! Typed edit ops applied to source with tree-sitter — the executable half
//! of the intent algebra (`rename`, `add_param`, `remove_param`).
//!
//! The server never holds source, so this runs where the source is: in the
//! hook binary, on the agent's machine. An op is ~20 tokens for a model to
//! emit where a rewritten function is ~2K, it lands on every call site the
//! parser can see instead of the ones the model remembered, and two ops can
//! be proven to commute where two text diffs cannot. Python and the
//! TypeScript family for now; other grammars are refused, not guessed.
//!
//! Edits are computed as byte ranges from the syntax tree and applied from
//! the end of each file backwards, so no offset moves under a later edit.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use tree_sitter::{Node, Parser};

use crate::engine::extension_of;
use crate::langs::{language_of, spec_for_ext};

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Rename { symbol: String, new_name: String },
    AddParam {
        path: String,
        symbol: String,
        name: String,
        r#type: Option<String>,
        default: Option<String>,
        /// The expression every call site passes; absent means call sites
        /// are left alone and listed, for the model to decide one by one.
        call_arg: Option<String>,
    },
    RemoveParam { path: String, symbol: String, name: String },
    /// Append one expression to every call of `symbol` — under the given
    /// path prefixes only, when callers in different layers need different
    /// expressions (`payload["region"]` in handlers, `settings.REGION` in
    /// jobs). The definition is not touched: that is `add_param`'s job.
    /// `optional`: a recipe replayed against a target whose callers lack
    /// this layer changes nothing here instead of refusing the batch.
    PassArg { path: String, symbol: String, arg: String, within: Vec<String>, optional: bool },
    /// Replace a function's body with the given text; the signature, its
    /// docstring position and the surrounding indentation are the parser's
    /// to keep. The op an agent emits instead of reading a file to edit it.
    ReplaceBody { path: String, symbol: String, body: String },
}

/// One op, or a list of them, from JSON. A list is a batch: applied in
/// order against the same in-memory files, all or nothing.
pub fn parse_ops(v: &Value) -> Result<Vec<Op>, String> {
    match v {
        Value::Array(items) => {
            if items.is_empty() {
                return Err("the batch is empty".into());
            }
            items.iter().enumerate().map(|(i, item)| parse_op(item).map_err(|e| format!("op {}: {e}", i + 1))).collect()
        }
        Value::Object(_) => Ok(vec![parse_op(v)?]),
        _ => Err("the op must be a JSON object or a list of them".into()),
    }
}

/// Every op in order, each seeing the files as the previous one left them.
/// A refused op refuses the batch: nothing is returned to write, so a
/// half-applied change never reaches disk, the ledger or the dashboard.
pub fn apply_all(ops: &[Op], files: &BTreeMap<String, String>) -> Result<Applied, String> {
    let mut working = files.clone();
    let mut out = Applied::default();
    for (i, op) in ops.iter().enumerate() {
        let step = apply(op, &working).map_err(|e| format!("op {} ({}) refused: {e}", i + 1, op.kind()))?;
        for (path, content) in step.files {
            working.insert(path.clone(), content.clone());
            out.files.insert(path, content);
        }
        out.call_sites.extend(step.call_sites);
        out.unchanged_call_sites.extend(step.unchanged_call_sites);
        out.notes.extend(step.notes.into_iter().map(|n| format!("op {}: {n}", i + 1)));
    }
    // a call site an early op left alone but a later op changed (add_param
    // without call_arg, then pass_arg per layer) is handled, not unchanged —
    // reporting it as untouched sent agents off to inspect and re-edit
    out.unchanged_call_sites.retain(|(path, _)| !out.files.contains_key(path));
    if out.unchanged_call_sites.is_empty() {
        out.notes.retain(|n| !n.contains("call sites listed, not changed"));
    }
    Ok(out)
}

impl Op {
    pub fn kind(&self) -> &'static str {
        match self {
            Op::Rename { .. } => "rename",
            Op::AddParam { .. } => "add_param",
            Op::RemoveParam { .. } => "remove_param",
            Op::PassArg { .. } => "pass_arg",
            Op::ReplaceBody { .. } => "replace_body",
        }
    }

    /// The symbol the op is about and the file it is declared against, for
    /// the intent that announces a batch before it lands.
    pub fn target(&self) -> (Option<&str>, &str) {
        match self {
            Op::Rename { symbol, .. } => (None, symbol),
            Op::AddParam { path, symbol, .. } | Op::RemoveParam { path, symbol, .. } | Op::PassArg { path, symbol, .. } | Op::ReplaceBody { path, symbol, .. } => (Some(path), symbol),
        }
    }

    /// The op in the shape `declare_intent`'s typed algebra takes.
    pub fn as_operation(&self) -> Value {
        match self {
            Op::Rename { symbol, new_name } => json!({"op": "rename", "symbol": symbol, "new_name": new_name}),
            Op::AddParam { symbol, name, r#type, default, .. } => {
                let mut v = json!({"op": "add_param", "symbol": symbol, "name": name});
                if let Some(t) = r#type { v["type"] = json!(t); }
                if let Some(d) = default { v["default"] = json!(d); }
                v
            }
            Op::RemoveParam { symbol, name, .. } => json!({"op": "remove_param", "symbol": symbol, "name": name}),
            Op::PassArg { symbol, .. } | Op::ReplaceBody { symbol, .. } => json!({"op": "modify", "symbol": symbol}),
        }
    }
}

/// One op from its JSON shape, the same shape `declare_intent` takes.
pub fn parse_op(v: &Value) -> Result<Op, String> {
    let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let need = |key: &str| text(key).filter(|s| !s.is_empty()).ok_or_else(|| format!("{key} is required"));
    match text("op").as_deref() {
        Some("rename") => Ok(Op::Rename { symbol: need("symbol")?, new_name: need("new_name")? }),
        Some("add_param") => Ok(Op::AddParam {
            path: need("path")?,
            symbol: need("symbol")?,
            name: need("name")?,
            r#type: text("type").filter(|s| !s.is_empty()),
            default: text("default").filter(|s| !s.is_empty()),
            call_arg: text("call_arg").filter(|s| !s.is_empty()),
        }),
        Some("remove_param") => {
            Ok(Op::RemoveParam { path: need("path")?, symbol: need("symbol")?, name: need("name")? })
        }
        Some("replace_body") => Ok(Op::ReplaceBody { path: need("path")?, symbol: need("symbol")?, body: need("body")? }),
        Some("pass_arg") => Ok(Op::PassArg {
            path: need("path")?,
            symbol: need("symbol")?,
            arg: need("arg")?,
            within: v.get("in").and_then(Value::as_array).into_iter().flatten()
                .filter_map(Value::as_str).map(str::to_string).collect(),
            optional: v.get("optional").and_then(Value::as_bool).unwrap_or(false),
        }),
        Some(other) => Err(format!("unsupported op: {other}")),
        None => Err("op is required".into()),
    }
}

#[derive(Debug, Default)]
pub struct Applied {
    /// path -> new content, only for files that changed
    pub files: BTreeMap<String, String>,
    pub call_sites: Vec<(String, usize)>,
    pub unchanged_call_sites: Vec<(String, usize)>,
    pub notes: Vec<String>,
}

impl Applied {
    pub fn summary(&self, ops: &[Op]) -> Value {
        json!({
            "ok": true,
            "ops": ops.iter().map(|op| op.kind()).collect::<Vec<_>>(),
            "files_changed": self.files.keys().collect::<Vec<_>>(),
            "call_sites": self.call_sites.iter().map(|(p, l)| json!({"path": p, "line": l})).collect::<Vec<_>>(),
            "unchanged_call_sites": self.unchanged_call_sites.iter().map(|(p, l)| json!({"path": p, "line": l})).collect::<Vec<_>>(),
            "notes": self.notes,
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Family { Python, Ts }

fn family_of(path: &str) -> Option<Family> {
    match spec_for_ext(&extension_of(path))?.name {
        "python" => Some(Family::Python),
        "typescript" | "tsx" | "javascript" => Some(Family::Ts),
        _ => None,
    }
}

fn parse(path: &str, content: &str) -> Option<tree_sitter::Tree> {
    let spec = spec_for_ext(&extension_of(path))?;
    let mut parser = Parser::new();
    parser.set_language(&language_of(spec.name)).ok()?;
    parser.parse(content, None)
}

fn walk<'a>(node: Node<'a>, f: &mut dyn FnMut(Node<'a>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, f);
    }
}

fn text<'a>(node: Node<'a>, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

fn is_identifier(kind: &str, family: Family) -> bool {
    match family {
        Family::Python => kind == "identifier",
        Family::Ts => matches!(kind, "identifier" | "property_identifier" | "shorthand_property_identifier"),
    }
}

fn is_def(kind: &str, family: Family) -> bool {
    match family {
        Family::Python => kind == "function_definition",
        Family::Ts => matches!(kind, "function_declaration" | "method_definition" | "generator_function_declaration"),
    }
}

fn is_call(kind: &str, family: Family) -> bool {
    match family {
        Family::Python => kind == "call",
        Family::Ts => kind == "call_expression",
    }
}

/// The name a parameter node declares, whatever wrapper it wears.
fn param_name<'a>(node: Node<'a>, src: &'a str, family: Family) -> Option<&'a str> {
    match family {
        Family::Python => match node.kind() {
            "identifier" => Some(text(node, src)),
            "typed_parameter" => node.named_child(0).map(|n| text(n, src)),
            "default_parameter" | "typed_default_parameter" => {
                node.child_by_field_name("name").map(|n| text(n, src))
            }
            _ => None,
        },
        Family::Ts => match node.kind() {
            "identifier" => Some(text(node, src)),
            "required_parameter" | "optional_parameter" => {
                node.child_by_field_name("pattern").map(|n| text(n, src))
            }
            _ => None,
        },
    }
}

#[derive(Debug, Clone)]
struct Change {
    start: usize,
    end: usize,
    text: String,
}

fn splice(content: &str, mut changes: Vec<Change>) -> String {
    changes.sort_by(|a, b| b.start.cmp(&a.start));
    let mut out = content.to_string();
    for change in changes {
        out.replace_range(change.start..change.end, &change.text);
    }
    out
}

/// The names a file binds to the definition's module, so a call site is
/// recognised the way the language resolves it rather than by spelling:
/// `from services.svc0 import serve as serve_a` makes `serve_a(...)` a call
/// of svc0's `serve`, and `from services.svc32 import serve as serve_b`
/// makes `serve_b(...)` not one. Returns (names bound to the symbol itself,
/// names bound to the whole module — usable as `<name>.symbol`).
fn bindings(file: &str, content: &str, def_path: &str, symbol: &str) -> (Vec<String>, Vec<String>) {
    let mut direct = Vec::new();
    let mut modules = Vec::new();
    if file == def_path {
        direct.push(symbol.to_string());
    }
    let Some(parsed) = crate::engine::parse(file, content) else { return (direct, modules) };
    for import in &parsed.imports {
        if !module_matches(def_path, file, &import.module) {
            continue;
        }
        for (local, original) in &import.names {
            if original == symbol {
                direct.push(local.clone());
            } else if original == "*" {
                if local == "*" {
                    direct.push(symbol.to_string()); // `from m import *`
                } else {
                    modules.push(local.clone()); // `import m as local`
                }
            }
        }
    }
    (direct, modules)
}

/// Does an import's module string name the file the symbol is defined in?
/// Python dotted modules against the path, relative imports against the
/// importing file's package; TypeScript relative specifiers against the
/// importing file's directory.
fn module_matches(def_path: &str, importing: &str, module: &str) -> bool {
    let strip_ext = |p: &str| p.rsplit_once('.').map(|(stem, _)| stem.to_string()).unwrap_or_else(|| p.to_string());
    let def_stem = strip_ext(def_path);
    let importer_dir = importing.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
    let normalize = |joined: &str| -> String {
        let mut parts: Vec<&str> = Vec::new();
        for part in joined.split('/') {
            match part {
                "" | "." => {}
                ".." => { parts.pop(); }
                other => parts.push(other),
            }
        }
        parts.join("/")
    };
    if module.starts_with("./") || module.starts_with("../") {
        let resolved = normalize(&format!("{importer_dir}/{module}"));
        return resolved == def_stem || format!("{resolved}/index") == def_stem;
    }
    if let Some(rest) = module.strip_prefix('.') {
        // python relative import: one dot is the package itself, each
        // further dot one package up
        let ups = rest.chars().take_while(|c| *c == '.').count();
        let rest = &rest[ups..];
        let mut base = importer_dir.clone();
        for _ in 0..ups {
            base = base.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        }
        let joined = if rest.is_empty() { base } else if base.is_empty() { rest.replace('.', "/") } else { format!("{base}/{}", rest.replace('.', "/")) };
        return joined == def_stem;
    }
    let dotted = def_stem.replace('/', ".");
    module == dotted || dotted.ends_with(&format!(".{module}")) || module.ends_with(&format!(".{dotted}"))
}

/// Does this call reach the symbol through one of the file's bindings?
fn calls_symbol(function: Node, src: &str, symbol: &str, bindings: &(Vec<String>, Vec<String>)) -> bool {
    let name = text(function, src);
    if bindings.0.iter().any(|b| b == name) {
        return true;
    }
    match name.rsplit_once('.') {
        Some((object, member)) => member == symbol && bindings.1.iter().any(|m| m == object),
        None => false,
    }
}

/// The byte range to remove for one argument or parameter, taking a
/// neighbouring comma with it so the list stays well-formed.
fn removal_range(list: Node, victim: Node, src: &str) -> (usize, usize) {
    let mut cursor = list.walk();
    let children: Vec<Node> = list.children(&mut cursor).collect();
    let idx = children.iter().position(|c| c.id() == victim.id()).unwrap_or(0);
    // prefer eating the comma before; the first element eats the comma after
    if idx > 0 && text(children[idx - 1], src) == "," {
        return (children[idx - 1].start_byte(), victim.end_byte());
    }
    if idx + 1 < children.len() && text(children[idx + 1], src) == "," {
        let mut end = children[idx + 1].end_byte();
        // and the whitespace after that comma, so "a, b" -> "b" not " b"
        while src.as_bytes().get(end) == Some(&b' ') {
            end += 1;
        }
        return (victim.start_byte(), end);
    }
    (victim.start_byte(), victim.end_byte())
}

pub fn apply(op: &Op, files: &BTreeMap<String, String>) -> Result<Applied, String> {
    match op {
        Op::Rename { symbol, new_name } => rename(symbol, new_name, files),
        Op::AddParam { path, symbol, name, r#type, default, call_arg } => {
            add_param(path, symbol, name, r#type.as_deref(), default.as_deref(), call_arg.as_deref(), files)
        }
        Op::RemoveParam { path, symbol, name } => remove_param(path, symbol, name, files),
        Op::PassArg { path, symbol, arg, within, optional } => pass_arg(path, symbol, arg, within, *optional, files),
        Op::ReplaceBody { path, symbol, body } => replace_body(path, symbol, body, files),
    }
}

fn pass_arg(
    def_path: &str, symbol: &str, arg: &str, within: &[String], optional: bool, files: &BTreeMap<String, String>,
) -> Result<Applied, String> {
    let family = family_of(def_path).ok_or_else(|| format!("{def_path}: language not supported for typed edits"))?;
    let mut applied = Applied::default();
    for (path, content) in files {
        if family_of(path) != Some(family) {
            continue;
        }
        if !within.is_empty() && !within.iter().any(|prefix| path.starts_with(prefix.as_str())) {
            continue;
        }
        let Some(tree) = parse(path, content) else { continue };
        let bound = bindings(path, content, def_path, symbol);
        if bound.0.is_empty() && bound.1.is_empty() {
            continue;
        }
        let mut changes = Vec::new();
        let mut touched = Vec::new();
        walk(tree.root_node(), &mut |node| {
            if !is_call(node.kind(), family) {
                return;
            }
            let Some(function) = node.child_by_field_name("function") else { return };
            if !calls_symbol(function, content, symbol, &bound) {
                return;
            }
            let Some(args) = node.child_by_field_name("arguments") else { return };
            let mut cursor = args.walk();
            let children: Vec<Node> = args.children(&mut cursor).collect();
            let existing: Vec<Node> = children.iter().copied().filter(|c| c.is_named()).collect();
            let close = children.last().copied().unwrap_or(args);
            let sep = if existing.is_empty() { String::new() } else { ", ".into() };
            changes.push(Change { start: close.start_byte(), end: close.start_byte(), text: format!("{sep}{arg}") });
            touched.push((path.clone(), node.start_position().row + 1));
        });
        if !changes.is_empty() {
            applied.files.insert(path.clone(), splice(content, changes));
        }
        applied.call_sites.extend(touched);
    }
    if applied.call_sites.is_empty() {
        let where_ = if within.is_empty() { "the repo".to_string() } else { within.join(", ") };
        if optional {
            let mut none = Applied::default();
            none.notes.push(format!("pass_arg skipped: no call of {symbol} under {where_} (optional)"));
            return Ok(none);
        }
        return Err(format!("no call of {symbol} under {where_}"));
    }
    Ok(applied)
}

/// The indentation unit the file uses: the leading whitespace of the first
/// indented line, or four spaces.
fn indent_unit(src: &str) -> String {
    for line in src.lines() {
        let ws: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        if !ws.is_empty() && ws.len() < line.len() {
            return if ws.starts_with('\t') { "\t".to_string() } else { ws };
        }
    }
    "    ".to_string()
}

/// The leading whitespace of the line a byte offset sits on.
fn line_indent(src: &str, at: usize) -> String {
    let line_start = src[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    src[line_start..].chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

/// `body` with its common indentation removed, every line then indented by
/// `indent` except the first (which lands where the block already starts).
fn reindent(body: &str, indent: &str, first_line_indented: bool) -> String {
    let lines: Vec<&str> = body.trim_end().lines().collect();
    let common = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.chars().take_while(|c| *c == ' ' || *c == '\t').count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let stripped: String = if l.trim().is_empty() { String::new() } else { l.chars().skip(common).collect() };
            if stripped.is_empty() { String::new() } else if i == 0 && !first_line_indented { stripped } else { format!("{indent}{stripped}") }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn replace_body(def_path: &str, symbol: &str, body: &str, files: &BTreeMap<String, String>) -> Result<Applied, String> {
    let family = family_of(def_path).ok_or_else(|| format!("{def_path}: language not supported for typed edits"))?;
    let src = files.get(def_path).ok_or_else(|| format!("{def_path} is not among the given files"))?;
    let tree = parse(def_path, src).ok_or_else(|| format!("{def_path}: parse failed"))?;
    if body.trim().is_empty() {
        return Err("replace_body needs a non-empty body".into());
    }
    let mut range: Option<(usize, usize, usize)> = None; // block start, block end, def start
    walk(tree.root_node(), &mut |node| {
        if range.is_some() || !is_def(node.kind(), family) {
            return;
        }
        let Some(name_node) = node.child_by_field_name("name") else { return };
        if text(name_node, src) != symbol {
            return;
        }
        let Some(block) = node.child_by_field_name("body") else { return };
        range = Some((block.start_byte(), block.end_byte(), node.start_byte()));
    });
    let Some((start, end, def_at)) = range else { return Err(format!("{symbol} is not defined in {def_path}")) };
    let def_indent = line_indent(src, def_at);
    let inner = format!("{def_indent}{}", indent_unit(src));
    let new_text = match family {
        // a Python block starts at its first statement, on its own line or
        // right after the colon; either way the rest of the lines take the
        // body's indentation
        Family::Python => {
            let same_line = !src[..start].ends_with(|c: char| c == ' ' || c == '\t') || src[..start].trim_end_matches([' ', '\t']).ends_with(':');
            if same_line && !src[..start].ends_with('\n') && src[..start].trim_end_matches([' ', '\t']).ends_with(':') {
                // `def f(): return 1` — put the new body on its own lines
                format!("\n{}", reindent(body, &inner, true))
            } else {
                reindent(body, &inner, false)
            }
        }
        Family::Ts => format!("{{\n{}\n{def_indent}}}", reindent(body, &inner, true)),
    };
    let mut applied = Applied::default();
    applied.files.insert(def_path.to_string(), splice(src, vec![Change { start, end, text: new_text }]));
    applied.notes.push(format!("replaced the body of {symbol} in {def_path} ({} line(s))", body.trim_end().lines().count()));
    Ok(applied)
}

fn rename(symbol: &str, new_name: &str, files: &BTreeMap<String, String>) -> Result<Applied, String> {
    let mut applied = Applied::default();
    for (path, content) in files {
        let Some(family) = family_of(path) else { continue };
        let Some(tree) = parse(path, content) else { continue };
        let mut changes = Vec::new();
        walk(tree.root_node(), &mut |node| {
            if is_identifier(node.kind(), family) && text(node, content) == symbol {
                changes.push(Change { start: node.start_byte(), end: node.end_byte(), text: new_name.to_string() });
            }
        });
        if !changes.is_empty() {
            applied.files.insert(path.clone(), splice(content, changes));
        }
    }
    if applied.files.is_empty() {
        return Err(format!("{symbol} is not named anywhere in the given files"));
    }
    applied.notes.push("renamed every identifier with that name across the given files (strings and comments untouched)".into());
    Ok(applied)
}

fn add_param(
    def_path: &str, symbol: &str, name: &str, r#type: Option<&str>, default: Option<&str>,
    call_arg: Option<&str>, files: &BTreeMap<String, String>,
) -> Result<Applied, String> {
    let family = family_of(def_path).ok_or_else(|| format!("{def_path}: language not supported for typed edits"))?;
    let def_src = files.get(def_path).ok_or_else(|| format!("{def_path} is not among the given files"))?;
    let tree = parse(def_path, def_src).ok_or_else(|| format!("{def_path}: parse failed"))?;
    let mut applied = Applied::default();

    // the definition: the new parameter goes last, before any *args/**kwargs
    let mut def_changes = Vec::new();
    let mut found = false;
    walk(tree.root_node(), &mut |node| {
        if found || !is_def(node.kind(), family) {
            return;
        }
        let Some(name_node) = node.child_by_field_name("name") else { return };
        if text(name_node, def_src) != symbol {
            return;
        }
        let Some(params) = node.child_by_field_name("parameters") else { return };
        found = true;
        let mut cursor = params.walk();
        let children: Vec<Node> = params.children(&mut cursor).collect();
        let existing: Vec<Node> = children.iter().copied().filter(|c| c.is_named()).collect();
        let mut param = name.to_string();
        if let Some(t) = r#type {
            param.push_str(&format!(": {t}"));
        }
        if let Some(d) = default {
            param.push_str(&format!(" = {d}"));
        }
        let splat = existing.iter().copied().find(|c| {
            matches!(c.kind(), "list_splat_pattern" | "dictionary_splat_pattern" | "keyword_separator" | "rest_pattern")
        });
        match splat {
            Some(s) => def_changes.push(Change { start: s.start_byte(), end: s.start_byte(), text: format!("{param}, ") }),
            None => {
                let close = children.last().copied().unwrap_or(params);
                let sep = if existing.is_empty() { String::new() } else { ", ".into() };
                def_changes.push(Change { start: close.start_byte(), end: close.start_byte(), text: format!("{sep}{param}") });
            }
        }
    });
    if !found {
        return Err(format!("{symbol} is not defined in {def_path}"));
    }
    applied.files.insert(def_path.to_string(), splice(def_src, def_changes));

    // the call sites: every call of that name in every file of the family
    for (path, content) in files {
        if family_of(path) != Some(family) {
            continue;
        }
        let base = applied.files.get(path).cloned().unwrap_or_else(|| content.clone());
        let Some(tree) = parse(path, &base) else { continue };
        let bound = bindings(path, &base, def_path, symbol);
        if bound.0.is_empty() && bound.1.is_empty() {
            continue; // this file cannot reach the symbol at all
        }
        let mut changes = Vec::new();
        let mut touched = Vec::new();
        let mut left = Vec::new();
        walk(tree.root_node(), &mut |node| {
            if !is_call(node.kind(), family) {
                return;
            }
            let Some(function) = node.child_by_field_name("function") else { return };
            if !calls_symbol(function, &base, symbol, &bound) {
                return;
            }
            let Some(args) = node.child_by_field_name("arguments") else { return };
            let line = node.start_position().row + 1;
            let Some(arg) = call_arg else {
                left.push((path.clone(), line));
                return;
            };
            let mut cursor = args.walk();
            let children: Vec<Node> = args.children(&mut cursor).collect();
            let existing: Vec<Node> = children.iter().copied().filter(|c| c.is_named()).collect();
            let keyworded = family == Family::Python && existing.iter().any(|c| c.kind() == "keyword_argument");
            let piece = if keyworded { format!("{name}={arg}") } else { arg.to_string() };
            let close = children.last().copied().unwrap_or(args);
            let sep = if existing.is_empty() { String::new() } else { ", ".into() };
            changes.push(Change { start: close.start_byte(), end: close.start_byte(), text: format!("{sep}{piece}") });
            touched.push((path.clone(), line));
        });
        if !changes.is_empty() {
            applied.files.insert(path.clone(), splice(&base, changes));
        }
        applied.call_sites.extend(touched);
        applied.unchanged_call_sites.extend(left);
    }
    if call_arg.is_none() && !applied.unchanged_call_sites.is_empty() {
        applied.notes.push(match default {
            Some(_) => "call sites left as they were: the default covers them".into(),
            None => "call sites listed, not changed: pass call_arg to give them all one expression, or edit them one by one".into(),
        });
    }
    Ok(applied)
}

fn remove_param(def_path: &str, symbol: &str, name: &str, files: &BTreeMap<String, String>) -> Result<Applied, String> {
    let family = family_of(def_path).ok_or_else(|| format!("{def_path}: language not supported for typed edits"))?;
    let def_src = files.get(def_path).ok_or_else(|| format!("{def_path} is not among the given files"))?;
    let tree = parse(def_path, def_src).ok_or_else(|| format!("{def_path}: parse failed"))?;
    let mut applied = Applied::default();

    let mut def_changes = Vec::new();
    let mut index: Option<usize> = None;
    walk(tree.root_node(), &mut |node| {
        if index.is_some() || !is_def(node.kind(), family) {
            return;
        }
        let Some(name_node) = node.child_by_field_name("name") else { return };
        if text(name_node, def_src) != symbol {
            return;
        }
        let Some(params) = node.child_by_field_name("parameters") else { return };
        let mut cursor = params.walk();
        let named: Vec<Node> = params.children(&mut cursor).filter(|c| c.is_named()).collect();
        for (i, p) in named.iter().enumerate() {
            if param_name(*p, def_src, family) == Some(name) {
                let (start, end) = removal_range(params, *p, def_src);
                def_changes.push(Change { start, end, text: String::new() });
                index = Some(i);
                break;
            }
        }
    });
    let Some(position) = index else {
        return Err(format!("{symbol} in {def_path} has no parameter {name}"));
    };
    applied.files.insert(def_path.to_string(), splice(def_src, def_changes));

    for (path, content) in files {
        if family_of(path) != Some(family) {
            continue;
        }
        let base = applied.files.get(path).cloned().unwrap_or_else(|| content.clone());
        let Some(tree) = parse(path, &base) else { continue };
        let bound = bindings(path, &base, def_path, symbol);
        if bound.0.is_empty() && bound.1.is_empty() {
            continue;
        }
        let mut changes = Vec::new();
        let mut touched = Vec::new();
        walk(tree.root_node(), &mut |node| {
            if !is_call(node.kind(), family) {
                return;
            }
            let Some(function) = node.child_by_field_name("function") else { return };
            if !calls_symbol(function, &base, symbol, &bound) {
                return;
            }
            let Some(args) = node.child_by_field_name("arguments") else { return };
            let mut cursor = args.walk();
            let named: Vec<Node> = args.children(&mut cursor).filter(|c| c.is_named()).collect();
            let by_keyword = named.iter().copied().find(|a| {
                a.kind() == "keyword_argument"
                    && a.child_by_field_name("name").map(|n| text(n, &base)) == Some(name)
            });
            let victim = by_keyword.or_else(|| {
                named.iter().copied().filter(|a| a.kind() != "keyword_argument").nth(position)
            });
            if let Some(v) = victim {
                let (start, end) = removal_range(args, v, &base);
                changes.push(Change { start, end, text: String::new() });
                touched.push((path.clone(), node.start_position().row + 1));
            }
        });
        if !changes.is_empty() {
            applied.files.insert(path.clone(), splice(&base, changes));
        }
        applied.call_sites.extend(touched);
    }
    applied.notes.push("positional call arguments were matched by position; methods called through an instance are not adjusted for self".into());
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect()
    }

    #[test]
    fn add_param_lands_on_the_definition_and_every_call_site() {
        let src = files(&[
            ("core/mod0.py", "FACTOR = 1.1\n\n\ndef compute(x):\n    return x * FACTOR\n"),
            ("services/svc0.py", "from core.mod0 import compute\n\n\ndef serve(x):\n    return compute(x) + 1\n\n\ndef twice(x):\n    return compute(x, ) if False else compute(x=x)\n"),
            ("README.md", "compute(x) is not code\n"),
        ]);
        let op = parse_op(&json!({"op": "add_param", "path": "core/mod0.py", "symbol": "compute", "name": "region", "call_arg": "region"})).unwrap();
        let out = apply(&op, &src).unwrap();
        assert_eq!(out.files["core/mod0.py"], "FACTOR = 1.1\n\n\ndef compute(x, region):\n    return x * FACTOR\n");
        let svc = &out.files["services/svc0.py"];
        assert!(svc.contains("return compute(x, region) + 1"), "{svc}");
        assert!(svc.contains("compute(x=x, region=region)"), "a keyworded call gets a keyword: {svc}");
        assert_eq!(out.call_sites.len(), 3);
        assert!(!out.files.contains_key("README.md"));
    }

    #[test]
    fn add_param_without_call_arg_lists_the_call_sites_instead_of_guessing() {
        let src = files(&[
            ("a.py", "def f(a, *args, **kw):\n    return a\n"),
            ("b.py", "from a import f\nf(1)\n"),
        ]);
        let op = parse_op(&json!({"op": "add_param", "path": "a.py", "symbol": "f", "name": "n", "type": "int", "default": "3"})).unwrap();
        let out = apply(&op, &src).unwrap();
        assert_eq!(out.files["a.py"], "def f(a, n: int = 3, *args, **kw):\n    return a\n");
        assert!(!out.files.contains_key("b.py"));
        assert_eq!(out.unchanged_call_sites, vec![("b.py".to_string(), 2)]);
    }

    #[test]
    fn remove_param_takes_the_argument_out_of_every_call() {
        let src = files(&[
            ("a.py", "def f(a, region, b=2):\n    return a\n"),
            ("b.py", "from a import f\nf(1, 'eu')\nf(1, region='eu', b=3)\n"),
        ]);
        let op = parse_op(&json!({"op": "remove_param", "path": "a.py", "symbol": "f", "name": "region"})).unwrap();
        let out = apply(&op, &src).unwrap();
        assert_eq!(out.files["a.py"], "def f(a, b=2):\n    return a\n");
        assert_eq!(out.files["b.py"], "from a import f\nf(1)\nf(1, b=3)\n");
    }

    #[test]
    fn rename_touches_identifiers_not_strings() {
        let src = files(&[
            ("a.py", "def helper(x):\n    return x  # helper\n"),
            ("b.py", "from a import helper\nprint('helper', helper(1))\n"),
        ]);
        let op = parse_op(&json!({"op": "rename", "symbol": "helper", "new_name": "scale"})).unwrap();
        let out = apply(&op, &src).unwrap();
        assert_eq!(out.files["a.py"], "def scale(x):\n    return x  # helper\n");
        assert_eq!(out.files["b.py"], "from a import scale\nprint('helper', scale(1))\n");
    }

    #[test]
    fn typescript_gets_the_same_treatment() {
        let src = files(&[
            ("lib.ts", "export function compute(x: number): number {\n  return x * 2;\n}\n"),
            ("app.ts", "import { compute } from './lib';\nexport const v = compute(3);\n"),
        ]);
        let op = parse_op(&json!({"op": "add_param", "path": "lib.ts", "symbol": "compute", "name": "region", "type": "string", "call_arg": "region"})).unwrap();
        let out = apply(&op, &src).unwrap();
        assert!(out.files["lib.ts"].contains("function compute(x: number, region: string): number"), "{}", out.files["lib.ts"]);
        assert!(out.files["app.ts"].contains("compute(3, region)"), "{}", out.files["app.ts"]);
    }

    #[test]
    fn pass_arg_reaches_only_the_callers_under_the_given_prefixes() {
        let src = files(&[
            ("services/svc0.py", "def serve(x, region):\n    return x\n"),
            ("handlers/h0.py", "from services.svc0 import serve\n\n\ndef handle(payload):\n    return serve(payload['x'])\n"),
            ("jobs/job0.py", "import settings\nfrom services.svc0 import serve\n\n\ndef run(x):\n    return serve(x) * 2\n"),
        ]);
        let op = parse_op(&json!({"op": "pass_arg", "path": "services/svc0.py", "symbol": "serve", "arg": "payload['region']", "in": ["handlers/"]})).unwrap();
        let out = apply(&op, &src).unwrap();
        assert!(out.files["handlers/h0.py"].contains("serve(payload['x'], payload['region'])"));
        assert!(!out.files.contains_key("jobs/job0.py"));
        let op = parse_op(&json!({"op": "pass_arg", "path": "services/svc0.py", "symbol": "serve", "arg": "settings.REGION", "in": ["jobs/"]})).unwrap();
        let out = apply(&op, &src).unwrap();
        assert!(out.files["jobs/job0.py"].contains("serve(x, settings.REGION) * 2"));
    }

    #[test]
    fn call_sites_are_found_through_import_aliases_and_only_from_the_right_module() {
        let src = files(&[
            ("services/svc0.py", "def serve(x):\n    return x\n"),
            ("services/svc32.py", "def serve(x):\n    return -x\n"),
            ("handlers/h0.py", "from services.svc0 import serve as serve_a\nfrom services.svc32 import serve as serve_b\n\n\ndef handle(payload):\n    return serve_a(payload['x']) + serve_b(payload['x'])\n"),
            ("jobs/job0.py", "import services.svc0 as s0\n\n\ndef run(x):\n    return s0.serve(x)\n"),
            ("pkg/inner.py", "from .svc0 import serve\n\ndef f(x):\n    return serve(x)\n"),
        ]);
        let op = parse_op(&json!({"op": "pass_arg", "path": "services/svc0.py", "symbol": "serve", "arg": "region"})).unwrap();
        let out = apply(&op, &src).unwrap();
        assert_eq!(out.files["handlers/h0.py"], "from services.svc0 import serve as serve_a\nfrom services.svc32 import serve as serve_b\n\n\ndef handle(payload):\n    return serve_a(payload['x'], region) + serve_b(payload['x'])\n");
        assert!(out.files["jobs/job0.py"].contains("s0.serve(x, region)"), "{}", out.files["jobs/job0.py"]);
        assert!(!out.files.contains_key("pkg/inner.py"), "pkg/.svc0 is not services/svc0");
        assert!(!out.files.contains_key("services/svc32.py"));
    }

    #[test]
    fn typescript_relative_specifiers_resolve_against_the_importing_file() {
        let src = files(&[
            ("src/lib/math.ts", "export function compute(x: number) {\n  return x;\n}\n"),
            ("src/app.ts", "import { compute as c } from './lib/math';\nexport const v = c(3);\n"),
            ("src/other/use.ts", "import { compute } from '../lib/math';\nexport const w = compute(4);\n"),
            ("src/decoy.ts", "import { compute } from './lib/other';\nexport const z = compute(5);\n"),
        ]);
        let op = parse_op(&json!({"op": "pass_arg", "path": "src/lib/math.ts", "symbol": "compute", "arg": "region"})).unwrap();
        let out = apply(&op, &src).unwrap();
        assert!(out.files["src/app.ts"].contains("c(3, region)"));
        assert!(out.files["src/other/use.ts"].contains("compute(4, region)"));
        assert!(!out.files.contains_key("src/decoy.ts"));
    }

    #[test]
    fn a_batch_applies_in_order_and_refuses_as_a_whole() {
        let src = files(&[
            ("core/mod0.py", "F = 1


def compute(x):
    return x * F
"),
            ("services/svc0.py", "from core.mod0 import compute


def serve(x):
    return compute(x) + 1
"),
            ("handlers/h0.py", "from services.svc0 import serve as serve_a


def handle(payload):
    return serve_a(payload['x'])
"),
            ("jobs/job0.py", "import settings
from services.svc0 import serve


def run(x):
    return serve(x)
"),
        ]);
        let ops = parse_ops(&json!([
            {"op": "add_param", "path": "core/mod0.py", "symbol": "compute", "name": "region", "call_arg": "region"},
            {"op": "add_param", "path": "services/svc0.py", "symbol": "serve", "name": "region"},
            {"op": "pass_arg", "path": "services/svc0.py", "symbol": "serve", "arg": "payload['region']", "in": ["handlers/"]},
            {"op": "pass_arg", "path": "services/svc0.py", "symbol": "serve", "arg": "settings.REGION", "in": ["jobs/"]},
        ])).unwrap();
        let out = apply_all(&ops, &src).unwrap();
        assert_eq!(out.files.len(), 4, "{:?}", out.files.keys());
        assert!(out.files["services/svc0.py"].contains("def serve(x, region):
    return compute(x, region) + 1"), "{}", out.files["services/svc0.py"]);
        assert!(out.files["handlers/h0.py"].contains("serve_a(payload['x'], payload['region'])"));
        assert!(out.files["jobs/job0.py"].contains("serve(x, settings.REGION)"));
        // one bad op in the middle refuses everything: nothing to write
        let bad = parse_ops(&json!([
            {"op": "add_param", "path": "core/mod0.py", "symbol": "compute", "name": "region"},
            {"op": "add_param", "path": "core/mod0.py", "symbol": "nope", "name": "x"},
        ])).unwrap();
        let err = apply_all(&bad, &src).unwrap_err();
        assert!(err.starts_with("op 2 (add_param) refused"), "{err}");
    }

    #[test]
    fn the_parser_keeps_the_facts_an_agent_reads_a_file_for() {
        let src = "import settings\nfrom services.svc0 import serve as serve_a\n\n\n@cached\ndef handle(payload, retries=3):\n    \"\"\"doc\"\"\"\n    return serve_a(payload['x'], retries) + settings.RETRIES\n";
        let parsed = crate::engine::parse("handlers/h0.py", src).unwrap();
        let handle = parsed.symbols.iter().find(|s| s.name == "handle").unwrap();
        assert_eq!(handle.span, (5, 8), "decorator included, 1-based");
        assert_eq!(handle.params, vec!["payload", "retries"]);
        assert_eq!(handle.sites, vec![("serve_a".to_string(), 8, "payload['x'], retries".to_string())]);
    }

    #[test]
    fn other_languages_are_refused_not_guessed() {
        let src = files(&[("main.go", "func compute(x int) int { return x }\n")]);
        let op = parse_op(&json!({"op": "add_param", "path": "main.go", "symbol": "compute", "name": "r"})).unwrap();
        assert!(apply(&op, &src).unwrap_err().contains("not supported"));
    }

    #[test]
    fn replace_body_keeps_the_signature_and_indentation_in_python() {
        let mut files = BTreeMap::new();
        files.insert("core/mod0.py".to_string(), "FACTOR = 1.1\n\n\ndef compute(x):\n    \"\"\"Apply rule 0.\"\"\"\n    return x * FACTOR\n\n\ndef describe():\n    return \"rule 0\"\n".to_string());
        let op = parse_op(&json!({"op": "replace_body", "path": "core/mod0.py", "symbol": "compute",
                                  "body": "\"\"\"Apply rule 0, doubled in Europe.\"\"\"\nif region.startswith('eu'):\n    return x * FACTOR * 2\nreturn x * FACTOR"})).unwrap();
        let out = apply(&op, &files).unwrap();
        assert_eq!(out.files["core/mod0.py"], "FACTOR = 1.1\n\n\ndef compute(x):\n    \"\"\"Apply rule 0, doubled in Europe.\"\"\"\n    if region.startswith('eu'):\n        return x * FACTOR * 2\n    return x * FACTOR\n\n\ndef describe():\n    return \"rule 0\"\n");
        assert!(out.notes[0].starts_with("replaced the body of compute"));
    }

    #[test]
    fn replace_body_follows_a_methods_indentation_and_a_brace_block() {
        let mut files = BTreeMap::new();
        files.insert("svc.py".to_string(), "class Svc:\n    def serve(self, x):\n        return x\n\n    def other(self):\n        return 1\n".to_string());
        let op = parse_op(&json!({"op": "replace_body", "path": "svc.py", "symbol": "serve", "body": "y = x * 2\nreturn y"})).unwrap();
        let out = apply(&op, &files).unwrap();
        assert_eq!(out.files["svc.py"], "class Svc:\n    def serve(self, x):\n        y = x * 2\n        return y\n\n    def other(self):\n        return 1\n");
        let mut ts = BTreeMap::new();
        ts.insert("a.ts".to_string(), "export function f(x: number) {\n  return x;\n}\n\nfunction g() {\n  return 2;\n}\n".to_string());
        let op = parse_op(&json!({"op": "replace_body", "path": "a.ts", "symbol": "f", "body": "const y = x * 2;\nreturn y;"})).unwrap();
        let out = apply(&op, &ts).unwrap();
        assert_eq!(out.files["a.ts"], "export function f(x: number) {\n  const y = x * 2;\n  return y;\n}\n\nfunction g() {\n  return 2;\n}\n");
        assert!(apply(&parse_op(&json!({"op": "replace_body", "path": "a.ts", "symbol": "nope", "body": "return 1;"})).unwrap(), &ts).is_err());
    }
}
