//! `apply '<op json | [ops json]>'`: typed edit ops, applied to this checkout.
//!
//! The executable half of the intent algebra. The model emits
//! `{"op":"add_param","path":"core/mod0.py","symbol":"compute","name":"region","call_arg":"region"}`
//! — twenty tokens — and the parser lands it on the definition and on every
//! call site it can see. A list is a whole change in one call: computed
//! first against the files in memory, then declared as ONE intent (the ops
//! are the typed algebra `declare_intent` takes, so the dashboard shows the
//! plan before a file moves), then written and reported file by file, then
//! completed. A refused op refuses the batch: nothing is written, declared
//! or reported — a half-applied change never reaches disk or the ledger.
//! Output is one JSON line. Anything the parser cannot do safely is refused
//! with a reason, never guessed.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::{self, Env};
use crate::http;
use crate::report::{source_of, tracked_sources, user_agent, HOOK_VERSION};

const AGENT: &str = "collide-hook/apply";

/// `apply [--recipe ID] [--at PATH::SYMBOL] [--save-recipe NAME] [ops]`.
///
/// `--at` names the primary target; ops may then say `$target` /
/// `$target.path` for it and `$caller` / `$caller.path` for each function
/// that calls it (expanded from the map, one op per caller). `--save-recipe`
/// keeps the template after the check passes, so the next agent can replay
/// it with `--recipe ID --at <its target>` and no ops at all.
pub fn run_args(args: &[String], env: &Env) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", json!({"_result": "USAGE", "usage": "collide-hook apply [--recipe ID] [--at PATH::SYMBOL] [--save-recipe NAME] '<op | [ops]>' (or the ops on stdin via a quoted heredoc). Ops: add_param{path,symbol,name,call_arg?,type?,default?} · pass_arg{path,symbol,arg,in:[prefixes],optional?} · rename{symbol,new_name} · remove_param{path,symbol,name} · replace_body{path,symbol,body} (the body text, no signature; the parser keeps the indentation). With --at, ops may use $target / $target.path for the named symbol and $caller / $caller.path for each function that calls it. --recipe replays a saved template with no ops. The batch is computed, written, verified with the repo check (restored on failure), declared and reported in this one call; the output's first field `_result` says OK or REFUSED."}));
        return 0;
    }
    let mut recipe = String::new();
    let mut at = String::new();
    let mut save = String::new();
    let mut ops_arg = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--recipe" => { recipe = args.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--at" => { at = args.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            "--save-recipe" => { save = args.get(i + 1).cloned().unwrap_or_default(); i += 2; }
            other => { ops_arg = other.to_string(); i += 1; }
        }
    }
    run_with(&ops_arg, &recipe, &at, &save, env)
}

pub fn run(arg: &str, env: &Env) -> i32 {
    run_with(arg, "", "", "", env)
}

/// A recipe's ops with `$target` and `$caller` placeholders made concrete.
pub fn expand_template(ops: &Value, target_path: &str, target_symbol: &str, callers: &[(String, String)]) -> Result<Value, String> {
    let Some(items) = ops.as_array() else { return Err("recipe ops must be a list".into()) };
    let fill = |v: &Value, path: &str, symbol: &str| -> Value {
        match v {
            Value::String(s) => {
                let mut t = s.clone();
                for (from, to) in [("$target.path", target_path), ("$target", target_symbol), ("$caller.path", path), ("$caller", symbol)] {
                    t = t.replace(from, to);
                }
                Value::String(t)
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| fill_inner(x, path, symbol, target_path, target_symbol)).collect()),
            other => other.clone(),
        }
    };
    let mut out: Vec<Value> = Vec::new();
    for item in items {
        let text = item.to_string();
        if text.contains("$caller") {
            if callers.is_empty() {
                return Err(format!("the recipe has an op per caller but the map lists no callers of {target_path}::{target_symbol}"));
            }
            for (cp, cs) in callers {
                let mut copy = serde_json::Map::new();
                for (k, v) in item.as_object().cloned().unwrap_or_default() {
                    copy.insert(k, fill(&v, cp, cs));
                }
                out.push(Value::Object(copy));
            }
        } else {
            let mut copy = serde_json::Map::new();
            for (k, v) in item.as_object().cloned().unwrap_or_default() {
                copy.insert(k, fill(&v, "", ""));
            }
            out.push(Value::Object(copy));
        }
    }
    Ok(Value::Array(out))
}

