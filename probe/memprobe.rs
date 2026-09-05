//! Peak-memory probe. Each scenario must run in its own process: once an
//! earlier scenario frees memory the allocator reuses it, so an RSS delta
//! measured afterwards reads as zero. Peak (VmHWM) in a fresh process is the
//! only honest number.
//!
//! Run: cargo run --release --no-default-features --example memprobe -- <scenario> <xlsx-file>

use formualizer::common::value::LiteralValue;
use formualizer::workbook::backends::CalamineAdapter;
use formualizer::workbook::traits::SpreadsheetReader;
use formualizer::workbook::{LoadStrategy, Workbook, WorkbookConfig};

fn input_path() -> String {
    std::env::args()
        .nth(2)
        .expect("usage: memprobe <scenario> <xlsx-file>")
}

fn peak_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: f64 = rest.trim().split_whitespace().next().unwrap().parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}

// glibc keeps freed memory in its arenas rather than returning it, so resident
// size alone cannot tell live data apart from a spent transient.
extern "C" {
    fn malloc_trim(pad: usize) -> i32;
}

fn trim() {
    unsafe {
        malloc_trim(0);
    }
}

/// Resident set right now, as opposed to the high-water mark.
fn rss_mb() -> f64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f64 = rest.trim().split_whitespace().next().unwrap().parse().unwrap();
            return kb / 1024.0;
        }
    }
    0.0
}

/// Split the topology's cost into what it holds afterwards and what building it
/// briefly needed. Only the first is worth restructuring; a flatter layout does
/// nothing about a transient spike.
fn topo_break() {
    let path = input_path();
    let data = std::fs::read(&path).unwrap();
    let before = rss_mb();
    let topo = formualizer_partitioned::graph::build(&data);
    let after_rss = rss_mb();
    println!("   file {:.1} MB   after build: rss {:.1} MB, peak {:.1} MB",
             data.len() as f64 / 1048576.0, after_rss, peak_mb());
    println!("   resident growth over build: {:.1} MB", after_rss - before);

    trim();
    let trimmed = rss_mb();
    println!("   after returning free memory to the OS: rss {:.1} MB", trimmed);

    let mb = |b: usize| b as f64 / 1048576.0;
    let n_cells = topo.cells.len();
    let n_comp = topo.comp_cells.len();
    let vec_hdr = std::mem::size_of::<Vec<u32>>();
    let inner_cells: usize = topo.comp_cells.iter().map(|v| v.capacity() * 4).sum();
    let n_refs: usize = topo.comp_refs.iter().map(|v| v.len()).sum();
    let inner_refs: usize = topo
        .comp_refs
        .iter()
        .map(|v| v.capacity() * std::mem::size_of::<formualizer_partitioned::graph::RangeRef>())
        .sum();
    println!("   cells      {:>8}      {:>6.2} MB", n_cells,
             mb(n_cells * std::mem::size_of::<formualizer_partitioned::graph::FormulaCell>()));
    println!("   comp_of    {:>8}      {:>6.2} MB", n_cells, mb(n_cells * 4));
    println!("   comp_cells {:>8} comps {:>6.2} MB  (headers {:.2} + data {:.2})",
             n_comp, mb(n_comp * vec_hdr + inner_cells), mb(n_comp * vec_hdr), mb(inner_cells));
    println!("   comp_refs  {:>8} refs  {:>6.2} MB  (headers {:.2} + data {:.2})",
             n_refs, mb(n_comp * vec_hdr + inner_refs), mb(n_comp * vec_hdr), mb(inner_refs));
    println!("   extent+cost{:>8}      {:>6.2} MB", n_comp * 2, mb(n_comp * 16));
    println!("   index      {:>8}      {:>6.2} MB (approx, 2x load factor)",
             topo.index.len(), mb(topo.index.capacity() * 20));
    let text_bytes: usize = topo.texts.iter().map(|t| t.len() + 16).sum();
    let n_ast_refs: usize = topo.ast_refs.iter().map(|v| v.len()).sum();
    println!("   texts      {:>8}      {:>6.2} MB", topo.texts.len(), mb(text_bytes));
    println!("   ast_refs   {:>8} refs  {:>6.2} MB",
             n_ast_refs, mb(n_ast_refs * std::mem::size_of::<formualizer_partitioned::graph::RawRef>()
                            + topo.ast_refs.len() * vec_hdr));
    println!("   -> allocations for the two vector-of-vector fields: {}", n_comp * 2);
    let held = rss_mb();
    drop(topo);
    trim();
    println!("   topology dropped: rss {:.1} -> {:.1} MB, so it held {:.1} MB live",
             held, rss_mb(), held - rss_mb());
}

