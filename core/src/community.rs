//! Louvain community detection.
//!
//! This is the most expensive thing Collide computes. `repo_map` clusters the
//! symbol graph into subsystems, and `repo_map` is folded into every briefing,
//! which is the first call every agent makes. The partition is cached by graph
//! fingerprint — but the fingerprint moves on every reported edit, so on a
//! busy repo the cache is cold exactly when the most agents are working.
//! Measured on a synthetic 5,000-file graph, NetworkX takes 1.56s; this takes
//! a fraction of that, which is the difference between a briefing that feels
//! instant and one that stalls.
//!
//! Standard two-phase Louvain: local moving to a modularity optimum, then
//! aggregate each community into a super-node and repeat. Node visiting order
//! is shuffled with a seeded PRNG so a given seed always yields the same
//! partition — a clustering that jitters between calls would make the
//! dashboard's subsystems unreadable.

use std::collections::HashMap;

/// xorshift64*: deterministic, tiny, and good enough to break the order bias
/// that makes naive Louvain unstable.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            items.swap(i, j);
        }
    }
}

/// An undirected weighted graph over dense node indices.
struct Graph {
    /// adjacency: node -> [(neighbour, weight)], self-loops included once
    adjacency: Vec<Vec<(usize, f64)>>,
    /// weighted degree, counting a self-loop twice (the standard convention)
    degree: Vec<f64>,
    /// self-loop weight per node, carried through aggregation
    self_loops: Vec<f64>,
    /// total edge weight (sum of all weights, self-loops once)
    total: f64,
}

impl Graph {
    fn from_edges(node_count: usize, edges: &[(usize, usize, f64)]) -> Self {
        let mut adjacency = vec![Vec::new(); node_count];
        let mut degree = vec![0.0; node_count];
        let mut self_loops = vec![0.0; node_count];
        let mut total = 0.0;
        for &(a, b, weight) in edges {
            total += weight;
            if a == b {
                self_loops[a] += weight;
                degree[a] += 2.0 * weight;
                adjacency[a].push((a, weight));
            } else {
                degree[a] += weight;
                degree[b] += weight;
                adjacency[a].push((b, weight));
                adjacency[b].push((a, weight));
            }
        }
        Graph { adjacency, degree, self_loops, total }
    }

    fn len(&self) -> usize {
        self.adjacency.len()
    }
}

/// One pass of local moving. Returns the community label per node.
fn local_moving(graph: &Graph, resolution: f64, rng: &mut Rng) -> Vec<usize> {
    let n = graph.len();
    let mut community: Vec<usize> = (0..n).collect();
    let mut community_total: Vec<f64> = graph.degree.clone();
    if graph.total <= 0.0 {
        return community;
    }
    let m2 = 2.0 * graph.total;

    let mut order: Vec<usize> = (0..n).collect();
    rng.shuffle(&mut order);

    // weight from the node being considered into each candidate community
    let mut weights: HashMap<usize, f64> = HashMap::new();
    let mut improved = true;
    let mut sweeps = 0;
    while improved && sweeps < 32 {
        improved = false;
        sweeps += 1;
        for &node in &order {
            let own = community[node];
            let degree = graph.degree[node];

            weights.clear();
            for &(neighbour, weight) in &graph.adjacency[node] {
                if neighbour == node {
                    continue; // a self-loop never argues for a move
                }
                *weights.entry(community[neighbour]).or_insert(0.0) += weight;
            }

            // take the node out of its community before scoring alternatives
            community_total[own] -= degree;
            let stay = weights.get(&own).copied().unwrap_or(0.0)
                - resolution * community_total[own] * degree / m2;

            let mut best_community = own;
            let mut best_gain = stay;
            for (&candidate, &weight) in &weights {
                if candidate == own {
                    continue;
                }
                let gain = weight - resolution * community_total[candidate] * degree / m2;
                // ties go to the lower index so the result does not depend on
                // HashMap iteration order
                if gain > best_gain || (gain == best_gain && candidate < best_community) {
                    best_gain = gain;
                    best_community = candidate;
                }
            }

            community_total[best_community] += degree;
            if best_community != own {
                community[node] = best_community;
                improved = true;
            }
        }
    }
    community
}

/// Relabel communities to a dense 0..k range, preserving first-seen order.
fn densify(labels: &[usize]) -> (Vec<usize>, usize) {
    let mut mapping: HashMap<usize, usize> = HashMap::new();
    let mut dense = Vec::with_capacity(labels.len());
    for &label in labels {
        let next = mapping.len();
        let id = *mapping.entry(label).or_insert(next);
        dense.push(id);
    }
    (dense, mapping.len())
}