/// A concrete batch, generalized: ops on the target become `$target`, ops
/// on any direct caller become `$caller` (identical shapes collapsed to
/// one), layer-specific pass_args become optional. What the first agent
/// wrote for mod0, svc0, svc1, svc2 replays for mod5 and its services.
pub fn generalize(ops: &Value, target_path: &str, target_symbol: &str, callers: &[(String, String)]) -> Value {
    let Some(items) = ops.as_array() else { return ops.clone() };
    let mut out: Vec<Value> = Vec::new();
    for item in items {
        let Some(map) = item.as_object() else { continue };
        let path = map.get("path").and_then(Value::as_str).unwrap_or("");
        let symbol = map.get("symbol").and_then(Value::as_str).unwrap_or("");
        let mut copy = map.clone();
        if path == target_path && symbol == target_symbol {
            copy.insert("path".into(), json!("$target.path"));
            copy.insert("symbol".into(), json!("$target"));
        } else if callers.iter().any(|(p, s)| p == path && s == symbol) {
            copy.insert("path".into(), json!("$caller.path"));
            copy.insert("symbol".into(), json!("$caller"));
        }
        if copy.get("op").and_then(Value::as_str) == Some("pass_arg") && !copy.contains_key("optional") {
            copy.insert("optional".into(), json!(true));
        }
        let value = Value::Object(copy);
        if !out.contains(&value) {
            out.push(value);
        }
    }
    Value::Array(out)
}

fn fill_inner(v: &Value, path: &str, symbol: &str, target_path: &str, target_symbol: &str) -> Value {
    match v {
        Value::String(s) => {
            let mut t = s.clone();
            for (from, to) in [("$target.path", target_path), ("$target", target_symbol), ("$caller.path", path), ("$caller", symbol)] {
                t = t.replace(from, to);
            }
            Value::String(t)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| fill_inner(x, path, symbol, target_path, target_symbol)).collect()),
        other => other.clone(),
    }
}

