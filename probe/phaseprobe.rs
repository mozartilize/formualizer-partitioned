//! Where the component path spends time and memory, phase by phase.
//!
//! Run: cargo run --release --no-default-features --example phaseprobe -- FILE...

use std::time::Instant;

use formualizer::workbook::backends::CalamineAdapter;
use formualizer::workbook::traits::SpreadsheetReader;
use formualizer::workbook::{LoadStrategy, Workbook, WorkbookConfig};

use formualizer_partitioned::graph;
use formualizer_partitioned::partition::{plan_batches, DataStore, DEFAULT_BUDGET_CELLS};

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Resident set size in MB, read from /proc.
fn rss() -> f64 {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: f64 = s.split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0.0);
    pages * 4096.0 / 1024.0 / 1024.0
}

fn main() {
    for path in std::env::args().skip(1) {
        let data = std::fs::read(&path).unwrap();
        let start_rss = rss();

        let t = Instant::now();
        let mut topo = graph::build(&data);
        let t_build = ms(t);
        let rss_build = rss();

        let t = Instant::now();
        let store = DataStore::load(&mut topo);
        let t_store = ms(t);
        let rss_store = rss();

        let batches = plan_batches(&topo, DEFAULT_BUDGET_CELLS);

        let store_len = store.len();
        let t = Instant::now();
        let ev = formualizer_partitioned::partition::run(&store, &topo, DEFAULT_BUDGET_CELLS).unwrap();
        let t_run = ms(t);
        let rss_run = rss();

        drop(ev);
        drop(store);

        let t = Instant::now();
        let adapter = <CalamineAdapter as SpreadsheetReader>::open_bytes(data.clone()).unwrap();
        let mut whole =
            Workbook::from_reader(adapter, LoadStrategy::EagerAll, WorkbookConfig::interactive())
                .unwrap();
        whole.evaluate_all().unwrap();
        let t_whole = ms(t);

        let name = path.rsplit('/').next().unwrap_or(&path);
        println!("{}", &name[..name.len().min(60)]);
        println!(
            "  shape    {} formulas, {} sources, {} components, {} batches",
            topo.cells.len(),
            topo.texts.len(),
            topo.comp_cells.len(),
            batches.len()
        );
        println!(
            "  build    {t_build:>8.1} ms   +{:>6.1} MB",
            rss_build - start_rss
        );
        println!(
            "  store    {t_store:>8.1} ms   +{:>6.1} MB   {} values kept",
            rss_store - rss_build,
            store_len
        );
        println!("  run      {t_run:>8.1} ms   +{:>6.1} MB", rss_run - rss_store);
        println!(
            "  whole    {t_whole:>8.1} ms                 ratio {:.2}x",
            (t_build + t_store + t_run) / t_whole
        );
    }
}