/// Load and evaluate the whole file, as the current pipeline does.
fn baseline() {
    let data = std::fs::read(input_path()).unwrap();
    let adapter = <CalamineAdapter as SpreadsheetReader>::open_bytes(data).unwrap();
    let mut wb =
        Workbook::from_reader(adapter, LoadStrategy::EagerAll, WorkbookConfig::interactive())
            .unwrap();
    wb.evaluate_all().unwrap();
    std::hint::black_box(wb.get_value("Loaded Report", 200, 1));
}

/// The data floor a partitioned run cannot go below: the adapter stays alive to
/// serve input values to every mini-workbook.
fn adapter_only() {
    let data = std::fs::read(input_path()).unwrap();
    let mut a = <CalamineAdapter as SpreadsheetReader>::open_bytes(data).unwrap();
    let mut n = 0usize;
    for s in a.sheet_names().unwrap() {
        if let Some((r, c)) = a.sheet_bounds(&s) {
            n += a.read_range(&s, (1, 1), (r, c)).unwrap().len();
        }
    }
    std::hint::black_box(n);
}

/// Adapter plus one mini-workbook holding the file's largest component: the
/// 14995-cell chain in column A. This is the peak a partitioned run would hit.
fn mini() {
    let data = std::fs::read(input_path()).unwrap();
    let mut a = <CalamineAdapter as SpreadsheetReader>::open_bytes(data).unwrap();
    let sheet = "Loaded Report";
    let inputs = a.read_range(sheet, (170, 1), (15165, 1)).unwrap();

    let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
    wb.add_sheet(sheet).unwrap();
    if let Some(cd) = inputs.get(&(170, 1)) {
        if let Some(v) = cd.value.clone() {
            wb.set_value(sheet, 170, 1, v).unwrap();
        }
    }
    for r in 171..=15165u32 {
        wb.set_formula(sheet, r, 1, &format!("=A{}", r - 1)).unwrap();
    }
    drop(inputs);
    wb.evaluate_all().unwrap();
    std::hint::black_box(wb.get_value(sheet, 15165, 1));
}

/// Is read_range cheap per call, or does it rescan the sheet? Phase 4 issues
/// one call per referenced range, so a per-call cost proportional to the sheet
/// would dominate everything else.
fn read_cost() {
    let data = std::fs::read(input_path()).unwrap();
    let mut a = <CalamineAdapter as SpreadsheetReader>::open_bytes(data).unwrap();
    let sheet = "Loaded Report";
    for n in [10usize, 200] {
        let t = std::time::Instant::now();
        for i in 0..n {
            let r = 100 + i as u32;
            std::hint::black_box(a.read_range(sheet, (r, 6), (r, 8)).unwrap().len());
        }
        println!(
            "{n:>4} small read_range calls: {:>8.1} ms  ({:.3} ms each)",
            t.elapsed().as_secs_f64() * 1000.0,
            t.elapsed().as_secs_f64() * 1000.0 / n as f64
        );
    }
}

/// The partitioned pipeline measured on the Rust side only, with no Python
/// grid, to separate engine cost from the cost of handing results to Python.
fn partitioned() {
    let data = std::fs::read(input_path()).unwrap();
    let mut topo = formualizer_partitioned::graph::build(&data);
    // values come from the XML reader, so no backend is opened here
    let t = std::time::Instant::now();
    let store = formualizer_partitioned::partition::DataStore::load(&mut topo);
    println!(
        "   store: {} values, {:.2}s, peak so far {:.1} MB",
        store.len(),
        t.elapsed().as_secs_f64(),
        peak_mb()
    );
    let ev = formualizer_partitioned::partition::run(&store, &topo, 32_768).unwrap();
    println!("   batches={} biggest={}", ev.n_batches, ev.biggest_batch_cells);
    std::hint::black_box(&ev.values);
}

/// Cost of each layer on its own, to locate the floor.
fn layers() {
    let data = std::fs::read(input_path()).unwrap();
    println!("   file bytes            peak {:.1} MB", peak_mb());
    let mut topo = formualizer_partitioned::graph::build(&data);
    println!(
        "   + topology ({} cells) peak {:.1} MB",
        topo.cells.len(),
        peak_mb()
    );
    // values come from the XML reader, so no backend is opened here
    let store = formualizer_partitioned::partition::DataStore::load(&mut topo);
    println!("   + store ({} vals)  peak {:.1} MB", store.len(), peak_mb());
    std::hint::black_box(&store);
}