fn run_with(arg: &str, recipe: &str, at: &str, save: &str, env: &Env) -> i32 {
    let cwd0 = std::env::current_dir().unwrap_or_default();
    let project_dir0 = config::get(env, "CLAUDE_PROJECT_DIR");
    let project0 = (!project_dir0.is_empty()).then(|| PathBuf::from(project_dir0));
    let root0 = config::find_repo_root(&[Some(cwd0.clone()), project0]).unwrap_or(cwd0);
    let cfg0 = config::config(Some(&root0), env);
    // the template, from a saved recipe or from the ops as given
    let template_raw: String = if !recipe.is_empty() {
        if !cfg0.usable() {
            println!("{}", json!({"ok": false, "error": "a recipe needs the Collide server (no config/credential here)"}));
            return 1;
        }
        match http::post(&cfg0.server, "/recipe/get", &cfg0.token, &user_agent(),
                         &json!({"repo_id": cfg0.repo_id, "id": recipe}), Duration::from_secs(10)) {
            Ok(r) if r.get("ok").and_then(Value::as_bool).unwrap_or(false) => r.get("ops").cloned().unwrap_or(Value::Null).to_string(),
            Ok(r) => {
                println!("{}", json!({"ok": false, "error": format!("recipe {recipe}: {}", r.get("reason").and_then(Value::as_str).unwrap_or("not found"))}));
                return 1;
            }
            Err(_) => {
                println!("{}", json!({"ok": false, "error": "could not fetch the recipe from the server"}));
                return 1;
            }
        }
    } else if arg.is_empty() {
        let mut buf = String::new();
        let _ = std::io::stdin().read_to_string(&mut buf);
        buf
    } else {
        arg.to_string()
    };
    // `$target` / `$caller` placeholders need --at; callers come from the map
    let raw = if !at.is_empty() || template_raw.contains("$target") || template_raw.contains("$caller") {
        let Some((tpath, tsymbol)) = at.split_once("::") else {
            println!("{}", json!({"ok": false, "error": "--at must be <path>::<symbol> when the ops use $target/$caller"}));
            return 1;
        };
        let parsed = serde_json::from_str::<Value>(&template_raw)
            .or_else(|first| serde_json::from_str::<Value>(&template_raw.replace("\\'", "'")).map_err(|_| first));
        let Ok(template) = parsed else {
            println!("{}", json!({"ok": false, "error": "the recipe/ops must be a JSON object or a list of them"}));
            return 1;
        };
        let template = if template.is_object() { Value::Array(vec![template]) } else { template };
        let mut callers: Vec<(String, String)> = Vec::new();
        if template.to_string().contains("$caller") {
            if !cfg0.usable() {
                println!("{}", json!({"ok": false, "error": "$caller ops need the Collide server to list the callers"}));
                return 1;
            }
            if let Ok(r) = http::post(&cfg0.server, "/callers", &cfg0.token, &user_agent(),
                                      &json!({"repo_id": cfg0.repo_id, "path": tpath, "symbol": tsymbol}), Duration::from_secs(10)) {
                for c in r.get("callers").and_then(Value::as_array).into_iter().flatten() {
                    let p = c.get("path").and_then(Value::as_str).unwrap_or("");
                    let s = c.get("symbol").and_then(Value::as_str).unwrap_or("");
                    if !p.is_empty() && !s.is_empty() {
                        callers.push((p.to_string(), s.to_string()));
                    }
                }
            }
        }
        match expand_template(&template, tpath, tsymbol, &callers) {
            Ok(v) => v.to_string(),
            Err(e) => {
                println!("{}", json!({"ok": false, "error": e}));
                return 1;
            }
        }
    } else {
        template_raw.clone()
    };
    // a batch with a target is a recipe by default: the next agent on the
    // same shape replays it instead of redoing it. A replay never re-saves.
    let saved_template = if !save.is_empty() {
        Some((save.to_string(), template_raw.clone(), at.to_string()))
    } else if recipe.is_empty() && !at.is_empty() {
        Some((auto_recipe_name(&template_raw, at), template_raw.clone(), at.to_string()))
    } else {
        None
    };
    run_batch(&raw, recipe, saved_template, env)
}

/// A name for a recipe nobody named: what the first op does to the target.
pub fn auto_recipe_name(template_raw: &str, at: &str) -> String {
    let symbol = at.split_once("::").map(|(_, s)| s).unwrap_or(at);
    let parsed = serde_json::from_str::<Value>(template_raw).unwrap_or(Value::Null);
    let ops: Vec<Value> = match parsed {
        Value::Array(a) => a,
        Value::Object(_) => vec![parsed],
        _ => Vec::new(),
    };
    let field = |op: &Value, key: &str| op.get(key).and_then(Value::as_str).unwrap_or("").replace("$target", symbol);
    let first = ops.first().map(|op| match op.get("op").and_then(Value::as_str).unwrap_or("") {
        "add_param" => format!("add {} to {}", field(op, "name"), field(op, "symbol")),
        "remove_param" => format!("remove {} from {}", field(op, "name"), field(op, "symbol")),
        "rename" => format!("rename {} to {}", field(op, "symbol"), field(op, "new_name")),
        "pass_arg" => format!("pass {} to {}", field(op, "arg"), field(op, "symbol")),
        "replace_body" => format!("rewrite {}", field(op, "symbol")),
        other => format!("{other} {}", field(op, "symbol")),
    }).unwrap_or_else(|| format!("change {symbol}"));
    if ops.len() > 1 { format!("{first} (+{} op(s))", ops.len() - 1) } else { first }
}

