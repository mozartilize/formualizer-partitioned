//! List the formulas whose graph references are unsupported.
//! Run: cargo run --release --manifest-path probe/Cargo.toml --bin refprobe -- file.xlsx

use formualizer_partitioned::graph::{self, RawRef};
use std::env;

fn main() {
    let path = env::args().nth(1).expect("usage: refprobe <file.xlsx>");
    let data = std::fs::read(path).expect("read xlsx");
    let src = graph::read(&data);
    for name in &src.defined_names {
        if name.target.is_none() {
            println!("NAME {:?}: unsupported", name.name);
        }
    }
    let topo = graph::build_from(src);
    for name in &topo.static_names {
        let scope = match name.scope {
            graph::NameScope::Workbook => "workbook".to_string(),
            graph::NameScope::Sheet(sheet) => format!("sheet:{sheet}"),
            graph::NameScope::Invalid => "invalid".to_string(),
        };
        println!("STATIC {:?} scope={scope} target={:?}", name.name, name.target);
    }
    for cell in &topo.cells {
        let refs = &topo.ast_refs[cell.ast as usize];
        if refs.iter().any(|r| matches!(r, RawRef::Unsupported)) {
            println!(
                "{}!{}:{} {}",
                topo.sheets[cell.sheet as usize].name,
                cell.row,
                cell.col,
                topo.texts[cell.ast as usize]
            );
        }
    }
}