/// Where the store's memory actually goes: the backend's own sheet cache, the
/// map it returns per call, or the values kept afterwards.
fn store_steps() {
    let data = std::fs::read(input_path()).unwrap();
    let mut a = <CalamineAdapter as SpreadsheetReader>::open_bytes(data).unwrap();
    let sheet = "Loaded Report";
    println!("   size_of::<LiteralValue>() = {}", std::mem::size_of::<LiteralValue>());
    println!("   after open                 peak {:.1} MB", peak_mb());
    std::hint::black_box(a.read_range(sheet, (1, 1), (1, 12)).unwrap().len());
    println!("   after 1 tiny read_range    peak {:.1} MB", peak_mb());

    let mut kept: std::collections::HashMap<(u32, u32), LiteralValue> = std::collections::HashMap::new();
    let mut r = 1u32;
    while r <= 15163 {
        let end = 15163u32.min(r + 2729);
        let m = a.read_range(sheet, (r, 1), (end, 12)).unwrap();
        let returned = m.len();
        for ((rr, cc), cd) in m {
            if let Some(v) = cd.value {
                if !matches!(v, LiteralValue::Empty) {
                    kept.insert((rr, cc), v);
                }
            }
        }
        println!(
            "   rows {r:>6}..{end:<6} returned {returned:>6} kept {:>6}  peak {:.1} MB",
            kept.len(),
            peak_mb()
        );
        r = end + 1;
    }
    std::hint::black_box(&kept);
}

/// Compare the backend's two read paths on one cell. `read_range` and
/// `read_sheet` are separate code paths in the backend and need not agree.
fn inline_str() {
    let path = input_path();
    let data = std::fs::read(&path).unwrap();
    let mut a = <CalamineAdapter as SpreadsheetReader>::open_bytes(data).unwrap();
    let sheet = std::env::args().nth(3).unwrap_or_else(|| "Calculation".to_string());
    let (r, c) = (12u32, 9u32);

    let via_range = a.read_range(&sheet, (r, c), (r, c)).unwrap();
    println!("   read_range({sheet}!R{r}C{c}) -> {:?}", via_range.get(&(r, c)).map(|d| &d.value));

    let sd = a.read_sheet(&sheet).unwrap();
    let found = sd.cells.iter().find(|(k, _)| **k == (r, c)).map(|(_, v)| &v.value);
    println!("   read_sheet cells={} -> {:?}", sd.cells.len(), found);
}

/// Trace one cell through the partitioned pipeline: topology sheet mapping,
/// what the store kept, and whether the cell was claimed as a formula.
fn trace_cell() {
    let path = input_path();
    let sheet = std::env::args().nth(3).unwrap();
    let r: u32 = std::env::args().nth(4).unwrap().parse().unwrap();
    let c: u32 = std::env::args().nth(5).unwrap().parse().unwrap();
    let data = std::fs::read(&path).unwrap();
    let mut topo = formualizer_partitioned::graph::build(&data);
    println!("   topo sheets: {:?}", topo.sheets.iter().map(|s| (&s.name, s.max_row, s.max_col)).collect::<Vec<_>>());
    // values come from the XML reader, so no backend is opened here
    let si = topo.sheets.iter().position(|s| s.name == sheet).unwrap() as u16;
    println!("   sheet index for {sheet:?} = {si}");
    println!("   in topo.index (claimed as formula)? {}", topo.index.contains_key(&(si, r, c)));
    let store = formualizer_partitioned::partition::DataStore::load(&mut topo);
    println!("   store len={} get({si},{r},{c}) = {:?}", store.len(), store.get(si, r, c));
}

/// Print the references a formula yields, to see how odd inputs parse.
fn show_refs() {
    for f in std::env::args().skip(2) {
        let src = if f.starts_with('=') { f.clone() } else { format!("={f}") };
        match formualizer::parse::parser::parse(&src) {
            Ok(ast) => {
                println!("  {src}");
                ast.visit_refs(|rv| println!("      {rv:?}"));
            }
            Err(e) => println!("  {src}  PARSE ERROR {e:?}"),
        }
    }
}

/// Does the incremental edit path accept a formula that names its own cell?
/// `ROW(A7)` and `CELL("filename",Q2)` take a reference for its address, not
/// its value, and files in the wild contain them.
fn self_ref() {
    for f in ["=ROW(A7)-6", "=A7+1", "=SUM(A1:A7)", "=B7+1"] {
        let mut wb = Workbook::new_with_config(WorkbookConfig::ephemeral());
        wb.add_sheet("S").unwrap();
        match wb.set_formula("S", 7, 1, f) {
            Ok(()) => println!("   A7 {f:<14} set ok, eval={:?}", wb.evaluate_all().err().map(|e| e.to_string())),
            Err(e) => println!("   A7 {f:<14} REJECTED: {e}"),
        }
    }
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_default();
    let t = std::time::Instant::now();
    match which.as_str() {
        "baseline" => baseline(),
        "adapter" => adapter_only(),
        "mini" => mini(),
        "read" => read_cost(),
        "partitioned" => partitioned(),
        "layers" => layers(),
        "topobreak" => topo_break(),
        "storesteps" => store_steps(),
        "inline" => inline_str(),
        "trace" => trace_cell(),
        "refs" => show_refs(),
        "selfref" => self_ref(),
        _ => {
            eprintln!("scenarios: baseline | adapter | mini | read | partitioned");
            return;
        }
    }
    println!(
        "{which:<10} peak={:>7.1} MB  {:.2}s",
        peak_mb(),
        t.elapsed().as_secs_f64()
    );
}