fn run_batch(raw: &str, recipe_id: &str, save: Option<(String, String, String)>, env: &Env) -> i32 {
    let raw = raw.to_string();
    // A batch written inside a single-quoted shell argument arrives with
    // `\'` where the model meant `'` — bash does not unescape inside single
    // quotes. That is the one artifact worth repairing before refusing: a
    // refusal here costs the agent a whole turn. Stdin (a heredoc) has no
    // such problem and is the documented way to pass a batch.
    let parsed = serde_json::from_str::<Value>(&raw)
        .or_else(|first| serde_json::from_str::<Value>(&raw.replace("\\'", "'")).map_err(|_| first));
    let Ok(ops_json) = parsed else {
        println!("{}", json!({"_result": "REFUSED — the op must be a JSON object or a list of them", "ok": false, "error": "the op must be a JSON object or a list of them (pass a batch on stdin with a heredoc to avoid shell quoting; `collide-hook apply --help` for the shape)"}));
        return 1;
    };
    let ops = match collide_core::edit::parse_ops(&ops_json) {
        Ok(ops) => ops,
        Err(problem) => {
            println!("{}", json!({"_result": format!("REFUSED — {problem}"), "ok": false, "error": problem}));
            return 1;
        }
    };
    let cwd = std::env::current_dir().unwrap_or_default();
    let project_dir = config::get(env, "CLAUDE_PROJECT_DIR");
    let project = (!project_dir.is_empty()).then(|| PathBuf::from(project_dir));
    let root = config::find_repo_root(&[Some(cwd.clone()), project]).unwrap_or(cwd);

    // every tracked source file is a candidate: call sites live anywhere
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for rel in tracked_sources(&root) {
        if let Some(content) = source_of(&root.join(&rel)) {
            files.insert(rel, content);
        }
    }
    // all of it in memory first: a refusal here costs nothing anywhere
    let applied = match collide_core::edit::apply_all(&ops, &files) {
        Ok(applied) => applied,
        Err(problem) => {
            println!("{}", json!({"_result": format!("REFUSED — {problem}; nothing was written"), "ok": false, "error": problem, "written": false}));
            return 1;
        }
    };

    let cfg = config::config(Some(&root), env);
    let live = cfg.usable();
    let before: BTreeMap<String, String> = applied.files.keys()
        .filter_map(|rel| files.get(rel).map(|c| (rel.clone(), c.clone())))
        .collect();
    for (rel, content) in &applied.files {
        if std::fs::write(root.join(rel), content).is_err() {
            println!("{}", json!({"ok": false, "error": format!("could not write {rel}"), "written": false}));
            return 1;
        }
    }
    // Verification by the compiler, not by inference: the repo's own check
    // runs on the written files before anything is declared or reported.
    // If it fails, every file goes back exactly as it was — a batch that
    // breaks the build never becomes repo state, a ledger row or a
    // dashboard event — and the agent gets the check's tail, not a guess.
    if !cfg.verify.is_empty() {
        let check = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cfg.verify)
            .current_dir(&root)
            .output();
        let (passed, tail) = match check {
            Ok(out) => {
                let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                let tail: String = text.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
                (out.status.success(), tail)
            }
            Err(e) => (false, format!("could not run verify: {e}")),
        };
        if !passed {
            for (rel, content) in &before {
                let _ = std::fs::write(root.join(rel), content);
            }
            println!("{}", json!({
                "_result": format!("REFUSED — the repo check `{}` failed on the written batch; every file was restored, nothing changed", cfg.verify),
                "ok": false,
                "error": format!("verify failed ({}); every file restored", cfg.verify),
                "verify": {"command": cfg.verify, "tail": tail},
                "reverted": true, "written": false,
            }));
            return 1;
        }
    }
    // the plan, on the record before it is reported file by file
    let mut paths: BTreeSet<String> = applied.files.keys().cloned().collect();
    let mut symbols: BTreeSet<String> = BTreeSet::new();
    for op in &ops {
        let (path, symbol) = op.target();
        if let Some(p) = path {
            paths.insert(p.to_string());
        }
        symbols.insert(symbol.to_string());
    }
    let summary = format!(
        "{} typed op(s) via collide-hook apply{}: {}",
        ops.len(),
        if recipe_id.is_empty() { String::new() } else { format!(" (recipe {recipe_id})") },
        ops.iter().map(|op| op.kind()).collect::<Vec<_>>().join(", ")
    );
    let mut intent_id = String::new();
    if live {
        if let Ok(declared) = http::post(
            &cfg.server, "/intent/declare", &cfg.token, &user_agent(),
            &json!({
                "repo_id": cfg.repo_id,
                "paths": paths.iter().collect::<Vec<_>>(),
                "symbols": symbols.iter().collect::<Vec<_>>(),
                "operations": ops.iter().map(|op| op.as_operation()).collect::<Vec<_>>(),
                "summary": summary,
                "agent": AGENT,
            }),
            Duration::from_secs(10),
        ) {
            intent_id = declared.get("intent_id").and_then(Value::as_str).unwrap_or("").to_string();
        }
    }

    // each file reported like any write, so the map, the ledger and every
    // teammate's next check see it — without the model in the loop
    if live {
        for (rel, content) in &applied.files {
            let mut payload = json!({"repo_id": cfg.repo_id, "path": rel,
                        "agent": AGENT, "hook_version": HOOK_VERSION,
                        "verified": if cfg.verify.is_empty() { Value::Null } else {
                            json!({"command": cfg.verify, "ok": true}) }});
            if let Some(map) = payload.as_object_mut() {
                map.extend(cfg.source_fields(rel, content));
            }
            let _ = http::post(&cfg.server, "/report", &cfg.token, &user_agent(), &payload, Duration::from_secs(10));
        }
        if !intent_id.is_empty() {
            let _ = http::post(
                &cfg.server, "/intent/complete", &cfg.token, &user_agent(),
                &json!({"repo_id": cfg.repo_id, "intent_id": intent_id, "rationale": summary}),
                Duration::from_secs(10),
            );
        }
        // what the batch saved: every file landed without the agent reading
        // or editing it; with a recipe, another agent's work was replayed
        if !applied.files.is_empty() {
            let _ = http::post(
                &cfg.server, "/apply/done", &cfg.token, &user_agent(),
                &json!({"repo_id": cfg.repo_id, "files": applied.files.len(), "ops": ops.len(),
                        "recipe": recipe_id, "agent": AGENT}),
                Duration::from_secs(5),
            );
        }
    }
    let mut out = applied.summary(&ops);
    if let Some(map) = out.as_object_mut() {
        map.insert("intent_id".into(), json!(intent_id));
        map.insert("declared".into(), json!(!intent_id.is_empty()));
        map.insert("reported".into(), json!(live));
        if !recipe_id.is_empty() {
            map.insert("recipe".into(), json!(recipe_id));
        }
        // the verdict first, in words: agents re-ran batches and inspected
        // diffs when the first thing they saw was a list of call sites
        let verified = if cfg.verify.is_empty() {
            String::new()
        } else {
            map.insert("verified".into(), json!({"command": cfg.verify, "ok": true}));
            format!(" and verified by `{}`", cfg.verify)
        };
        let via = if recipe_id.is_empty() { String::new() } else { format!(" (recipe {recipe_id})") };
        map.insert("_result".into(), json!(format!(
            "OK — {} file(s) written{verified}{via}: the definition and every call site are done and reported. Do not run this again.",
            applied.files.len()
        )));
        // a long call-site list is noise once the verdict is known
        if applied.call_sites.len() > 8 {
            let sample: Vec<Value> = applied.call_sites.iter().take(8).map(|(p, l)| json!({"path": p, "line": l})).collect();
            map.insert("call_sites".into(), json!(sample));
            map.insert("call_sites_more".into(), json!(applied.call_sites.len() - 8));
        }
    }
    // the batch passed the check: keep it as a recipe for the next agent
    if let (Some((name, template, at)), true) = (save, live) {
        let (target_path, target_symbol) = at.split_once("::").map(|(p, s)| (p.to_string(), s.to_string())).unwrap_or_default();
        let template_json = serde_json::from_str::<Value>(&template).unwrap_or(Value::Null);
        let mut template_json = if template_json.is_object() { Value::Array(vec![template_json]) } else { template_json };
        // a concrete batch (no placeholders) is generalized here, where the
        // target and its callers are known — the first agent should not
        // have to write a template to leave one behind
        // always: an agent may write the callers as $caller and still name
        // the target concretely, and placeholders already present pass through
        if !target_symbol.is_empty() {
            let mut callers: Vec<(String, String)> = Vec::new();
            if let Ok(r) = http::post(&cfg.server, "/callers", &cfg.token, &user_agent(),
                                      &json!({"repo_id": cfg.repo_id, "path": target_path, "symbol": target_symbol}), Duration::from_secs(10)) {
                for c in r.get("callers").and_then(Value::as_array).into_iter().flatten() {
                    let p = c.get("path").and_then(Value::as_str).unwrap_or("");
                    let s = c.get("symbol").and_then(Value::as_str).unwrap_or("");
                    if !p.is_empty() && !s.is_empty() {
                        callers.push((p.to_string(), s.to_string()));
                    }
                }
            }
            template_json = generalize(&template_json, &target_path, &target_symbol, &callers);
        }
        if let Ok(saved) = http::post(
            &cfg.server, "/recipe", &cfg.token, &user_agent(),
            &json!({"repo_id": cfg.repo_id, "name": name, "target_symbol": target_symbol, "ops": template_json,
                    "files": applied.files.len(), "verified": !cfg.verify.is_empty(), "applied_at": at}),
            Duration::from_secs(10),
        ) {
            if let Some(map) = out.as_object_mut() {
                map.insert("recipe_saved".into(), saved.get("id").cloned().unwrap_or(Value::Null));
                map.insert("replay".into(), saved.get("replay").cloned().unwrap_or(Value::Null));
            }
        }
    }
    println!("{out}");
    0
}

