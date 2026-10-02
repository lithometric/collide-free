//! Resolving a file's raw name-level edges into a cross-file graph.
//!
//! The parser records, per declaration, the names it touches and how. This
//! turns those names into edges between real repo paths by following each
//! file's imports — per language, because a module string means something
//! different in Go than in Rust than in Ruby.
//!
//! Every edge carries how confident the resolution is:
//!
//!   extracted  the target is named by an import, or is in the same file
//!   inferred   resolved by a unique name match across the repo
//!   ambiguous  several files define that name; the nearest was chosen
//!
//! Nothing is invented. A module outside the repo resolves honestly to an
//! `ext:` node rather than being guessed at, because a wrong edge is worse
//! than a missing one: `blast_radius` is read as a list of things that will
//! break.

use std::collections::{BTreeMap, BTreeSet};

pub const EXTRACTED: &str = "extracted";
pub const INFERRED: &str = "inferred";
pub const AMBIGUOUS: &str = "ambiguous";

/// Where a language looks for a file when it sees a module name.
fn ext_candidates(language: &str) -> &'static [&'static str] {
    match language {
        "python" => &[".py", "/__init__.py", ".pyi"],
        "typescript" => &[".ts", ".tsx", ".d.ts", "/index.ts", "/index.tsx", ".js", ".jsx", "/index.js"],
        "tsx" => &[".tsx", ".ts", "/index.tsx", "/index.ts", ".js", ".jsx"],
        "javascript" => &[".js", ".jsx", ".mjs", ".cjs", "/index.js", "/index.jsx", ".ts", ".tsx", "/index.ts"],
        "rust" => &[".rs", "/mod.rs"],
        "ruby" => &[".rb"],
        "php" => &[".php"],
        "java" => &[".java"],
        "c_sharp" => &[".cs"],
        _ => &[""],
    }
}

pub fn node_id(path: &str, symbol: &str) -> String {
    format!("{path}::{symbol}")
}

fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(index) => &path[..index],
        None => "",
    }
}

fn splitext(path: &str) -> (&str, &str) {
    let base = match path.rfind('/') {
        Some(index) => &path[index + 1..],
        None => path,
    };
    match base.rfind('.') {
        Some(dot) if dot > 0 => {
            let cut = path.len() - (base.len() - dot);
            (&path[..cut], &path[cut..])
        }
        _ => (path, ""),
    }
}

/// `posixpath.normpath`, lexically.
fn normpath(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(&last) if last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".into()
    } else {
        joined
    }
}

fn join(base: &str, rest: &str) -> String {
    if rest.starts_with('/') {
        return rest.to_string();
    }
    if base.is_empty() {
        return rest.to_string();
    }
    format!("{}/{rest}", base.trim_end_matches('/'))
}

fn candidates(stem: &str, language: &str) -> Vec<String> {
    ext_candidates(language).iter().map(|ext| normpath(&format!("{stem}{ext}"))).collect()
}

