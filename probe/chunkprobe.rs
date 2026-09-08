//! Where the chunk path spends time, phase by phase.
//! Run: cargo run --release --manifest-path probe/Cargo.toml --bin chunkprobe -- FILE...

use std::time::Instant;

use formualizer::workbook::backends::CalamineAdapter;
use formualizer::workbook::traits::SpreadsheetReader;
use formualizer::workbook::{LoadStrategy, Workbook, WorkbookConfig};

use formualizer_partitioned::graph;
use formualizer_partitioned::partition::{self, DataStore};
use formualizer_partitioned::prelude;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    for path in std::env::args().skip(1) {
        let data = std::fs::read(&path).unwrap();

        let t = Instant::now();
        let mut src = graph::read(&data);
        prelude::fold_and_rewrite(&data, &mut src);
        let mut topo = graph::build_from(src);
        let t_topo = ms(t);

        let t = Instant::now();
        let plan = partition::plan_scratch(
            &topo,
            partition::DEFAULT_CHUNK_ROWS,
            partition::DEFAULT_LOOKUP_BUDGET,
        );
        let t_plan = ms(t);

        let t = Instant::now();
        let store = DataStore::reread(&data, &topo).unwrap();
        let t_store = ms(t);

        let t = Instant::now();
        let run = plan
            .as_ref()
            .map(|p| partition::run_scratch(&store, &topo, p))
            .unwrap_or_else(|e| Err(e.to_string()));
        let t_run = ms(t);

        let t = Instant::now();
        let adapter =
            <CalamineAdapter as SpreadsheetReader>::open_bytes(data.clone()).unwrap();
        let mut whole =
            Workbook::from_reader(adapter, LoadStrategy::EagerAll, WorkbookConfig::interactive())
                .unwrap();
        whole.evaluate_all().unwrap();
        let t_whole = ms(t);

        let name = path.rsplit('/').next().unwrap_or(&path);
        println!("{}", &name[..name.len().min(60)]);
        println!("  formulas={} extent={} plan={:?}", topo.cells.len(), topo.full_extent_cells, plan.as_ref().map(|p| (p.first_row, p.last_row, p.carry, p.n_chunks())));
        println!("  topo {t_topo:>8.1} ms   plan {t_plan:>8.1} ms   store {t_store:>8.1} ms   run {t_run:>8.1} ms   whole {t_whole:>8.1} ms");
        let _ = run;
    }
}