#[cfg(test)]
mod tests {
    use super::{expand_template, generalize};
    use serde_json::json;

    #[test]
    fn a_concrete_batch_generalizes_and_replays_elsewhere() {
        let concrete = json!([
            {"op":"add_param","path":"core/mod0.py","symbol":"compute","name":"region"},
            {"op":"pass_arg","path":"core/mod0.py","symbol":"compute","arg":"region","in":["services/"]},
            {"op":"add_param","path":"services/svc0.py","symbol":"serve","name":"region"},
            {"op":"pass_arg","path":"services/svc0.py","symbol":"serve","arg":"payload['region']","in":["handlers/"]},
            {"op":"add_param","path":"services/svc1.py","symbol":"serve","name":"region"},
            {"op":"pass_arg","path":"services/svc1.py","symbol":"serve","arg":"payload['region']","in":["handlers/"]},
        ]);
        let callers = vec![("services/svc0.py".to_string(), "serve".to_string()), ("services/svc1.py".to_string(), "serve".to_string())];
        let template = generalize(&concrete, "core/mod0.py", "compute", &callers);
        assert_eq!(template, json!([
            {"op":"add_param","path":"$target.path","symbol":"$target","name":"region"},
            {"op":"pass_arg","path":"$target.path","symbol":"$target","arg":"region","in":["services/"],"optional":true},
            {"op":"add_param","path":"$caller.path","symbol":"$caller","name":"region"},
            {"op":"pass_arg","path":"$caller.path","symbol":"$caller","arg":"payload['region']","in":["handlers/"],"optional":true},
        ]));
        let other = vec![("services/svc12.py".to_string(), "serve".to_string())];
        let expanded = expand_template(&template, "core/mod5.py", "compute", &other).unwrap();
        assert_eq!(expanded[0]["path"], "core/mod5.py");
        assert_eq!(expanded[2]["path"], "services/svc12.py");
        assert_eq!(expanded.as_array().unwrap().len(), 4);
    }
}
