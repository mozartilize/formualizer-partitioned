//! Which components exceed the formula-text budget that routes them cell by cell.
//!
//! Run: cargo run --release --manifest-path probe/Cargo.toml --bin budgetprobe -- FILE...

use formualizer_partitioned::graph;

/// Must track `COMPONENT_TEXT_BUDGET` in `src/partition.rs`. That constant is
/// private, so this probe cannot read it.
const BUDGET: usize = 1_000_000;

fn main() {
    println!(
        "{:<52} {:>8} {:>7} {:>12} {:>7} {:>12}",
        "file", "formulas", "comps", "largest_text", "over", "over_cells"
    );
    for path in std::env::args().skip(1) {
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                println!("{path}: unreadable: {e}");
                continue;
            }
        };
        let topo = graph::build(&data);

        let mut largest = 0usize;
        let mut over = 0usize;
        let mut over_cells = 0usize;
        for comp in &topo.comp_cells {
            let bytes: usize = comp
                .iter()
                .map(|&i| topo.texts[topo.cells[i as usize].ast as usize].len())
                .sum();
            largest = largest.max(bytes);
            if bytes > BUDGET {
                over += 1;
                over_cells += comp.len();
            }
        }

        let name = path.rsplit('/').next().unwrap_or(&path);
        println!(
            "{:<52} {:>8} {:>7} {:>12} {:>7} {:>12}",
            &name[..name.len().min(52)],
            topo.cells.len(),
            topo.comp_cells.len(),
            largest,
            over,
            over_cells
        );
    }
}