/// Collapse each community into one node, summing edge weights.
fn aggregate(graph: &Graph, labels: &[usize], count: usize) -> Graph {
    let mut merged: HashMap<(usize, usize), f64> = HashMap::new();
    for node in 0..graph.len() {
        let a = labels[node];
        if graph.self_loops[node] > 0.0 {
            *merged.entry((a, a)).or_insert(0.0) += graph.self_loops[node];
        }
        for &(neighbour, weight) in &graph.adjacency[node] {
            if neighbour == node {
                continue; // already counted as a self-loop
            }
            let b = labels[neighbour];
            if a == b {
                // each internal edge is seen from both ends: half of the pair
                *merged.entry((a, a)).or_insert(0.0) += weight / 2.0;
            } else if node < neighbour {
                let key = if a < b { (a, b) } else { (b, a) };
                *merged.entry(key).or_insert(0.0) += weight;
            }
        }
    }
    let edges: Vec<(usize, usize, f64)> =
        merged.into_iter().map(|((a, b), weight)| (a, b, weight)).collect();
    Graph::from_edges(count, &edges)
}

/// Modularity of a partition — used by the parity test to prove this finds a
/// partition at least as good as NetworkX's, since Louvain is heuristic and
/// two correct implementations need not agree node for node.
pub fn modularity(edges: &[(usize, usize, f64)], labels: &[usize], resolution: f64) -> f64 {
    let node_count = labels.len();
    let graph = Graph::from_edges(node_count, edges);
    if graph.total <= 0.0 {
        return 0.0;
    }
    let m2 = 2.0 * graph.total;
    let community_count = labels.iter().copied().max().map(|m| m + 1).unwrap_or(0);
    let mut inside = vec![0.0; community_count];
    let mut total = vec![0.0; community_count];
    for node in 0..node_count {
        total[labels[node]] += graph.degree[node];
    }
    for &(a, b, weight) in edges {
        if labels[a] == labels[b] {
            inside[labels[a]] += if a == b { weight } else { 2.0 * weight };
        }
    }
    (0..community_count)
        .map(|c| inside[c] / m2 - resolution * (total[c] / m2).powi(2))
        .sum()
}

/// Louvain over a named edge list. Returns one vector of node names per
/// community, each sorted, communities ordered largest first.
pub fn louvain(
    named_edges: &[(String, String, f64)],
    resolution: f64,
    seed: u64,
    isolated: &[String],
) -> Vec<Vec<String>> {
    // intern names to dense indices; isolated nodes carry no edges but must
    // still appear in the partition as singleton communities
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut names: Vec<String> = Vec::new();
    fn intern(name: &str, index: &mut HashMap<String, usize>, names: &mut Vec<String>) -> usize {
        if let Some(&id) = index.get(name) {
            return id;
        }
        let id = names.len();
        names.push(name.to_string());
        index.insert(name.to_string(), id);
        id
    }

    let mut edges: Vec<(usize, usize, f64)> = Vec::with_capacity(named_edges.len());
    for (a, b, weight) in named_edges {
        let ai = intern(a, &mut index, &mut names);
        let bi = intern(b, &mut index, &mut names);
        edges.push((ai, bi, *weight));
    }
    for name in isolated {
        intern(name, &mut index, &mut names);
    }
    let node_count = names.len();
    if node_count == 0 {
        return Vec::new();
    }

    let mut rng = Rng::new(seed);
    let mut graph = Graph::from_edges(node_count, &edges);
    // membership of each ORIGINAL node, rewritten at every level
    let mut membership: Vec<usize> = (0..node_count).collect();

    for _level in 0..24 {
        let labels = local_moving(&graph, resolution, &mut rng);
        let (dense, count) = densify(&labels);
        if count == graph.len() {
            break; // nothing merged: this level is already optimal
        }
        for node in membership.iter_mut() {
            *node = dense[*node];
        }
        graph = aggregate(&graph, &dense, count);
    }

    let community_count = membership.iter().copied().max().map(|m| m + 1).unwrap_or(0);
    let mut buckets: Vec<Vec<String>> = vec![Vec::new(); community_count];
    for (node, &community) in membership.iter().enumerate() {
        buckets[community].push(names[node].clone());
    }
    buckets.retain(|bucket| !bucket.is_empty());
    for bucket in buckets.iter_mut() {
        bucket.sort();
    }
    buckets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.first().cmp(&b.first())));
    buckets
}

/// Modularity of a named partition, for callers holding names rather than
/// indices (the parity test, and anything comparing two clusterings).
pub fn modularity_named(
    named_edges: &[(String, String, f64)],
    communities: &[Vec<String>],
    resolution: f64,
) -> f64 {
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut labels: Vec<usize> = Vec::new();
    for (community_id, members) in communities.iter().enumerate() {
        for name in members {
            let id = labels.len();
            index.insert(name.clone(), id);
            labels.push(community_id);
        }
    }
    let mut edges: Vec<(usize, usize, f64)> = Vec::with_capacity(named_edges.len());
    for (a, b, weight) in named_edges {
        let (Some(&ai), Some(&bi)) = (index.get(a), index.get(b)) else { continue };
        edges.push((ai, bi, *weight));
    }
    modularity(&edges, &labels, resolution)
}
