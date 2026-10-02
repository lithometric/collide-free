//! Collide's parse hot path, in Rust.
//!
//! Optional by design: the Python server imports this when it is installed
//! and falls back to its own engine when it is not, so a deploy without the
//! extension behaves identically — only slower.

pub mod community;
pub mod edit;
pub mod engine;
pub mod graph;
pub mod langs;
pub mod local;

#[cfg(feature = "python")]
mod python_api {
use crate::{community, engine, graph, langs};
use pyo3::prelude::*;
use pyo3::types::PyList;

type PySymbol = (String, String, String, String, Vec<String>, Vec<(String, String, String)>, (u32, u32), Vec<String>, Vec<(String, u32, String)>, String);
type PyImport = (String, Vec<(String, String)>);
type PyParse = (String, String, Vec<PySymbol>, Vec<PyImport>, String);

/// `(status, language, symbols, imports, doc)` or `None` for an unknown extension.
/// Symbols are `(name, kind, signature, hash, refs, edges)` in declaration
/// order; imports are `(module, [(local, original)])`.
#[pyfunction]
fn parse(py: Python<'_>, path: String, content: String) -> Option<PyParse> {
    // the parse itself touches no Python objects, so the GIL goes back to
    // the other agents' requests while tree-sitter works
    let parsed = py.allow_threads(move || engine::parse(&path, &content))?;
    Some((
        parsed.status.to_string(),
        parsed.language.to_string(),
        parsed
            .symbols
            .into_iter()
            .map(|s| (s.name, s.kind, s.signature, s.hash, s.refs, s.edges, s.span, s.params, s.sites, s.doc))
            .collect(),
        parsed.imports.into_iter().map(|i| (i.module, i.names)).collect(),
        parsed.doc,
    ))
}

/// Louvain communities over a weighted edge list.
///
/// `edges` is [(node, node, weight)]; `isolated` names nodes with no edges so
/// they still appear as singletons. Returns one sorted list of node names per
/// community, largest first. Deterministic for a given seed.
#[pyfunction]
#[pyo3(signature = (edges, isolated=Vec::new(), resolution=1.0, seed=7))]
fn louvain(
    py: Python<'_>,
    edges: Vec<(String, String, f64)>,
    isolated: Vec<String>,
    resolution: f64,
    seed: u64,
) -> Vec<Vec<String>> {
    py.allow_threads(move || community::louvain(&edges, resolution, seed, &isolated))
}

/// Modularity of a partition — how good a clustering is, on the same scale
/// NetworkX reports, so the two can be compared directly.
#[pyfunction]
#[pyo3(signature = (edges, communities, resolution=1.0))]
fn modularity(
    py: Python<'_>,
    edges: Vec<(String, String, f64)>,
    communities: Vec<Vec<String>>,
    resolution: f64,
) -> f64 {
    py.allow_threads(move || community::modularity_named(&edges, &communities, resolution))
}

/// Resolve one file's raw name-level edges into cross-file graph edges.
///
/// JSON in, JSON out, because the shape is nested and this runs once per
/// reported file rather than in a loop — the conversion cost is noise next to
/// the parse that precedes it, and a single shape keeps the two
/// implementations comparable field for field.
///
/// Input:  {path, language, symbols: {name: {edges: [[target, member, kind]]}},
///          imports: [{module, names: [[local, original]]}],
///          known: [path], name_index: {name: [path]}}
/// Output: [{from, to, kind, confidence}]
#[pyfunction]
fn resolve_edges(py: Python<'_>, request: String) -> PyResult<String> {
    use std::collections::{BTreeMap, BTreeSet};

    py.allow_threads(move || {
        let parsed: serde_json::Value = serde_json::from_str(&request)
            .map_err(|error| pyo3::exceptions::PyValueError::new_err(error.to_string()))?;
        let text = |key: &str| parsed.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let path = text("path");
        let language = text("language");

        let known: BTreeSet<String> = parsed
            .get("known")
            .and_then(|v| v.as_array())
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();

        let name_index: BTreeMap<String, Vec<String>> = parsed
            .get("name_index")
            .and_then(|v| v.as_object())
            .map(|map| {
                map.iter()
                    .map(|(name, paths)| {
                        let paths = paths
                            .as_array()
                            .map(|items| {
                                items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
                            })
                            .unwrap_or_default();
                        (name.clone(), paths)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let imports: Vec<graph::ImportSpec> = parsed
            .get("imports")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .map(|item| graph::ImportSpec {
                        module: item.get("module").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        names: item
                            .get("names")
                            .and_then(|v| v.as_array())
                            .map(|pairs| {
                                pairs
                                    .iter()
                                    .filter_map(|pair| {
                                        let pair = pair.as_array()?;
                                        Some((
                                            pair.first()?.as_str()?.to_string(),
                                            pair.get(1)?.as_str()?.to_string(),
                                        ))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // the borrow checker wants the edge tuples to outlive the view of them
        let raw: Vec<(String, Vec<(String, String, String)>)> = parsed
            .get("symbols")
            .and_then(|v| v.as_object())
            .map(|map| {
                map.iter()
                    .map(|(name, entry)| {
                        let edges = entry
                            .get("edges")
                            .and_then(|v| v.as_array())
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(|edge| {
                                        let edge = edge.as_array()?;
                                        Some((
                                            edge.first()?.as_str()?.to_string(),
                                            edge.get(1)?.as_str()?.to_string(),
                                            edge.get(2)?.as_str()?.to_string(),
                                        ))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        (name.clone(), edges)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let symbols: Vec<graph::SymbolEdges> = raw
            .iter()
            .map(|(name, edges)| graph::SymbolEdges { name, edges })
            .collect();

        let edges = graph::resolve_edges(
            &path, &language, &symbols, &imports, &known, &name_index,
        );
        let out: Vec<serde_json::Value> = edges
            .into_iter()
            .map(|edge| {
                serde_json::json!({
                    "from": edge.from, "to": edge.to,
                    "kind": edge.kind, "confidence": edge.confidence,
                })
            })
            .collect();
        Ok(serde_json::to_string(&out).unwrap_or_default())
    })
}

/// Extensions this core can parse; the Python side uses it to decide which
/// files take the Rust path and which fall back.
#[pyfunction]
fn supported_extensions(py: Python<'_>) -> Bound<'_, PyList> {
    let exts: Vec<&str> = langs::specs().iter().flat_map(|spec| spec.exts.iter().copied()).collect();
    PyList::new_bound(py, exts)
}

#[pymodule]
fn collide_core(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(parse, module)?)?;
    module.add_function(wrap_pyfunction!(supported_extensions, module)?)?;
    module.add_function(wrap_pyfunction!(louvain, module)?)?;
    module.add_function(wrap_pyfunction!(resolve_edges, module)?)?;
    module.add_function(wrap_pyfunction!(modularity, module)?)?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
}
