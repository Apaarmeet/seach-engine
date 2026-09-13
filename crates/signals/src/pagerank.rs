//! PageRank over the crawled link graph.
//!
//! Batch by design: authority is a global property of the graph, so it's
//! recomputed periodically (hours/days), never per query. At web scale this
//! step itself becomes distributed (Pregel/GraphX shaped); here it's a
//! single in-memory power iteration.

use std::collections::HashMap;

pub struct Graph {
    pub nodes: Vec<String>,
    pub index: HashMap<String, usize>,
    pub out_edges: Vec<Vec<usize>>,
}

impl Graph {
    /// Build from (page_url, outlink_urls) pairs. Links pointing outside the
    /// crawled set are dropped — there's no node on the other end to rank.
    pub fn build<I, S>(pages: I) -> Self
    where
        I: IntoIterator<Item = (String, Vec<S>)>,
        S: AsRef<str>,
    {
        let mut nodes = Vec::new();
        let mut index = HashMap::new();
        let mut raw: Vec<Vec<String>> = Vec::new();

        for (url, links) in pages {
            let id = *index.entry(url.clone()).or_insert_with(|| {
                nodes.push(url.clone());
                raw.push(Vec::new());
                nodes.len() - 1
            });
            raw[id] = links.into_iter().map(|l| l.as_ref().to_string()).collect();
        }

        let n = nodes.len();
        let mut out_edges = vec![Vec::new(); n];
        for (i, links) in raw.iter().enumerate() {
            let mut seen = std::collections::HashSet::new();
            for link in links {
                if let Some(&j) = index.get(link) {
                    // Dedup parallel edges: 50 links from one page to another
                    // shouldn't transfer 50x the authority.
                    if j != i && seen.insert(j) {
                        out_edges[i].push(j);
                    }
                }
            }
        }

        Graph { nodes, index, out_edges }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

/// Power-iterate to convergence (or `iterations`, whichever comes first).
pub fn compute(graph: &Graph, damping: f64, iterations: usize) -> Vec<f32> {
    let n = graph.len();
    if n == 0 {
        return Vec::new();
    }

    let d = damping;
    let mut rank = vec![1.0 / n as f64; n];

    for _ in 0..iterations {
        let mut next = vec![(1.0 - d) / n as f64; n];

        // Dangling nodes (no in-corpus out-edges) would otherwise leak their
        // rank out of the system each iteration; redistribute it evenly so
        // the vector stays a probability distribution.
        let dangling: f64 = (0..n)
            .filter(|&i| graph.out_edges[i].is_empty())
            .map(|i| rank[i])
            .sum();
        let dangling_share = d * dangling / n as f64;

        for i in 0..n {
            let edges = &graph.out_edges[i];
            if edges.is_empty() {
                continue;
            }
            let share = d * rank[i] / edges.len() as f64;
            for &j in edges {
                next[j] += share;
            }
        }
        for v in next.iter_mut() {
            *v += dangling_share;
        }

        let delta: f64 = rank.iter().zip(&next).map(|(a, b)| (a - b).abs()).sum();
        rank = next;
        if delta < 1e-10 {
            break;
        }
    }

    rank.into_iter().map(|v| v as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_outranks_leaf() {
        // b is linked by a and c; it should outrank both.
        let graph = Graph::build(vec![
            ("a".to_string(), vec!["b"]),
            ("c".to_string(), vec!["b"]),
            ("b".to_string(), vec!["a"]),
        ]);
        let r = compute(&graph, 0.85, 100);
        let b = r[graph.index["b"]];
        let c = r[graph.index["c"]];
        assert!(b > c, "hub {b} should outrank leaf {c}");
    }

    #[test]
    fn ranks_sum_to_one() {
        let graph = Graph::build(vec![
            ("a".to_string(), vec!["b", "c"]),
            ("b".to_string(), vec!["c"]),
            ("c".to_string(), Vec::<&str>::new()), // dangling
        ]);
        let total: f32 = compute(&graph, 0.85, 200).iter().sum();
        assert!((total - 1.0).abs() < 1e-4, "ranks summed to {total}, expected 1.0");
    }

    #[test]
    fn parallel_edges_do_not_multiply_authority() {
        let many = Graph::build(vec![
            ("a".to_string(), vec!["b", "b", "b", "b", "c"]),
            ("b".to_string(), Vec::<&str>::new()),
            ("c".to_string(), Vec::<&str>::new()),
        ]);
        let r = compute(&many, 0.85, 100);
        // b and c each got one distinct edge from a, so they tie.
        assert!((r[many.index["b"]] - r[many.index["c"]]).abs() < 1e-6);
    }
}
