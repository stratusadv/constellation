//! Benchmark for `rank_by_structure`, the personalized-PageRank power iteration
//! that runs on every `explore` call over the whole graph. The hottest per-query
//! path in the MCP server; this bench is the one to watch when changing the
//! ranking buffers or the adjacency representation.

use constellation_mcp::rank_by_structure;

fn main() {
    divan::main();
}

/// A deterministic adjacency list of `count` nodes: each node links to two
/// neighbours at fixed strides, giving a connected graph with a realistic
/// average degree (no RNG, so the graph is identical across runs).
fn build_adjacency(count: usize) -> Vec<Vec<u32>> {
    let mut adjacency: Vec<Vec<u32>> = vec![Vec::new(); count];

    for node in 0..count {
        for stride in [1usize, 7] {
            let neighbour = ((node + stride) % count) as u32;

            adjacency[node].push(neighbour);
            adjacency[neighbour as usize].push(node as u32);
        }
    }

    adjacency
}

fn seeds(count: usize) -> Vec<usize> {
    vec![0, count / 3, (count * 2) / 3, count - 1]
}

#[divan::bench(args = [1_000usize, 10_000, 50_000])]
fn rank(bencher: divan::Bencher, count: usize) {
    bencher
        .with_inputs(|| (build_adjacency(count), seeds(count)))
        .bench_values(|(adjacency, seeds)| rank_by_structure(&seeds, &adjacency));
}