/// The known path ending with `tail` (plus a language extension), preferring
/// one that shares the most leading directories with `near`.
fn suffix_match(
    tail: &str, known: &BTreeSet<String>, language: &str, near: &str,
) -> Option<String> {
    let tail = tail.trim_matches('/');
    if tail.is_empty() {
        return None;
    }
    let mut hits: Vec<String> = Vec::new();
    for candidate in candidates(tail, language) {
        let needle = format!("/{}", candidate.trim_start_matches('/'));
        for path in known {
            if path == &candidate || path.ends_with(&needle) {
                hits.push(path.clone());
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    if hits.len() == 1 || near.is_empty() {
        hits.sort_by_key(|p| (p.len(), p.clone()));
        return hits.into_iter().next();
    }
    let near_parts: Vec<&str> = near.split('/').collect();
    let shared = |path: &str| -> usize {
        path.split('/').zip(near_parts.iter()).take_while(|(a, b)| a == *b).count()
    };
    hits.sort_by_key(|path| (std::cmp::Reverse(shared(path)), path.len(), path.clone()));
    hits.into_iter().next()
}

/// A directory (returned with a trailing slash) whose path ends with `tail`.
fn dir_match(tail: &str, known: &BTreeSet<String>) -> Option<String> {
    let tail = tail.trim_matches('/');
    let mut hits: Vec<String> = known
        .iter()
        .map(|path| dirname(path).to_string())
        .filter(|dir| dir == tail || dir.ends_with(&format!("/{tail}")))
        .collect();
    hits.sort_by_key(|p| (p.len(), p.clone()));
    hits.dedup();
    hits.into_iter().next().map(|dir| format!("{dir}/"))
}

/// The repo path a module string points at, or `None` for something outside
/// the repo. A trailing slash means a package directory.
pub fn resolve_module(
    module: &str, from_path: &str, language: &str, known: &BTreeSet<String>,
) -> Option<String> {
    let module = module.trim();
    if module.is_empty() {
        return None;
    }
    let here = dirname(from_path);

    match language {
        "python" => {
            if let Some(stripped) = module.strip_prefix('.') {
                let dots = module.len() - module.trim_start_matches('.').len();
                let rest = module[dots..].replace('.', "/");
                let _ = stripped;
                let mut base = here.to_string();
                for _ in 1..dots {
                    base = dirname(&base).to_string();
                }
                let stem = if rest.is_empty() { base.clone() } else { join(&base, &rest) };
                for candidate in candidates(&stem, language) {
                    if known.contains(&candidate) {
                        return Some(candidate);
                    }
                }
                return if rest.is_empty() { dir_match(&stem, known) } else { None };
            }
            suffix_match(&module.replace('.', "/"), known, language, from_path)
        }
        "typescript" | "tsx" | "javascript" => {
            if module.starts_with('.') {
                let stem = normpath(&join(here, module));
                let (base, ext) = splitext(&stem);
                let stem = if matches!(ext, ".js" | ".jsx" | ".ts" | ".tsx" | ".mjs" | ".cjs") {
                    base.to_string()
                } else {
                    stem.clone()
                };
                for candidate in candidates(&stem, language) {
                    if known.contains(&candidate) {
                        return Some(candidate);
                    }
                }
                return known.contains(&stem).then_some(stem);
            }
            if module.starts_with("@/") || module.starts_with("~/") || module.starts_with("src/") {
                let trimmed = if module.starts_with('@') || module.starts_with('~') {
                    module.split_once('/').map(|(_, rest)| rest).unwrap_or(module)
                } else {
                    module
                };
                return suffix_match(splitext(trimmed).0, known, language, from_path);
            }
            None // a package
        }
        "rust" => {
            let mut parts: Vec<&str> = module.split("::").filter(|p| !p.is_empty()).collect();
            if parts.is_empty() {
                return None;
            }
            let stem = match parts[0] {
                "crate" => {
                    parts.remove(0);
                    let root = dir_match("src", known).unwrap_or_default();
                    let root = root.trim_end_matches('/').to_string();
                    if parts.is_empty() { root } else { join(&root, &parts.join("/")) }
                }
                "super" => {
                    let mut base = here.to_string();
                    while parts.first() == Some(&"super") {
                        base = dirname(&base).to_string();
                        parts.remove(0);
                    }
                    if parts.is_empty() { base } else { join(&base, &parts.join("/")) }
                }
                "self" => {
                    parts.remove(0);
                    if parts.is_empty() { here.to_string() } else { join(here, &parts.join("/")) }
                }
                _ => return suffix_match(&parts.join("/"), known, language, from_path),
            };
            for candidate in candidates(&stem, language) {
                if known.contains(&candidate) {
                    return Some(candidate);
                }
            }
            dir_match(&stem, known)
        }
        "go" => {
            let head = module.split('/').next().unwrap_or("");
            if head.contains('.') || module.contains('/') {
                dir_match(module.trim_end_matches('/').rsplit('/').next().unwrap_or(""), known)
            } else {
                None
            }
        }
        "java" | "c_sharp" | "php" => {
            let separator = if language == "php" { '\\' } else { '.' };
            let parts: Vec<&str> = module.split(separator).filter(|p| !p.is_empty()).collect();
            if parts.is_empty() {
                return None;
            }
            if let Some(exact) = suffix_match(&parts.join("/"), known, language, from_path) {
                return Some(exact);
            }
            let lowered: Vec<String> = parts
                .iter()
                .map(|p| if language == "java" { p.to_lowercase() } else { p.to_string() })
                .collect();
            dir_match(&lowered.join("/"), known).or_else(|| dir_match(&parts.join("/"), known))
        }
        "c" | "cpp" => {
            if known.contains(module) {
                return Some(module.to_string());
            }
            let joined = normpath(&join(here, module));
            if known.contains(&joined) {
                return Some(joined);
            }
            suffix_match(module, known, language, from_path)
        }
        "ruby" => {
            let stem = module.strip_suffix(".rb").unwrap_or(module);
            let stem = if module.starts_with('.') { normpath(&join(here, stem)) } else { stem.to_string() };
            for candidate in candidates(&stem, language) {
                if known.contains(&candidate) {
                    return Some(candidate.clone());
                }
                let joined = normpath(&join(here, &candidate));
                if known.contains(&joined) {
                    return Some(joined);
                }
            }
            suffix_match(&stem, known, language, from_path)
        }
        _ => suffix_match(&module.replace('.', "/"), known, language, from_path),
    }
}

/// One symbol's raw edges, as the parser recorded them.
pub struct SymbolEdges<'a> {
    pub name: &'a str,
    /// (target name, member accessed on it, edge kind)
    pub edges: &'a [(String, String, String)],
}

pub struct ImportSpec {
    pub module: String,
    /// (local name, original name); "*" as original means the whole module
    pub names: Vec<(String, String)>,
}

pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub confidence: &'static str,
}

/// Resolve one file's edges against the current file set.
pub fn resolve_edges(
    path: &str,
    language: &str,
    symbols: &[SymbolEdges],
    imports: &[ImportSpec],
    known: &BTreeSet<String>,
    name_index: &BTreeMap<String, Vec<String>>,
) -> Vec<Edge> {
    let top_names: BTreeSet<&str> = symbols.iter().map(|s| s.name).collect();
    let mut bound: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    let mut wildcards: Vec<&str> = Vec::new();
    for import in imports {
        for (local, original) in &import.names {
            if local == "*" {
                wildcards.push(&import.module);
            } else {
                bound.insert(local.as_str(), (import.module.as_str(), original.as_str()));
            }
        }
    }

    let mut module_paths: BTreeMap<String, Option<String>> = BTreeMap::new();
    let module_path = |module: &str, cache: &mut BTreeMap<String, Option<String>>| -> Option<String> {
        if let Some(cached) = cache.get(module) {
            return cached.clone();
        }
        let resolved = resolve_module(module, path, language, known);
        cache.insert(module.to_string(), resolved.clone());
        resolved
    };

    let defines = |target_path: &str, name: &str| -> bool {
        name_index.get(name).map(|paths| paths.iter().any(|p| p == target_path)).unwrap_or(false)
    };
    let in_dir = |dir_marker: &str, name: &str| -> Option<String> {
        let prefix = dir_marker.trim_end_matches('/');
        let mut hits: Vec<String> = name_index
            .get(name)
            .map(|paths| paths.iter().filter(|p| dirname(p) == prefix).cloned().collect())
            .unwrap_or_default();
        hits.sort_by_key(|p| (p.len(), p.clone()));
        hits.into_iter().next()
    };

    let mut collected: BTreeMap<(String, String), Edge> = BTreeMap::new();
    let order = ["inherits", "calls", "uses_type", "references"];
    let rank = |kind: &str| order.iter().position(|k| *k == kind).unwrap_or(usize::MAX);

    for symbol in symbols {
        let src = node_id(path, symbol.name);
        for (target, member, kind) in symbol.edges {
            let (target, member) = (target.as_str(), member.as_str());
            let mut confidence = EXTRACTED;
            let to: Option<String> = if top_names.contains(target) && target != symbol.name {
                Some(node_id(path, target))
            } else if let Some((module, original)) = bound.get(target).copied() {
                match module_path(module, &mut module_paths) {
                    None => {
                        let tail = if !member.is_empty() {
                            member
                        } else if original != "*" && original != "default" {
                            original
                        } else {
                            ""
                        };
                        Some(format!("ext:{module}.{tail}").trim_end_matches('.').to_string())
                    }
                    Some(target_path) if target_path.ends_with('/') => {
                        match (!member.is_empty()).then(|| in_dir(&target_path, member)).flatten() {
                            Some(hit) => Some(node_id(&hit, member)),
                            None => {
                                confidence = INFERRED;
                                Some(node_id(&format!("{}/", target_path.trim_end_matches('/')), ""))
                            }
                        }
                    }
                    Some(target_path) if original == "*" || original == "default" => {
                        if !member.is_empty() && defines(&target_path, member) {
                            Some(node_id(&target_path, member))
                        } else if original == "default" {
                            confidence = INFERRED;
                            Some(node_id(&target_path, ""))
                        } else {
                            Some(node_id(&target_path, ""))
                        }
                    }
                    Some(target_path) => {
                        // `from pkg import db` then `db.orders()`: when the
                        // package does not define `db`, it is the submodule
                        let submodule = (language == "python" && !defines(&target_path, original))
                            .then(|| {
                                let sep = if module.ends_with('.') { "" } else { "." };
                                module_path(&format!("{module}{sep}{original}"), &mut module_paths)
                            })
                            .flatten()
                            .filter(|p| !p.ends_with('/') && *p != target_path);
                        if let Some(sub) = submodule {
                            if !member.is_empty() && defines(&sub, member) {
                                Some(node_id(&sub, member))
                            } else {
                                Some(node_id(&sub, ""))
                            }
                        } else {
                            if !defines(&target_path, original) {
                                confidence = INFERRED;
                            }
                            Some(node_id(&target_path, original))
                        }
                    }
                }
            } else {
                let all: Vec<String> = name_index
                    .get(target)
                    .map(|paths| paths.iter().filter(|p| p.as_str() != path).cloned().collect())
                    .unwrap_or_default();
                if all.is_empty() {
                    None
                } else {
                    let same_dir: Vec<String> =
                        all.iter().filter(|p| dirname(p) == dirname(path)).cloned().collect();
                    let wildcard_dirs: Vec<Option<String>> = wildcards
                        .iter()
                        .map(|w| module_path(w, &mut module_paths))
                        .collect();
                    let in_wild: Vec<String> = all
                        .iter()
                        .filter(|candidate| {
                            wildcard_dirs.iter().flatten().any(|w| {
                                candidate.as_str() == w
                                    || (w.ends_with('/') && candidate.starts_with(w.as_str()))
                            })
                        })
                        .cloned()
                        .collect();
                    let mut pool = if !in_wild.is_empty() {
                        in_wild
                    } else if !same_dir.is_empty() {
                        same_dir
                    } else {
                        all
                    };
                    confidence = if pool.len() == 1 { INFERRED } else { AMBIGUOUS };
                    pool.sort_by_key(|p| (p.len(), p.clone()));
                    pool.into_iter().next().map(|winner| node_id(&winner, target))
                }
            };
            let Some(to) = to else { continue };
            if to == src {
                continue;
            }
            let key = (src.clone(), to.clone());
            let replace = collected
                .get(&key)
                .map(|prior| rank(kind) < rank(&prior.kind))
                .unwrap_or(true);
            if replace {
                collected.insert(key, Edge { from: src.clone(), to, kind: kind.clone(), confidence });
            }
        }
    }
    collected.into_values().collect()
}
