//! Streaming xlsx reader + dependency-component extraction.
//!
//! Reads sheet XML directly instead of going through the engine's per-cell
//! public API. Each `Workbook::get_formula` call re-renders an AST to canonical
//! text, which costs far more than streaming the same sheet.
//!
//! Formulas are parsed with `formualizer-parse`, the same parser the engine
//! uses, so reference grammar (quoted sheet names, absolute anchors, ranges,
//! 3D refs) is handled exactly rather than approximated by a regex.
//!
//! A shared formula (`<f t="shared" si="N" ref="A2:A40">`) is parsed once at its
//! master anchor; every member cell reuses that AST and shifts relative
//! references by its offset from the anchor. That keeps parse count at the
//! number of distinct formulas rather than the number of formula cells.
//!
//! # Before optimizing the containers in this file
//!
//! Building the topology is 5-10% of a partitioned run. Measured with
//! `probe/phaseprobe.rs`: 390 ms of build against 7540 ms of evaluation on a
//! 265,587-formula workbook, and 26.5 ms against 159.7 ms on an
//! 8,378-formula one. That time goes to inflating and tokenizing sheet XML,
//! parsing, and walking ASTs, then to formula placement and evaluation inside
//! the engine. It does not go to the maps and sets below.
//!
//! So replacing a container here with a faster one cannot move the total by
//! 1%, even if the replacement were free. Two such replacements were tried
//! and measured slower; see the comments at `dense` and `boxes` in
//! `build_from`. Profile the run phase and target formula placement before
//! changing anything in this file for speed.

use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, Read};

use crate::values::{decode_escapes, decode_text, raw_attr, read_text_element, Val, Values};
use formualizer::LiteralValue;
use formualizer::parse::parser::{parse, ASTNode, ASTNodeType, ReferenceType};
use quick_xml::events::Event;
use quick_xml::Reader;
use zip::ZipArchive;

/// Disjoint-set union with union-by-rank and path halving. Near-O(alpha(n)) per
/// operation; builds weakly-connected components of the formula dependency
/// graph.
pub struct Dsu {
    parent: Vec<u32>,
    rank: Vec<u8>,
}

impl Dsu {
    pub fn new(n: usize) -> Self {
        Dsu {
            parent: (0..n as u32).collect(),
            rank: vec![0; n],
        }
    }

    pub fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            let gp = self.parent[self.parent[x as usize] as usize];
            self.parent[x as usize] = gp;
            x = gp;
        }
        x
    }

    pub fn union(&mut self, a: u32, b: u32) {
        let (mut ra, mut rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        if self.rank[ra as usize] < self.rank[rb as usize] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb as usize] = ra;
        if self.rank[ra as usize] == self.rank[rb as usize] {
            self.rank[ra as usize] += 1;
        }
    }
}

/// Inclusive bounding box in 1-based (row, col) space.
#[derive(Clone, Copy)]
pub struct BBox {
    pub min_r: u32,
    pub min_c: u32,
    pub max_r: u32,
    pub max_c: u32,
}

impl BBox {
    fn point(r: u32, c: u32) -> Self {
        BBox { min_r: r, min_c: c, max_r: r, max_c: c }
    }
    fn expand(&mut self, r: u32, c: u32) {
        self.min_r = self.min_r.min(r);
        self.min_c = self.min_c.min(c);
        self.max_r = self.max_r.max(r);
        self.max_c = self.max_c.max(c);
    }
    fn merge(&mut self, o: &BBox) {
        self.expand(o.min_r, o.min_c);
        self.expand(o.max_r, o.max_c);
    }
    pub fn cells(&self) -> u64 {
        (self.max_r - self.min_r + 1) as u64 * (self.max_c - self.min_c + 1) as u64
    }
}

/// One formula cell. `ast` indexes the deduplicated AST table; `dr`/`dc` is the
/// cell's offset from the anchor that AST was parsed at (zero unless this is a
/// shared-formula member).
pub struct FormulaCell {
    pub sheet: u16,
    pub row: u32,
    pub col: u32,
    pub ast: u32,
    pub dr: i64,
    pub dc: i64,
}

pub struct SheetInfo {
    pub name: String,
    pub max_row: u32,
    pub max_col: u32,
    /// Extent of the cells that actually carry a value or a formula.
    ///
    /// `max_row`/`max_col` follow what the engine reports, which includes the
    /// declared `<dimension>` and cells that exist only to carry a style. A
    /// consumer writing one database row per sheet row wants neither, because
    /// that gap can reach tens of thousands of empty rows on one sheet.
    pub data_row: u32,
    pub data_col: u32,
    /// Whether the cells of the part appear in ascending row order.
    ///
    /// The row stream returns rows in the order the part holds them, so it can
    /// only serve a consumer that asks for rows in ascending order. Excel and
    /// openpyxl both write rows in order, but the format does not require it,
    /// so the order is measured here and the stream is used only when it holds.
    pub rows_ascending: bool,
}

/// An inclusive cell range on one sheet: (sheet, r0, c0, r1, c1).
pub type RangeRef = (u16, u32, u32, u32, u32);

/// Visibility of an OOXML defined name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NameScope {
    Workbook,
    Sheet(u16),
    Invalid,
}

/// A defined name and its fixed target, when this reader supports the target.
///
/// Only a fixed absolute range counts. A constant name (`_Order1 = 0`) and an
/// open-sided name (`S!$A:$A`) both stay unsupported: the whole-file loader
/// drops a constant name and answers `#NAME?`, so resolving one here would make
/// a partitioned run disagree with the file it is supposed to reproduce, and
/// clamping an open side would freeze the extent that `ROWS`, `COLUMNS` and
/// `COUNTBLANK` read.
#[derive(Clone)]
pub struct DefinedName {
    pub name: Box<str>,
    pub scope: NameScope,
    pub target: Option<RangeRef>,
    /// Set when the name points at a sheet the workbook does not declare.
    /// See `missing_target_sheet`.
    pub missing_sheet: Option<String>,
}

/// A supported defined name that at least one formula uses.
#[derive(Clone)]
pub struct StaticName {
    pub name: Box<str>,
    pub scope: NameScope,
    pub target: RangeRef,
}

/// First parser failure, retained without keeping every rejected source.
#[derive(Debug)]
pub struct ParseFailure {
    pub stage: &'static str,
    pub sheet: String,
    pub row: u32,
    pub col: u32,
    pub formula: String,
    pub error: String,
}

/// Everything the XML read stage produces, before the dependency graph is
/// built.
///
/// The prelude stage runs between the two stages and can replace a formula's
/// text. The references are therefore collected in the graph stage, not here.
pub struct Sources {
    /// Workbook calculation settings, applied before mini-workbook construction.
    pub calc_settings: Option<formualizer::workbook::traits::CalcSettings>,
    pub sheets: Vec<SheetInfo>,
    pub cells: Vec<FormulaCell>,
    /// Distinct formula sources, indexed by `FormulaCell::ast`.
    pub texts: Vec<Box<str>>,
    /// The cell each distinct formula was parsed at, indexed as `texts`.
    ///
    /// A cell that reuses a template holds its offset from this anchor in
    /// `FormulaCell::dr` and `FormulaCell::dc`.
    pub anchors: Vec<(u32, u32)>,
    /// Data values of each sheet, in the order the part holds them.
    ///
    /// Collected by the same pass that finds the formulas, because inflating a
    /// worksheet part and tokenizing it costs about as much as everything else
    /// the value store does. Cells holding a formula are still in here: which
    /// ones those are is only settled once the sheets have all been read, so
    /// `DataStore` drops them.
    pub values: Vec<Vec<(u32, u32, Val)>>,
    /// Declared cells with no value, per sheet, in the order the part holds
    /// them. A `<c r="G36" s="26" t="n"/>` holds no value but does
    /// exist, and the whole-file loader keeps it as a blank. Only presence
    /// is observable, and only to the calls that count blanks rather than
    /// values, so the store keeps one of these only when a formula range
    /// covers it (see `partition::sheet_blanks`). Formula cells are never in
    /// here, even when they carry no cached value.
    pub blanks: Vec<Vec<(u32, u32)>>,
    pub defined_names: Vec<DefinedName>,
    /// Sheets that exist only because a defined name points at them.
    ///
    /// See `missing_target_sheet`. They hold no cells; they are reported so
    /// that a partitioned run lists the same sheets as a whole-file run.
    pub name_only_sheets: Vec<String>,
    pub dynamic_refs: u64,
    pub parse_errors: u64,
    pub first_parse_error: Option<ParseFailure>,
    /// Worksheet `<f>` cells, including shared members and rejected formulas.
    pub xml_formula_cells: u64,
    pub array_formulas: u64,
    /// Formulas that call a function two engines cannot be made to agree on.
    ///
    /// See `NONDETERMINISTIC_FNS`. A pinned clock covers `NOW` and `TODAY`, so
    /// this counts the calls that remain, such as `RAND`. The
    /// scratch path evaluates a formula once, so it rejects these files.
    pub nondeterministic_fns: u64,
    /// Formulas that call a function which reads its own cell position, such
    /// as `ROW` or `ADDRESS`.
    ///
    /// The scratch path moves a formula to another row. That move changes what
    /// these functions return, so it rejects these files.
    pub row_sensitive_fns: u64,
    /// Aggregate calls whose criterion takes the engine's text-lane path.
    /// Diagnostic; see the `TEXT_CRITERIA_FNS` comment.
    pub text_criteria_ifs: u64,
    /// Whether each distinct formula source can produce a spilled array.
    ///
    /// See `array_capable_ast`. The chunk layout refuses these templates;
    /// the components layout inspects the ones a batch places.
    pub array_capable: Vec<bool>,
    pub t_read_ms: f64,
}

/// The dependency structure of a workbook: which cells hold formulas, the
/// deduplicated ASTs behind them, and how they group into components.
pub struct Topology {
    /// Preserve the whole-file loader's cycle policy in every evaluation layout.
    pub calc_settings: Option<formualizer::workbook::traits::CalcSettings>,
    pub sheets: Vec<SheetInfo>,
    pub cells: Vec<FormulaCell>,
    /// Distinct formula sources, indexed by `FormulaCell::ast`. A batch parses
    /// only the ones it needs, so no run holds every parsed formula at once.
    pub texts: Vec<Box<str>>,
    /// The cell each distinct formula was parsed at, indexed as `texts`.
    pub anchors: Vec<(u32, u32)>,
    /// Data values of each sheet, as read. See `Sources::values`.
    ///
    /// `DataStore` takes these; a workbook that falls back to a whole-file run
    /// must drop them, since it reads its own values.
    pub values: Vec<Vec<(u32, u32, Val)>>,
    /// Declared cells with no value, per sheet. See `Sources::blanks`.
    ///
    /// `DataStore` keeps the ones a formula range covers, as blank entries.
    pub blanks: Vec<Vec<(u32, u32)>>,
    /// References of each distinct formula, at its anchor position.
    pub ast_refs: Vec<Vec<RawRef>>,
    /// Supported fixed-address names that at least one formula uses.
    pub static_names: Vec<StaticName>,
    /// Number of resolved name references. Component workbooks recreate
    /// the used definitions with their original scope and target.
    pub named_refs: u64,
    /// Rows each distinct formula reads in its lookup calls, indexed as
    /// `texts`. See `LOOKUP_FNS` and `Topology::lookup_work`.
    pub lookup_rows: Vec<u64>,
    /// Formula cell index -> component id.
    pub comp_of: Vec<u32>,
    /// Component id -> its formula cell indices.
    pub comp_cells: Vec<Vec<u32>>,
    /// Component id -> every range its formulas read.
    pub comp_refs: Vec<Vec<RangeRef>>,
    /// Component id -> bounding-box cell count, summed across sheets.
    pub comp_extent: Vec<u64>,
    pub index: HashMap<(u16, u32, u32), u32>,
    /// Sheets that exist only because a defined name points at them.
    /// See `Sources::name_only_sheets`.
    pub name_only_sheets: Vec<String>,
    pub full_extent_cells: u64,
    pub cross_sheet: bool,
    pub cross_row: bool,
    pub unsupported_refs: u64,
    /// References that only exist at runtime (INDIRECT/OFFSET). Static
    /// partitioning cannot see them, so their presence forces a whole-file run.
    pub dynamic_refs: u64,
    /// Diagnostic count of `<f t="array" ref="...">` anchors. The pinned
    /// Calamine loader treats these as ordinary formula text and ignores the
    /// declared extent. Actual spills use whole-file occupancy, just as for
    /// formulas without this XML annotation.
    pub array_formulas: u64,
    /// Formulas naming their own cell, such as `A7 =ROW(A7)-6`. A whole-file
    /// load accepts these, but the incremental edit path a mini-workbook is
    /// built with rejects them outright, so they cannot be partitioned.
    pub self_refs: u64,
    pub parse_errors: u64,
    pub first_parse_error: Option<ParseFailure>,
    pub xml_formula_cells: u64,
    /// Calls two engines cannot agree on. See `Sources::nondeterministic_fns`.
    pub nondeterministic_fns: u64,
    /// Formulas that read their own cell position. See
    /// `Sources::row_sensitive_fns`.
    pub row_sensitive_fns: u64,
    /// Aggregate calls whose criterion takes the engine's text-lane path.
    /// Diagnostic; see the `TEXT_CRITERIA_FNS` comment.
    pub text_criteria_ifs: u64,
    /// Whether each distinct formula source can produce a spilled array.
    /// See `Sources::array_capable`.
    pub array_capable: Vec<bool>,
    pub t_read_ms: f64,
    pub t_graph_ms: f64,
}

pub struct Analysis {
    pub n_formula_cells: usize,
    pub n_components: usize,
    pub n_parsed: usize,
    pub full_extent_cells: u64,
    pub biggest_extent_cells: u64,
    pub cross_sheet: bool,
    pub cross_row: bool,
    pub unsupported_refs: u64,
    pub parse_errors: u64,
    pub first_parse_error: Option<ParseFailure>,
    pub xml_formula_cells: u64,
    pub top: Vec<(u64, u32, u32)>,
    pub t_read_ms: f64,
    pub t_graph_ms: f64,
}

/// Parse "A1" / "$A$1" into 1-based (row, col).
pub(crate) fn parse_a1(s: &str) -> Option<(u32, u32)> {
    let mut col: u32 = 0;
    let mut row: u32 = 0;
    let mut seen_digit = false;
    for ch in s.bytes() {
        match ch {
            b'$' => {}
            b'A'..=b'Z' => {
                if seen_digit {
                    return None;
                }
                col = col * 26 + (ch - b'A' + 1) as u32;
            }
            b'a'..=b'z' => {
                if seen_digit {
                    return None;
                }
                col = col * 26 + (ch - b'a' + 1) as u32;
            }
            b'0'..=b'9' => {
                seen_digit = true;
                row = row * 10 + (ch - b'0') as u32;
            }
            _ => return None,
        }
    }
    if col == 0 || row == 0 {
        None
    } else {
        Some((row, col))
    }
}

fn attr_value(e: &quick_xml::events::BytesStart<'_>, key: &[u8]) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.as_ref() == key {
            return a.unescape_value().ok().map(|v| v.into_owned());
        }
    }
    None
}

/// A reference exactly as parsed, before it is resolved against a sheet and
/// shifted into a member cell's position.
///
/// Keeping these instead of the parsed formulas is what makes a topology small.
/// A parsed formula is a tree of nodes, each carrying the text it came from, and
/// that tree dwarfs every other field of the topology. The references are the
/// only part the graph needs, and the formula text is kept so a batch can
/// re-parse just its own formulas later.
#[derive(Clone)]
pub enum RawRef {
    Cell {
        sheet: Option<Box<str>>,
        row: u32,
        col: u32,
        row_abs: bool,
        col_abs: bool,
    },
    Range {
        sheet: Option<Box<str>>,
        start_row: Option<u32>,
        start_col: Option<u32>,
        end_row: Option<u32>,
        end_col: Option<u32>,
        start_row_abs: bool,
        start_col_abs: bool,
        end_row_abs: bool,
        end_col_abs: bool,
    },
    Named {
        name: Box<str>,
    },
    /// A table, 3D reference, external reference or broken reference.
    Unsupported,
}

/// One endpoint of a `:` range operator: a fully specified cell or span.
///
/// The parser represents `A1:B2` as one `Reference`, but a second colon
/// (`A1:B2:C3`) parses as `BinaryOp(":", Reference, Reference)`. Collecting
/// the endpoints separately would silently drop the cells between them (the
/// `SUM(I8:K8:M8)` bug: `L8` was never copied), so chained colons merge here.
struct ColonEnd {
    sheet: Option<Box<str>>,
    r0: u32,
    c0: u32,
    r1: u32,
    c1: u32,
    start_row_abs: bool,
    start_col_abs: bool,
    end_row_abs: bool,
    end_col_abs: bool,
}

fn colon_end(node: &ASTNode) -> Option<ColonEnd> {
    match &node.node_type {
        ASTNodeType::Reference { reference, .. } => match reference {
            ReferenceType::Cell { sheet, row, col, row_abs, col_abs } => {
                if *row == 0 || *col == 0 {
                    return None;
                }
                Some(ColonEnd {
                    sheet: sheet.clone().map(|s| s.into()),
                    r0: *row,
                    c0: *col,
                    r1: *row,
                    c1: *col,
                    start_row_abs: *row_abs,
                    start_col_abs: *col_abs,
                    end_row_abs: *row_abs,
                    end_col_abs: *col_abs,
                })
            }
            ReferenceType::Range {
                sheet,
                start_row: Some(r0),
                start_col: Some(c0),
                end_row: Some(r1),
                end_col: Some(c1),
                start_row_abs,
                start_col_abs,
                end_row_abs,
                end_col_abs,
            } => {
                if *r0 == 0 || *c0 == 0 || *r1 == 0 || *c1 == 0 {
                    return None;
                }
                Some(ColonEnd {
                    sheet: sheet.clone().map(|s| s.into()),
                    r0: *r0,
                    c0: *c0,
                    r1: *r1,
                    c1: *c1,
                    start_row_abs: *start_row_abs,
                    start_col_abs: *start_col_abs,
                    end_row_abs: *end_row_abs,
                    end_col_abs: *end_col_abs,
                })
            }
            _ => None,
        },
        ASTNodeType::BinaryOp { op, left, right } if op == ":" => {
            let a = colon_end(left)?;
            let b = colon_end(right)?;
            match (&a.sheet, &b.sheet) {
                (None, None) => {}
                (Some(x), Some(y)) if x.eq_ignore_ascii_case(y) => {}
                _ => return None,
            }
            Some(ColonEnd {
                sheet: a.sheet.or(b.sheet),
                r0: a.r0.min(b.r0),
                c0: a.c0.min(b.c0),
                r1: a.r1.max(b.r1),
                c1: a.c1.max(b.c1),
                start_row_abs: a.start_row_abs,
                start_col_abs: a.start_col_abs,
                end_row_abs: b.end_row_abs,
                end_col_abs: b.end_col_abs,
            })
        }
        _ => None,
    }
}

/// Collect a parsed formula's references so the tree itself can be dropped.
fn collect_refs(ast: &ASTNode) -> Vec<RawRef> {
    fn push_reference(reference: &ReferenceType, out: &mut Vec<RawRef>) {
        match reference {
            ReferenceType::Cell { sheet, row, col, row_abs, col_abs } => out.push(RawRef::Cell {
                sheet: sheet.clone().map(|s| s.into()),
                row: *row,
                col: *col,
                row_abs: *row_abs,
                col_abs: *col_abs,
            }),
            ReferenceType::Range {
                sheet,
                start_row,
                start_col,
                end_row,
                end_col,
                start_row_abs,
                start_col_abs,
                end_row_abs,
                end_col_abs,
            } => out.push(RawRef::Range {
                sheet: sheet.clone().map(|s| s.into()),
                start_row: *start_row,
                start_col: *start_col,
                end_row: *end_row,
                end_col: *end_col,
                start_row_abs: *start_row_abs,
                start_col_abs: *start_col_abs,
                end_row_abs: *end_row_abs,
                end_col_abs: *end_col_abs,
            }),
            ReferenceType::NamedRange(name) => out.push(RawRef::Named { name: name.clone().into() }),
            _ => out.push(RawRef::Unsupported),
        }
    }
    fn walk(node: &ASTNode, out: &mut Vec<RawRef>) {
        // A chained colon (`A1:B2:C3`) is one range, not its endpoints. An
        // endpoint the merger rejects (a function call, a cross-sheet pair)
        // falls back to the whole file instead of a silently partial closure.
        if let ASTNodeType::BinaryOp { op, .. } = &node.node_type {
            if op == ":" {
                match colon_end(node) {
                    Some(e) => out.push(RawRef::Range {
                        sheet: e.sheet,
                        start_row: Some(e.r0),
                        start_col: Some(e.c0),
                        end_row: Some(e.r1),
                        end_col: Some(e.c1),
                        start_row_abs: e.start_row_abs,
                        start_col_abs: e.start_col_abs,
                        end_row_abs: e.end_row_abs,
                        end_col_abs: e.end_col_abs,
                    }),
                    None => out.push(RawRef::Unsupported),
                }
                return;
            }
        }
        match &node.node_type {
            ASTNodeType::Reference { reference, .. } => push_reference(reference, out),
            ASTNodeType::UnaryOp { expr, .. } => walk(expr, out),
            ASTNodeType::BinaryOp { left, right, .. } => {
                walk(left, out);
                walk(right, out);
            }
            ASTNodeType::Function { name, args } => {
                // The tokenizer glues a dynamic endpoint onto the colon
                // (`A1:OFFSET(` parses as a function named `"A1:OFFSET"`).
                // That range has no static bound, so the file falls back.
                // (Excel function names never contain `:`, so any such name
                // is a glued range, not a real function.)
                if name.contains(':') {
                    out.push(RawRef::Unsupported);
                    return;
                }
                for a in args {
                    walk(a, out);
                }
            }
            ASTNodeType::Call { callee, args } => {
                walk(callee, out);
                for a in args {
                    walk(a, out);
                }
            }
            ASTNodeType::Array(rows) => {
                for r in rows {
                    for a in r {
                        walk(a, out);
                    }
                }
            }
            ASTNodeType::Literal(_) | ASTNodeType::Omitted => {}
        }
    }
    let mut out = Vec::new();
    walk(ast, &mut out);
    out
}

/// Resolve sheet order and worksheet part paths from workbook.xml + its rels.
pub fn sheet_parts<R: Read + std::io::Seek>(zip: &mut ZipArchive<R>) -> Vec<(String, String)> {
    let mut rid_target: HashMap<String, String> = HashMap::new();
    if let Ok(mut f) = zip.by_name("xl/_rels/workbook.xml.rels") {
        let mut s = String::new();
        if f.read_to_string(&mut s).is_ok() {
            let mut rdr = Reader::from_str(&s);
            let mut buf = Vec::new();
            loop {
                match rdr.read_event_into(&mut buf) {
                    Ok(Event::Empty(e)) | Ok(Event::Start(e)) => {
                        if e.name().as_ref().ends_with(b"Relationship") {
                            if let (Some(id), Some(t)) =
                                (attr_value(&e, b"Id"), attr_value(&e, b"Target"))
                            {
                                rid_target.insert(id, t);
                            }
                        }
                    }
                    Ok(Event::Eof) | Err(_) => break,
                    _ => {}
                }
                buf.clear();
            }
        }
    }

    let mut out = Vec::new();
    if let Ok(mut f) = zip.by_name("xl/workbook.xml") {
        let mut s = String::new();
        if f.read_to_string(&mut s).is_ok() {
            let mut rdr = Reader::from_str(&s);
            let mut buf = Vec::new();
            loop {
                match rdr.read_event_into(&mut buf) {
                    Ok(Event::Empty(e)) | Ok(Event::Start(e)) => {
                        if e.name().as_ref() == b"sheet" {
                            let name = attr_value(&e, b"name").unwrap_or_default();
                            let rid = attr_value(&e, b"r:id");
                            if let Some(rid) = rid {
                                if let Some(t) = rid_target.get(&rid) {
                                    let path = if let Some(stripped) = t.strip_prefix('/') {
                                        stripped.to_string()
                                    } else {
                                        format!("xl/{}", t.trim_start_matches("./"))
                                    };
                                    out.push((name, path));
                                }
                            }
                        }
                    }
                    Ok(Event::Eof) | Err(_) => break,
                    _ => {}
                }
                buf.clear();
            }
        }
    }
    out
}

fn parse_defined_target(raw: &str, parts: &[(String, String)], scope: NameScope) -> Option<RangeRef> {
    let text = normalise_target(raw);
    let reference = ReferenceType::from_string(text).ok()?;
    let sheet_index = |name: &str| {
        parts
            .iter()
            .position(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|i| i as u16)
    };
    let target_sheet = |sheet: Option<&str>| match sheet {
        Some(name) => sheet_index(name),
        None => match scope {
            NameScope::Sheet(sheet) => Some(sheet),
            _ => None,
        },
    };
    match reference {
        ReferenceType::Cell { sheet, row, col, row_abs, col_abs }
            if row > 0 && col > 0 && row_abs && col_abs =>
        {
            Some((target_sheet(sheet.as_deref())?, row, col, row, col))
        }
        ReferenceType::Range {
            sheet,
            start_row: Some(r0),
            start_col: Some(c0),
            end_row: Some(r1),
            end_col: Some(c1),
            start_row_abs,
            start_col_abs,
            end_row_abs,
            end_col_abs,
        } if r0 > 0
            && c0 > 0
            && r1 >= r0
            && c1 >= c0
            && start_row_abs
            && start_col_abs
            && end_row_abs
            && end_col_abs =>
        {
            Some((target_sheet(sheet.as_deref())?, r0, c0, r1, c1))
        }
        _ => None,
    }
}

fn normalise_target(raw: &str) -> &str {
    raw.trim().strip_prefix('=').unwrap_or(raw.trim()).trim()
}

/// The sheet a defined name points at when the workbook holds no such sheet.
///
/// The whole-file loader creates an empty sheet for such a name, so a
/// partitioned run that reports only the declared sheets returns a different
/// set of sheets for the same file. Reporting these keeps the two equal.
fn missing_target_sheet(raw: &str, parts: &[(String, String)]) -> Option<String> {
    let sheet = match ReferenceType::from_string(normalise_target(raw)).ok()? {
        ReferenceType::Cell { sheet: Some(s), .. } | ReferenceType::Range { sheet: Some(s), .. } => s,
        _ => return None,
    };
    // An external reference (`[1]Sheet1`) names another workbook, and the
    // loader does not create a sheet for it.
    if sheet.starts_with('[') || parts.iter().any(|(c, _)| c.eq_ignore_ascii_case(&sheet)) {
        return None;
    }
    Some(sheet)
}

/// Read the defined names without loading worksheet XML again.
fn read_defined_names<R: Read + std::io::Seek>(
    zip: &mut ZipArchive<R>,
    parts: &[(String, String)],
) -> Vec<DefinedName> {
    let Ok(file) = zip.by_name("xl/workbook.xml") else {
        return Vec::new();
    };
    let mut reader = Reader::from_reader(std::io::BufReader::new(file));
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut in_names = false;
    let mut current: Option<(Box<str>, NameScope, String)> = None;
    // localSheetId indexes workbook.xml, including sheets omitted by
    // sheet_parts when their relationship is missing. Map by name rather
    // than letting an omitted part silently shift every later name's scope.
    let mut scope_sheets = Vec::new();
    // Only the real workbook sheet list may contribute scope slots. A stray
    // namespaced `<x:sheet>` from an extension must not inject a slot, and
    // matching the raw element name keeps this consistent with `sheet_parts`.
    let mut in_sheets = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"sheets" => in_sheets = true,
            Ok(Event::End(e)) if e.local_name().as_ref() == b"sheets" => in_sheets = false,
            Ok(Event::Start(e)) | Ok(Event::Empty(e))
                if in_sheets && e.name().as_ref() == b"sheet" =>
            {
                let name = attr_value(&e, b"name");
                scope_sheets.push(name.and_then(|name| {
                    parts.iter().position(|(candidate, _)| candidate == &name).map(|i| i as u16)
                }));
            }
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"definedNames" => {
                in_names = true;
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"definedNames" => break,
            Ok(Event::Start(e)) if in_names && e.local_name().as_ref() == b"definedName" => {
                if let Some(name) = attr_value(&e, b"name") {
                    let scope = match attr_value(&e, b"localSheetId") {
                        None => NameScope::Workbook,
                        Some(raw) => match raw.parse::<usize>() {
                            Ok(i) if scope_sheets.get(i).copied().flatten().is_some() => {
                                NameScope::Sheet(scope_sheets[i].unwrap())
                            }
                            _ => NameScope::Invalid,
                        },
                    };
                    current = Some((name.into_boxed_str(), scope, String::new()));
                }
            }
            Ok(Event::Text(text)) => {
                if let Some((_, _, value)) = &mut current {
                    value.push_str(&decode_text(text.as_ref()));
                }
            }
            Ok(Event::CData(text)) => {
                if let Some((_, _, value)) = &mut current {
                    value.push_str(&String::from_utf8_lossy(&text));
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"definedName" => {
                if let Some((name, scope, value)) = current.take() {
                    out.push(DefinedName {
                        name,
                        scope,
                        target: parse_defined_target(&value, parts, scope),
                        missing_sheet: missing_target_sheet(&value, parts),
                    });
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Read every formula cell in the workbook.
///
/// Two mechanisms keep the number of retained formula sources low. A shared
/// formula (`<f t="shared" si="N">`) is stored once at its master anchor.
/// A formula that is written out per row is compared against the first formula
/// of its column; if the two agree after the column template moves to the new
/// row, the cell reuses the template and its own text is dropped.
///
/// Every formula is still parsed once. Only the retained text changes.
pub fn read(data: &[u8]) -> Sources {
    let t0 = std::time::Instant::now();
    let mut sheets: Vec<SheetInfo> = Vec::new();
    let mut cells: Vec<FormulaCell> = Vec::new();
    let mut texts: Vec<Box<str>> = Vec::new();
    let mut anchors: Vec<(u32, u32)> = Vec::new();
    let mut values: Vec<Vec<(u32, u32, Val)>> = Vec::new();
    let mut blanks: Vec<Vec<(u32, u32)>> = Vec::new();
    let mut dynamic_refs: u64 = 0;
    let mut parse_errors: u64 = 0;
    let mut first_parse_error = None;
    let mut xml_formula_cells = 0;
    let mut array_formulas: u64 = 0;
    let mut nondeterministic_fns: u64 = 0;
    let mut row_sensitive_fns: u64 = 0;
    let mut text_criteria_ifs: u64 = 0;
    let mut array_capable: Vec<bool> = Vec::new();

    let cursor = Cursor::new(data);
    let mut zip = match ZipArchive::new(cursor) {
        Ok(z) => z,
        Err(_) => {
            return Sources {
                calc_settings: None,
                sheets,
                cells,
                texts,
                anchors,
                values,
                blanks,
                defined_names: Vec::new(),
                name_only_sheets: Vec::new(),
                dynamic_refs,
                parse_errors,
                first_parse_error,
                xml_formula_cells,
                array_formulas,
                nondeterministic_fns,
                row_sensitive_fns,
                text_criteria_ifs,
                array_capable,
                t_read_ms: t0.elapsed().as_secs_f64() * 1000.0,
            }
        }
    };
    let calc_settings = zip.by_name("xl/workbook.xml").ok().and_then(|mut part| {
        let mut xml = Vec::new();
        part.read_to_end(&mut xml).ok()?;
        formualizer::workbook::calc_pr::parse_calc_pr(&xml)
    });
    let parts = sheet_parts(&mut zip);
    let defined_names = read_defined_names(&mut zip, &parts);
    let mut name_only_sheets: Vec<String> = Vec::new();
    for name in &defined_names {
        if let Some(sheet) = name.missing_sheet.clone() {
            if !name_only_sheets.iter().any(|s| s.eq_ignore_ascii_case(&sheet)) {
                name_only_sheets.push(sheet);
            }
        }
    }
    let decoder = Values::open(&mut zip);

    for (si, (name, path)) in parts.iter().enumerate() {
        // The part is streamed rather than read whole, because a worksheet
        // inflates to several times the size of the file it came from. A sheet
        // whose part cannot be opened still occupies its position in the
        // workbook, and callers index sheets by that position, so an unreadable
        // one is recorded as empty rather than skipped.
        let part = match zip.by_name(path) {
            Ok(f) => f,
            Err(_) => {
                sheets.push(SheetInfo {
                    name: name.clone(),
                    max_row: 0,
                    max_col: 0,
                    data_row: 0,
                    data_col: 0,
                    rows_ascending: true,
                });
                values.push(Vec::new());
                blanks.push(Vec::new());
                continue;
            }
        };

        let mut max_row = 0u32;
        let mut max_col = 0u32;
        let mut data_row = 0u32;
        let mut data_col = 0u32;
        // si attribute -> (ast index, anchor row, anchor col)
        let mut shared: HashMap<String, (u32, u32, u32)> = HashMap::new();
        // member cells resolved after the pass; the master may appear later
        let mut pending: Vec<(u32, u32, String)> = Vec::new();
        // column -> (ast index, anchor row, the parsed template).
        //
        // One tree per formula column is held at a time. A sheet of 300000
        // per-row formulas in 30 columns therefore keeps 30 trees, not 300000
        // texts. A formula that does not match its column template becomes the
        // new template for that column.
        let mut col_template: HashMap<u32, (u32, u32, ASTNode)> = HashMap::new();

        let mut rdr = Reader::from_reader(std::io::BufReader::new(part));
        let mut buf = Vec::new();
        let mut rows_ascending = true;
        let mut last_row = 0u32;
        let mut cur: Option<(u32, u32)> = None;
        // Whether the current cell holds a formula, and whether a value was
        // recorded for it. A declared cell with neither is a blank the
        // whole-file loader keeps, so it is recorded in `blanks` when its
        // element closes. A formula cell is never a blank, even when it
        // carries no cached value.
        let mut cur_is_formula = false;
        let mut cur_pushed = false;
        let mut cur_inline = false;
        let mut cur_style: Option<Vec<u8>> = None;
        let mut cur_ty: Option<Vec<u8>> = None;
        let mut in_v = false;
        let mut v_text = String::new();
        let mut text_buf: Vec<u8> = Vec::new();
        let mut vals: Vec<(u32, u32, Val)> = Vec::new();
        let mut sheet_blanks: Vec<(u32, u32)> = Vec::new();
        let mut in_f = false;
        let mut f_text = String::new();
        let mut f_shared = false;
        let mut f_si: Option<String> = None;

        loop {
            match rdr.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => match e.name().as_ref() {
                    // <dimension ref="A1:DB1968"/> is the sheet's declared
                    // extent. It can reach past the last <c> element, and the
                    // engine reports it as the sheet size, so the output grid
                    // has to agree with it.
                    b"dimension" => {
                        if let Some((r, c)) = attr_value(&e, b"ref")
                            .and_then(|d| d.rsplit(':').next().and_then(parse_a1))
                        {
                            max_row = max_row.max(r);
                            max_col = max_col.max(c);
                        }
                    }
                    b"c" => {
                        cur = attr_value(&e, b"r").and_then(|r| parse_a1(&r));
                        cur_is_formula = false;
                        cur_pushed = false;
                        cur_inline = attr_value(&e, b"t").as_deref() == Some("inlineStr");
                        cur_style = raw_attr(&e, b"s");
                        cur_ty = raw_attr(&e, b"t");
                        if let Some((r, c)) = cur {
                            max_row = max_row.max(r);
                            max_col = max_col.max(c);
                            if r < last_row {
                                rows_ascending = false;
                            }
                            last_row = r;
                        }
                    }
                    // An <is> block is only read as a value when the cell says
                    // so; without t="inlineStr" the reader skips it, and
                    // counting it here would extend a sheet by a row of blanks.
                    b"v" | b"is" | b"f"
                        if cur.is_some() && (e.name().as_ref() != b"is" || cur_inline) =>
                    {
                        if let Some((r, c)) = cur {
                            data_row = data_row.max(r);
                            data_col = data_col.max(c);
                        }
                        match e.name().as_ref() {
                            b"v" => {
                                in_v = true;
                                v_text.clear();
                            }
                            // Inline text holds the whole value here, and any
                            // <v> beside it repeats nothing.
                            b"is" => {
                                let text = read_text_element(&mut rdr, b"is", &mut text_buf);
                                if let (Some((r, c)), false) = (cur, text.is_empty()) {
                                    vals.push((r, c, Val::pack(LiteralValue::Text(text))));
                                    cur_pushed = true;
                                }
                            }
                            _ => {
                                cur_is_formula = true;
                                xml_formula_cells += 1;
                                f_shared = attr_value(&e, b"t").as_deref() == Some("shared");
                                if attr_value(&e, b"t").as_deref() == Some("array") {
                                    array_formulas += 1;
                                }
                                f_si = attr_value(&e, b"si");
                                f_text.clear();
                                in_f = true;
                            }
                        }
                    }
                    b"f" => {
                        cur_is_formula = true;
                        f_shared = attr_value(&e, b"t").as_deref() == Some("shared");
                        if attr_value(&e, b"t").as_deref() == Some("array") {
                            array_formulas += 1;
                        }
                        f_si = attr_value(&e, b"si");
                        f_text.clear();
                        in_f = true;
                    }
                    _ => {}
                },
                Ok(Event::Empty(e)) => match e.name().as_ref() {
                    b"dimension" => {
                        if let Some((r, c)) = attr_value(&e, b"ref")
                            .and_then(|d| d.rsplit(':').next().and_then(parse_a1))
                        {
                            max_row = max_row.max(r);
                            max_col = max_col.max(c);
                        }
                    }
                    b"c" => {
                        if let Some((r, c)) = attr_value(&e, b"r").and_then(|r| parse_a1(&r)) {
                            max_row = max_row.max(r);
                            max_col = max_col.max(c);
                            if r < last_row {
                                rows_ascending = false;
                            }
                            last_row = r;
                            // A self-closing cell carries no children, so it
                            // holds neither a value nor a formula.
                            sheet_blanks.push((r, c));
                        }
                        cur = None;
                    }
                    // A self-closing <f t="shared" si="N"/> is a shared-formula
                    // member: it carries no text and never emits an End event.
                    b"f" => {
                        if cur.is_some() {
                            cur_is_formula = true;
                        }
                        if let Some((r, c)) = cur {
                            xml_formula_cells += 1;
                            if attr_value(&e, b"t").as_deref() == Some("array") {
                                array_formulas += 1;
                            }
                            data_row = data_row.max(r);
                            data_col = data_col.max(c);
                        }
                        if let (Some((r, c)), Some(key)) = (cur, attr_value(&e, b"si")) {
                            pending.push((r, c, key));
                        }
                    }
                    _ => {}
                },
                Ok(Event::Text(t)) => {
                    if in_f {
                        if let Ok(s) = t.unescape() {
                            f_text.push_str(&s);
                        }
                    } else if in_v {
                        v_text.push_str(&decode_text(t.as_ref()));
                    }
                }
                Ok(Event::CData(t)) => {
                    if in_f {
                        f_text.push_str(&String::from_utf8_lossy(&t));
                    } else if in_v {
                        v_text.push_str(&String::from_utf8_lossy(&t));
                    }
                }
                Ok(Event::End(e)) => {
                    if e.name().as_ref() == b"c" {
                        // A declared cell with no value and no formula is a
                        // blank the whole-file loader keeps. The self-closing
                        // form is recorded where it is seen; this closes the
                        // Start/End form.
                        if let Some((r, c)) = cur {
                            if !cur_is_formula && !cur_pushed {
                                sheet_blanks.push((r, c));
                            }
                        }
                        cur = None;
                    }
                    if e.name().as_ref() == b"v" {
                        in_v = false;
                        if let Some((r, c)) = cur {
                            if let Some(val) = decoder.value(
                                &decode_escapes(&v_text),
                                cur_style.as_deref(),
                                cur_ty.as_deref(),
                            ) {
                                vals.push((r, c, Val::pack(val)));
                                cur_pushed = true;
                            }
                        }
                    }
                    if e.name().as_ref() == b"f" {
                        in_f = false;
                        if let Some((r, c)) = cur {
                            if !f_text.is_empty() {
                                // Sheet XML stores formula text without the
                                // leading '='; the parser needs it to enter
                                // formula mode (otherwise the text parses as a
                                // literal and yields no references).
                                let src = if f_text.starts_with('=') {
                                    f_text.clone()
                                } else {
                                    format!("={f_text}")
                                };
                                match parse(&src) {
                                    Ok(ast) => {
                                        // A cell reuses its column template when
                                        // the two trees agree after the
                                        // template moves to this row. The
                                        // template is then the only tree and
                                        // the only text that is kept.
                                        let reuse = col_template.get(&c).and_then(
                                            |(idx, anchor_row, template)| {
                                                let dr = r as i64 - *anchor_row as i64;
                                                if matches_shifted(template, &ast, dr, 0) {
                                                    Some((*idx, dr))
                                                } else {
                                                    None
                                                }
                                            },
                                        );

                                        let (idx, dr, dc) = match reuse {
                                            Some((idx, dr)) => (idx, dr, 0),
                                            None => {
                                                // Only a new template is
                                                // classified. A reused one was
                                                // classified when it was first
                                                // seen, and every gate that
                                                // reads these counters tests
                                                // them against zero.
                                                classify(
                                                    &ast,
                                                    &mut dynamic_refs,
                                                    &mut row_sensitive_fns,
                                                    &mut nondeterministic_fns,
                                                    &mut text_criteria_ifs,
                                                );
                                                array_capable.push(array_capable_ast(&ast));
                                                let idx = texts.len() as u32;
                                                texts.push(src.into_boxed_str());
                                                anchors.push((r, c));
                                                col_template.insert(c, (idx, r, ast));
                                                (idx, 0, 0)
                                            }
                                        };

                                        if f_shared {
                                            if let Some(key) = f_si.clone() {
                                                // A member shifts from the
                                                // anchor the template was
                                                // parsed at, which is not
                                                // always this master cell.
                                                let (ar, ac) = anchors[idx as usize];
                                                shared.insert(key, (idx, ar, ac));
                                            }
                                        }
                                        cells.push(FormulaCell {
                                            sheet: si as u16,
                                            row: r,
                                            col: c,
                                            ast: idx,
                                            dr,
                                            dc,
                                        });
                                    }
                                    Err(error) => {
                                        parse_errors += 1;
                                        first_parse_error.get_or_insert_with(|| ParseFailure {
                                            stage: "read",
                                            sheet: name.clone(),
                                            row: r,
                                            col: c,
                                            formula: src,
                                            error: error.to_string(),
                                        });
                                    }
                                }
                            } else if let Some(key) = f_si.clone() {
                                pending.push((r, c, key));
                            }
                        }
                        f_text.clear();
                        f_si = None;
                        f_shared = false;
                    }
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        for (r, c, key) in pending {
            if let Some(&(idx, ar, ac)) = shared.get(&key) {
                cells.push(FormulaCell {
                    sheet: si as u16,
                    row: r,
                    col: c,
                    ast: idx,
                    dr: r as i64 - ar as i64,
                    dc: c as i64 - ac as i64,
                });
            }
        }

        sheets.push(SheetInfo {
            name: name.clone(),
            max_row,
            max_col,
            data_row,
            data_col,
            rows_ascending,
        });
        values.push(vals);
        blanks.push(sheet_blanks);
    }

    Sources {
        calc_settings,
        sheets,
        cells,
        texts,
        anchors,
        values,
        blanks,
        defined_names,
        name_only_sheets,
        dynamic_refs,
        parse_errors,
        first_parse_error,
        xml_formula_cells,
        array_formulas,
        nondeterministic_fns,
        row_sensitive_fns,
        text_criteria_ifs,
        array_capable,
        t_read_ms: t0.elapsed().as_secs_f64() * 1000.0,
    }
}

/// Does `candidate` equal `template` after the template moves by (dr, dc)?
///
/// The comparison is structural. It ignores the source text that each node
/// carries, because two cells of one column hold different text but the same
/// shape. It also builds nothing: a template reference is shifted into a pair
/// of numbers and compared, so a mismatch costs no allocation.
fn matches_shifted(template: &ASTNode, candidate: &ASTNode, dr: i64, dc: i64) -> bool {
    match (&template.node_type, &candidate.node_type) {
        (ASTNodeType::Literal(a), ASTNodeType::Literal(b)) => a == b,
        (ASTNodeType::Omitted, ASTNodeType::Omitted) => true,
        (
            ASTNodeType::Reference { reference: a, .. },
            ASTNodeType::Reference { reference: b, .. },
        ) => reference_matches_shifted(a, b, dr, dc),
        (ASTNodeType::UnaryOp { op: ao, expr: ae }, ASTNodeType::UnaryOp { op: bo, expr: be }) => {
            ao == bo && matches_shifted(ae, be, dr, dc)
        }
        (
            ASTNodeType::BinaryOp { op: ao, left: al, right: ar },
            ASTNodeType::BinaryOp { op: bo, left: bl, right: br },
        ) => ao == bo && matches_shifted(al, bl, dr, dc) && matches_shifted(ar, br, dr, dc),
        (
            ASTNodeType::Function { name: an, args: aa },
            ASTNodeType::Function { name: bn, args: ba },
        ) => {
            an.eq_ignore_ascii_case(bn)
                && aa.len() == ba.len()
                && aa.iter().zip(ba).all(|(x, y)| matches_shifted(x, y, dr, dc))
        }
        (
            ASTNodeType::Call { callee: ac, args: aa },
            ASTNodeType::Call { callee: bc, args: ba },
        ) => {
            matches_shifted(ac, bc, dr, dc)
                && aa.len() == ba.len()
                && aa.iter().zip(ba).all(|(x, y)| matches_shifted(x, y, dr, dc))
        }
        (ASTNodeType::Array(a), ASTNodeType::Array(b)) => {
            a.len() == b.len()
                && a.iter().zip(b).all(|(ra, rb)| {
                    ra.len() == rb.len()
                        && ra.iter().zip(rb).all(|(x, y)| matches_shifted(x, y, dr, dc))
                })
        }
        _ => false,
    }
}

/// Compare a template reference with a candidate reference under a shift.
///
/// A template that moves off the sheet cannot match, because `shift` gives
/// `None` there. Anything this crate does not resolve statically, such as a
/// name or a table, is compared as it stands.
fn reference_matches_shifted(
    template: &ReferenceType,
    candidate: &ReferenceType,
    dr: i64,
    dc: i64,
) -> bool {
    match (template, candidate) {
        (
            ReferenceType::Cell { sheet: a_sheet, row: ar, col: ac, row_abs: ara, col_abs: aca },
            ReferenceType::Cell { sheet: b_sheet, row: br, col: bc, row_abs: bra, col_abs: bca },
        ) => {
            a_sheet == b_sheet
                && ara == bra
                && aca == bca
                && shift(*ar, dr, *ara) == Some(*br)
                && shift(*ac, dc, *aca) == Some(*bc)
        }
        (
            ReferenceType::Range {
                sheet: a_sheet,
                start_row: asr,
                start_col: asc,
                end_row: aer,
                end_col: aec,
                start_row_abs: asra,
                start_col_abs: asca,
                end_row_abs: aera,
                end_col_abs: aeca,
            },
            ReferenceType::Range {
                sheet: b_sheet,
                start_row: bsr,
                start_col: bsc,
                end_row: ber,
                end_col: bec,
                start_row_abs: bsra,
                start_col_abs: bsca,
                end_row_abs: bera,
                end_col_abs: beca,
            },
        ) => {
            a_sheet == b_sheet
                && (asra, asca, aera, aeca) == (bsra, bsca, bera, beca)
                && shifted_side(*asr, dr, *asra) == Some(*bsr)
                && shifted_side(*asc, dc, *asca) == Some(*bsc)
                && shifted_side(*aer, dr, *aera) == Some(*ber)
                && shifted_side(*aec, dc, *aeca) == Some(*bec)
        }
        _ => template == candidate,
    }
}

/// Shift one side of a range. An open side stays open. A side that moves off
/// the sheet gives `None`, which makes the template comparison fail.
fn shifted_side(v: Option<u32>, d: i64, is_abs: bool) -> Option<Option<u32>> {
    match v {
        None => Some(None),
        Some(v) => shift(v, d, is_abs).map(Some),
    }
}

fn shift(v: u32, d: i64, is_abs: bool) -> Option<u32> {
    if is_abs {
        return Some(v);
    }
    let x = v as i64 + d;
    if x < 1 {
        None
    } else {
        Some(x as u32)
    }
}

/// Build dependency components for a workbook given as xlsx bytes.
/// Names whose references only materialise at evaluation time. A static
/// precedent walk cannot see where they point, so a workbook using them must be
/// evaluated whole rather than partitioned.
const DYNAMIC_FNS: [&str; 2] = ["INDIRECT", "OFFSET"];

/// Names whose result depends on the cell the formula sits in.
///
/// The scratch path moves a formula to another row. That move changes what
/// these functions return, so a file that calls one of them cannot use that
/// path. The general partitioned path keeps the original row and is not
/// affected.
const ROW_SENSITIVE_FNS: [&str; 6] = ["ROW", "ROWS", "OFFSET", "INDIRECT", "CELL", "ADDRESS"];

/// Names that answer differently in two engines even with one pinned clock.
///
/// The partitioned path evaluates a formula in a mini-workbook and the
/// whole-file path evaluates it again, so anything that does not reproduce
/// across two engines makes the two paths disagree. `NOW` and `TODAY` are
/// Excel-volatile but they do reproduce, because `clock` pins one instant for
/// every workbook a call builds; they are therefore not listed here.
///
/// `RAND` and its relatives draw new numbers per evaluation. `INFO` reports
/// workbook-level facts such as the number of open sheets, which differ in a
/// mini-workbook holding one batch.
const NONDETERMINISTIC_FNS: [&str; 4] = ["RAND", "RANDARRAY", "RANDBETWEEN", "INFO"];

/// Names whose uncached cost grows with the height of the table they read.
///
/// The engine can index repeated exact lookups. Other lookup modes can still
/// scan the table. See `Topology::lookup_work`.
const LOOKUP_FNS: [&str; 6] = ["VLOOKUP", "HLOOKUP", "LOOKUP", "MATCH", "XLOOKUP", "XMATCH"];

/// Aggregate calls whose criterion takes the engine's text-lane path.
///
/// A text criterion such as `COUNTIF(range,"1")` matches through the
/// engine's lowered-text lane. At the pinned engine rev the base lane and
/// the overlay lane render a cell the same way: text lowercased, numbers
/// and booleans to their string forms. A text criterion therefore answers
/// identically in a loader-built workbook and an incrementally-built one
/// (a batch, a scratch probe), and follows Excel coercion, so these calls
/// partition. The counter is diagnostic; the numeric-comparison split is
/// kept because `">0"`-style criteria never touch the lane. `COUNTBLANK`
/// counts nulls directly and is not counted.
const TEXT_CRITERIA_FNS: [&str; 6] =
    ["COUNTIF", "COUNTIFS", "SUMIF", "SUMIFS", "AVERAGEIF", "AVERAGEIFS"];

/// Functions whose result can be an array even when every argument is a
/// scalar, plus lookup-style calls that spill when an index argument
/// evaluates to zero (`INDEX(range,0)`).
const ARRAY_FNS: [&str; 23] = [
    "INDEX", "XLOOKUP", "SEQUENCE", "MUNIT", "RANDARRAY", "TEXTSPLIT", "SORT", "SORTBY",
    "UNIQUE", "FILTER", "TRANSPOSE", "MAKEARRAY", "CHOOSECOLS", "CHOOSEROWS", "TOCOL",
    "TOROW", "DROP", "TAKE", "EXPAND", "VSTACK", "HSTACK", "WRAPROWS", "WRAPCOLS",
];

/// Functions that reduce their arguments to a scalar, so an array inside
/// them cannot reach the result. `IF` and `CHOOSE` are deliberately absent:
/// they pass a branch through. Unknown functions are treated as passing
/// their arguments through, which keeps the screen free of false
/// negatives.
const REDUCING_FNS: [&str; 46] = [
    "SUM", "COUNT", "COUNTA", "COUNTBLANK", "MIN", "MAX", "MINA", "MAXA", "AVERAGE",
    "AVERAGEA", "MEDIAN", "MODE", "PRODUCT", "SUMPRODUCT", "SUMIF", "SUMIFS", "COUNTIF",
    "COUNTIFS", "AVERAGEIF", "AVERAGEIFS", "MINIFS", "MAXIFS", "VLOOKUP", "HLOOKUP",
    "LOOKUP", "MATCH", "XMATCH", "LARGE", "SMALL", "STDEV", "STDEVA", "VAR", "VARA",
    "ABS", "INT", "MOD", "ROUND", "SQRT", "TEXT", "LEFT", "RIGHT", "MID", "LEN",
    "CONCAT", "CONCATENATE", "ISNUMBER",
];

/// Whether a formula source can produce a spilled array.
///
/// The screen is a superset of the spilling shapes, so it never misses one:
/// a result is an array only when a range, a named range or an array
/// literal flows to it, or when one of `ARRAY_FNS` produces it from
/// scalars. A range inside `REDUCING_FNS` is consumed and cannot reach the
/// result, so the common `SUM(A1:A3)` shape is not flagged. It
/// over-approximates elsewhere: `INDEX(A1,1)` is flagged although it
/// returns a scalar, and an unknown function is treated as passing its
/// arguments through. A false positive costs only an inspection of the
/// anchors a batch places; a false negative would make a spill go
/// unnoticed.
pub fn array_capable_ast(node: &ASTNode) -> bool {
    match &node.node_type {
        ASTNodeType::Reference { reference, .. } => matches!(
            reference,
            ReferenceType::Range { .. } | ReferenceType::NamedRange(_)
        ),
        ASTNodeType::UnaryOp { expr, .. } => array_capable_ast(expr),
        ASTNodeType::BinaryOp { left, right, .. } => {
            array_capable_ast(left) || array_capable_ast(right)
        }
        ASTNodeType::Function { name, args } => {
            if ARRAY_FNS.iter().any(|f| name.eq_ignore_ascii_case(f)) {
                return true;
            }
            if REDUCING_FNS.iter().any(|f| name.eq_ignore_ascii_case(f)) {
                return false;
            }
            args.iter().any(array_capable_ast)
        }
        ASTNodeType::Call { callee, args } => {
            array_capable_ast(callee) || args.iter().any(array_capable_ast)
        }
        ASTNodeType::Array(_) => true,
        _ => false,
    }
}

/// Repeated aggregate and lookup formulas usually read the same large range.
/// Keep this cache bounded so row-relative ranges cannot make memory scale with
/// the number of formula cells.
const RANGE_CACHE_CAPACITY: usize = 256;

/// Slice a sorted `(major_axis, minor_axis)` position index to one inclusive
/// interval on its major axis.
fn major_axis_range(index: &[(u32, u32)], lo: u32, hi: u32) -> &[(u32, u32)] {
    let start = index.partition_point(|&(major, _)| major < lo);
    let end = index.partition_point(|&(major, _)| major <= hi);
    &index[start..end]
}

/// Count the function calls that block a fast path, in one walk.
///
/// `dynamic` counts references that only exist at evaluation time.
/// `row_sensitive` counts calls whose result depends on the formula position.
/// `nondeterministic` counts calls that two engines cannot be made to agree on.
/// `text_criteria` counts aggregate calls whose criterion takes the
/// engine's text-lane path.
fn classify(
    node: &ASTNode,
    dynamic: &mut u64,
    row_sensitive: &mut u64,
    nondeterministic: &mut u64,
    text_criteria: &mut u64,
) {
    match &node.node_type {
        ASTNodeType::Function { name, args } => {
            if DYNAMIC_FNS.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                *dynamic += 1;
            }
            if ROW_SENSITIVE_FNS.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                *row_sensitive += 1;
            }
            if NONDETERMINISTIC_FNS.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                *nondeterministic += 1;
            }
            if TEXT_CRITERIA_FNS.iter().any(|d| name.eq_ignore_ascii_case(d))
                && has_unsafe_criteria_text(node)
            {
                *text_criteria += 1;
            }
            for a in args {
                classify(a, dynamic, row_sensitive, nondeterministic, text_criteria);
            }
        }
        ASTNodeType::UnaryOp { expr, .. } => {
            classify(expr, dynamic, row_sensitive, nondeterministic, text_criteria)
        }
        ASTNodeType::BinaryOp { left, right, .. } => {
            classify(left, dynamic, row_sensitive, nondeterministic, text_criteria);
            classify(right, dynamic, row_sensitive, nondeterministic, text_criteria);
        }
        ASTNodeType::Call { callee, args } => {
            classify(callee, dynamic, row_sensitive, nondeterministic, text_criteria);
            for a in args {
                classify(a, dynamic, row_sensitive, nondeterministic, text_criteria);
            }
        }
        ASTNodeType::Array(rows) => {
            for row in rows {
                for a in row {
                    classify(a, dynamic, row_sensitive, nondeterministic, text_criteria);
                }
            }
        }
        _ => {}
    }
}

/// Whether a call subtree holds a criterion that takes the text-lane
/// path.
///
/// A text literal is lane-bound unless it is a numeric comparison such as
/// `">0"`. A computed criterion such as `IF(A1="x","",">0")` can answer a
/// text at evaluation time, so any text literal anywhere in the subtree
/// counts; a text outside the call is a value, not a criterion, and is
/// correctly ignored. A criterion passed as a bare cell reference
/// (`COUNTIF(range,B1)`) takes the same lane but carries no literal, so it
/// was never counted — the counter was never a complete list of lane-bound
/// calls, and the engine fix does not depend on it.
fn has_unsafe_criteria_text(node: &ASTNode) -> bool {
    match &node.node_type {
        ASTNodeType::Literal(LiteralValue::Text(s)) => !is_numeric_comparison(s),
        ASTNodeType::UnaryOp { expr, .. } => has_unsafe_criteria_text(expr),
        ASTNodeType::BinaryOp { left, right, .. } => {
            has_unsafe_criteria_text(left) || has_unsafe_criteria_text(right)
        }
        ASTNodeType::Function { args, .. } => args.iter().any(has_unsafe_criteria_text),
        ASTNodeType::Call { callee, args } => {
            has_unsafe_criteria_text(callee) || args.iter().any(has_unsafe_criteria_text)
        }
        ASTNodeType::Array(rows) => rows.iter().flatten().any(has_unsafe_criteria_text),
        _ => false,
    }
}

/// Whether a criteria text is a comparison against a number, such as
/// `">0"` or `"<=5.5"`. Numeric comparisons take the numeric predicate
/// path and never touch the text lane; every other text criterion takes
/// the text-lane path.
fn is_numeric_comparison(s: &str) -> bool {
    let t = s.trim();
    let rhs = if let Some(r) = t.strip_prefix(">=") {
        r
    } else if let Some(r) = t.strip_prefix("<=") {
        r
    } else if let Some(r) = t.strip_prefix("<>") {
        r
    } else if let Some(r) = t.strip_prefix("=") {
        r
    } else if let Some(r) = t.strip_prefix(">") {
        r
    } else if let Some(r) = t.strip_prefix("<") {
        r
    } else {
        return false;
    };
    rhs.trim().parse::<f64>().is_ok()
}

fn resolve_defined_name(
    definitions: &[DefinedName],
    name: &str,
    own_sheet: u16,
) -> Result<(usize, RangeRef), ()> {
    let mut workbook = None;
    let mut local = None;
    for (i, definition) in definitions.iter().enumerate() {
        if !definition.name.eq_ignore_ascii_case(name) {
            continue;
        }
        match definition.scope {
            NameScope::Invalid => return Err(()),
            NameScope::Workbook => {
                if workbook.replace(i).is_some() {
                    return Err(());
                }
            }
            NameScope::Sheet(sheet) if sheet == own_sheet => {
                if local.replace(i).is_some() {
                    return Err(());
                }
            }
            NameScope::Sheet(_) => {}
        }
    }
    let i = local.or(workbook).ok_or(())?;
    definitions[i].target.map(|target| (i, target)).ok_or(())
}

/// Look a sheet name up case-insensitively. Excel sheet names are
/// case-insensitive; the index in `build_from` is keyed lowercase.
fn sheet_lookup(sheet_idx: &HashMap<String, u16>, name: &str) -> Option<u16> {
    sheet_idx.get(&name.to_lowercase()).copied()
}

/// Rows one formula reads in its lookup calls.
///
/// Each call is charged the height of the tallest range it names. A range that
/// leaves a row side open, such as `Lookup!A:B`, is charged the data extent of
/// the sheet it reads, because that is how far the engine scans.
fn lookup_rows_of(
    node: &ASTNode,
    sheets: &[SheetInfo],
    sheet_idx: &HashMap<String, u16>,
    definitions: &[DefinedName],
    own_sheet: u16,
) -> u64 {
    let mut total = 0u64;
    match &node.node_type {
        ASTNodeType::Function { name, args } => {
            if LOOKUP_FNS.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                total += args
                    .iter()
                    .map(|a| range_rows(a, sheets, sheet_idx, definitions, own_sheet))
                    .max()
                    .unwrap_or(0);
            }
            for a in args {
                total += lookup_rows_of(a, sheets, sheet_idx, definitions, own_sheet);
            }
        }
        ASTNodeType::UnaryOp { expr, .. } => {
            total += lookup_rows_of(expr, sheets, sheet_idx, definitions, own_sheet)
        }
        ASTNodeType::BinaryOp { left, right, .. } => {
            total += lookup_rows_of(left, sheets, sheet_idx, definitions, own_sheet);
            total += lookup_rows_of(right, sheets, sheet_idx, definitions, own_sheet);
        }
        ASTNodeType::Call { callee, args } => {
            total += lookup_rows_of(callee, sheets, sheet_idx, definitions, own_sheet);
            for a in args {
                total += lookup_rows_of(a, sheets, sheet_idx, definitions, own_sheet);
            }
        }
        ASTNodeType::Array(rows) => {
            for row in rows {
                for a in row {
                    total += lookup_rows_of(a, sheets, sheet_idx, definitions, own_sheet);
                }
            }
        }
        _ => {}
    }
    total
}

/// How many rows one argument of a lookup call covers.
fn range_rows(
    node: &ASTNode,
    sheets: &[SheetInfo],
    sheet_idx: &HashMap<String, u16>,
    definitions: &[DefinedName],
    own_sheet: u16,
) -> u64 {
    let ASTNodeType::Reference { reference, .. } = &node.node_type else {
        return 0;
    };
    match reference {
        ReferenceType::Cell { .. } => 1,
        ReferenceType::Range { sheet, start_row, end_row, .. } => {
            let si = match sheet {
                Some(s) => match sheet_lookup(sheet_idx, s.as_str()) {
                    Some(v) => v,
                    None => own_sheet,
                },
                None => own_sheet,
            };
            let extent = sheets.get(si as usize).map(|s| s.data_row).unwrap_or(0) as u64;
            match (start_row, end_row) {
                (Some(a), Some(b)) => (b.saturating_sub(*a) as u64) + 1,
                _ => extent,
            }
        }
        ReferenceType::NamedRange(name) => resolve_defined_name(definitions, name, own_sheet)
            .map(|(_, (_, r0, _, r1, _))| (r1 - r0 + 1) as u64)
            .unwrap_or(0),
        _ => 0,
    }
}

/// Read the workbook and group its formula cells into dependency components.
pub fn build(data: &[u8]) -> Topology {
    build_from(read(data))
}

/// Group the formula cells of an already-read workbook into components.
///
/// Only formula-to-formula edges merge components. A data precedent widens a
/// component's bounding box but never joins two formulas, so a shared input
/// cell cannot glue unrelated components together.
///
/// The references are collected here, and not in the read stage, because the
/// prelude stage can replace a formula's text between the two stages.
pub fn build_from(src: Sources) -> Topology {
    let Sources {
        calc_settings,
        sheets,
        cells,
        texts,
        anchors,
        values,
        blanks,
        defined_names,
        name_only_sheets,
        dynamic_refs,
        mut parse_errors,
        mut first_parse_error,
        xml_formula_cells,
        array_formulas,
        nondeterministic_fns,
        row_sensitive_fns,
        text_criteria_ifs,
        array_capable,
        t_read_ms,
    } = src;

    let t1 = std::time::Instant::now();
    // Excel sheet names are case-insensitive, so the index is keyed
    // lowercase. A formula that names a sheet spelling the workbook does not
    // hold must fall back, never silently drop the edge (see `visit`).
    let sheet_idx: HashMap<String, u16> = sheets
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name.to_lowercase(), i as u16))
        .collect();

    // The sheet each template sits on. A lookup range that leaves a row side
    // open is charged against the data extent of the sheet it reads, and that
    // sheet is the formula's own one unless the range names another.
    let mut template_sheet: Vec<u16> = vec![0; texts.len()];
    for fc in &cells {
        template_sheet[fc.ast as usize] = fc.sheet;
    }

    // Each distinct formula is parsed once more here, and the tree is dropped
    // as soon as its references are taken. Template compression keeps this
    // count near the number of formula columns, not the number of cells.
    let mut ast_refs: Vec<Vec<RawRef>> = Vec::with_capacity(texts.len());
    let mut lookup_rows: Vec<u64> = Vec::with_capacity(texts.len());
    for (k, text) in texts.iter().enumerate() {
        match parse(text) {
            Ok(ast) => {
                ast_refs.push(collect_refs(&ast));
                lookup_rows.push(lookup_rows_of(
                    &ast,
                    &sheets,
                    &sheet_idx,
                    &defined_names,
                    template_sheet[k],
                ));
            }
            Err(error) => {
                parse_errors += 1;
                first_parse_error.get_or_insert_with(|| ParseFailure {
                    stage: "graph",
                    sheet: sheets[template_sheet[k] as usize].name.clone(),
                    row: anchors[k].0,
                    col: anchors[k].1,
                    formula: text.to_string(),
                    error: error.to_string(),
                });
                ast_refs.push(Vec::new());
                lookup_rows.push(0);
            }
        }
    }

    let n = cells.len();
    let mut index: HashMap<(u16, u32, u32), u32> = HashMap::with_capacity(n * 2);
    let mut per_sheet_by_row: Vec<Vec<(u32, u32)>> = vec![Vec::new(); sheets.len()];
    let mut per_sheet_by_col: Vec<Vec<(u32, u32)>> = vec![Vec::new(); sheets.len()];
    for (i, fc) in cells.iter().enumerate() {
        index.insert((fc.sheet, fc.row, fc.col), i as u32);
        per_sheet_by_row[fc.sheet as usize].push((fc.row, fc.col));
        per_sheet_by_col[fc.sheet as usize].push((fc.col, fc.row));
    }
    for positions in &mut per_sheet_by_row {
        positions.sort_unstable();
    }
    for positions in &mut per_sheet_by_col {
        positions.sort_unstable();
    }

    let mut dsu = Dsu::new(n);
    let mut range_representatives: HashMap<RangeRef, Option<u32>> = HashMap::new();
    let mut range_cache_order: VecDeque<RangeRef> = VecDeque::new();
    let mut local: Vec<BBox> = cells.iter().map(|c| BBox::point(c.row, c.col)).collect();
    let mut refs_of: Vec<Vec<RangeRef>> = vec![Vec::new(); n];
    let mut cross_sheet = false;
    let mut cross_row = false;
    let mut unsupported_refs: u64 = 0;
    let mut named_refs: u64 = 0;
    let mut used_names: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut self_refs: u64 = 0;

    for i in 0..n {
        let fc = &cells[i];
        let own_sheet = fc.sheet;
        let (dr, dc) = (fc.dr, fc.dc);
        let mut edges: Vec<RangeRef> = Vec::new();
        let mut visit = |rv: &RawRef| match rv {
            RawRef::Cell { sheet, row, col, row_abs, col_abs } => {
                let (row, col, row_abs, col_abs) = (*row, *col, *row_abs, *col_abs);
                // A formula can carry a broken reference (`VLOOKUP(x,#REF!,2,0)`),
                // which parses with no usable coordinates. Shifting it would
                // invent an address, so it counts as unresolvable instead.
                if row == 0 || col == 0 {
                    unsupported_refs += 1;
                    return;
                }
                let psi = match sheet {
                    Some(s) => match sheet_lookup(&sheet_idx, s.as_ref()) {
                        Some(v) => v,
                        // A phantom spelling (`Ind&Man` beside `Ind & Man`)
                        // or a case mismatch is not an empty read. Count it
                        // so the file falls back instead of under-reading.
                        None => {
                            unsupported_refs += 1;
                            return;
                        }
                    },
                    None => own_sheet,
                };
                match (shift(row, dr, row_abs), shift(col, dc, col_abs)) {
                    (Some(r), Some(c)) => edges.push((psi, r, c, r, c)),
                    // A shared-formula member shifted off the sheet reads
                    // `#REF!` in Excel; dropping the edge would read blank.
                    _ => {
                        unsupported_refs += 1;
                    }
                }
            }
            RawRef::Range {
                sheet,
                start_row,
                start_col,
                end_row,
                end_col,
                start_row_abs,
                start_col_abs,
                end_row_abs,
                end_col_abs,
            } => {
                let (start_row, start_col, end_row, end_col) =
                    (*start_row, *start_col, *end_row, *end_col);
                let (start_row_abs, start_col_abs, end_row_abs, end_col_abs) =
                    (*start_row_abs, *start_col_abs, *end_row_abs, *end_col_abs);
                let psi = match sheet {
                    Some(s) => match sheet_lookup(&sheet_idx, s.as_ref()) {
                        Some(v) => v,
                        None => {
                            unsupported_refs += 1;
                            return;
                        }
                    },
                    None => own_sheet,
                };
                if start_row == Some(0)
                    || start_col == Some(0)
                    || end_row == Some(0)
                    || end_col == Some(0)
                {
                    unsupported_refs += 1;
                    return;
                }
                // An open range always pins one axis: `A:A` fixes the columns,
                // `1:1` the rows. All four sides open is not a whole-sheet
                // reference, it is a broken one (`#REF!`), and clamping it to
                // the sheet extent would make a formula read its own cell.
                if start_row.is_none()
                    && end_row.is_none()
                    && start_col.is_none()
                    && end_col.is_none()
                {
                    unsupported_refs += 1;
                    return;
                }
                let dims = &sheets[psi as usize];
                // An open side (A:A) means the whole column or row; clamp it to
                // the sheet extent so the range stays finite. A specified side
                // that shifts off the sheet is `#REF!`, not row 1: clamping it
                // would read a cell the formula never sees.
                let (Some(r0), Some(c0), Some(r1), Some(c1)) = (
                    shifted_side(start_row, dr, start_row_abs),
                    shifted_side(start_col, dc, start_col_abs),
                    shifted_side(end_row, dr, end_row_abs),
                    shifted_side(end_col, dc, end_col_abs),
                ) else {
                    unsupported_refs += 1;
                    return;
                };
                let r0 = r0.unwrap_or(1);
                let c0 = c0.unwrap_or(1);
                let r1 = r1.unwrap_or(dims.max_row.max(1));
                let c1 = c1.unwrap_or(dims.max_col.max(1));
                edges.push((psi, r0.min(r1), c0.min(c1), r0.max(r1), c0.max(c1)));
            }
            RawRef::Named { name } => match resolve_defined_name(&defined_names, name, own_sheet) {
                Ok((definition, target)) => {
                    named_refs += 1;
                    used_names.insert(definition);
                    edges.push(target);
                }
                Err(()) => unsupported_refs += 1,
            },
            RawRef::Unsupported => unsupported_refs += 1,
        };
        for rv in &ast_refs[fc.ast as usize] {
            visit(rv);
        }

        for &(psi, r0, c0, r1, c1) in &edges {
            if psi == own_sheet && (r0..=r1).contains(&fc.row) && (c0..=c1).contains(&fc.col) {
                self_refs += 1;
            }
            if psi != own_sheet {
                cross_sheet = true;
            }
            if r0 != fc.row || r1 != fc.row {
                cross_row = true;
            }
            local[i].expand(r0, c0);
            local[i].expand(r1, c1);
            if r0 == r1 && c0 == c1 {
                if let Some(&j) = index.get(&(psi, r0, c0)) {
                    dsu.union(i as u32, j);
                }
                continue;
            }

            let range = (psi, r0, c0, r1, c1);
            if let Some(representative) = range_representatives.get(&range).copied() {
                // `None` means the range contains data cells only. Sharing
                // those cells must not glue otherwise independent formulas.
                if let Some(j) = representative {
                    dsu.union(i as u32, j);
                }
                continue;
            }

            let mut representative = None;
            let row_span = r1 - r0;
            let col_span = c1 - c0;
            if row_span <= col_span {
                // A short row range is cheapest through the row-major index.
                for &(rr, cc) in major_axis_range(&per_sheet_by_row[psi as usize], r0, r1) {
                    if cc >= c0 && cc <= c1 {
                        let &j = index
                            .get(&(psi, rr, cc))
                            .expect("formula position index must stay in sync");
                        representative.get_or_insert(j);
                        dsu.union(i as u32, j);
                    }
                }
            } else {
                // Whole-column and other narrow-column ranges avoid scanning
                // every formula in every covered row.
                for &(cc, rr) in major_axis_range(&per_sheet_by_col[psi as usize], c0, c1) {
                    if rr >= r0 && rr <= r1 {
                        let &j = index
                            .get(&(psi, rr, cc))
                            .expect("formula position index must stay in sync");
                        representative.get_or_insert(j);
                        dsu.union(i as u32, j);
                    }
                }
            }

            // The first scan united every formula inside the range. A later
            // consumer can join that component through one representative;
            // an empty range remains cached as `None` and joins nothing.
            if range_representatives.len() == RANGE_CACHE_CAPACITY {
                if let Some(oldest) = range_cache_order.pop_front() {
                    range_representatives.remove(&oldest);
                }
            }
            range_representatives.insert(range, representative);
            range_cache_order.push_back(range);
        }
        refs_of[i] = edges;
    }

    // Compact DSU roots into dense component ids.
    //
    // This map is probed once per formula cell, but it holds one entry per
    // *component*, not per cell: 3,612 entries for 265,587 formula cells on a
    // measured workbook. It therefore stays in cache and the probes are
    // already cheap. Replacing it with a direct-addressed `Vec<u32>` sentinel
    // table of length `n` was measured and was not an improvement: build went
    // from 371-399 ms to 388-416 ms and build memory from +94.6 MB to
    // +95.3 MB, because the table allocates 1.06 MB to replace a map costing a
    // fraction of that. Keep the map.
    let mut dense: HashMap<u32, u32> = HashMap::new();
    let mut comp_of = vec![0u32; n];
    for (i, comp) in comp_of.iter_mut().enumerate() {
        let root = dsu.find(i as u32);
        let next = dense.len() as u32;
        *comp = *dense.entry(root).or_insert(next);
    }
    let n_comp = dense.len();

    let mut comp_cells: Vec<Vec<u32>> = vec![Vec::new(); n_comp];
    let mut comp_refs: Vec<Vec<RangeRef>> = vec![Vec::new(); n_comp];
    // Bounding box per (component, sheet) so a cross-sheet component sums its
    // per-sheet boxes instead of spanning a meaningless union.
    //
    // Keyed by (component, sheet) rather than by cell, so like `dense` above
    // this map is small and cache-resident. Rebuilding the same sums with a
    // per-component linear scan over `comp_cells` was measured alongside the
    // `dense` change and showed no gain either. The extents are summed as
    // `u64`, so iteration order cannot affect the result and this map does not
    // need an ordered type.
    let mut boxes: HashMap<(u32, u16), BBox> = HashMap::new();
    for i in 0..n {
        let c = comp_of[i] as usize;
        comp_cells[c].push(i as u32);
        comp_refs[c].append(&mut refs_of[i]);
        boxes
            .entry((comp_of[i], cells[i].sheet))
            .and_modify(|b| b.merge(&local[i]))
            .or_insert(local[i]);
    }
    let mut comp_extent = vec![0u64; n_comp];
    for ((c, _), b) in &boxes {
        comp_extent[*c as usize] += b.cells();
    }

    // Collapsing duplicate ranges matters when thousands of formulas read the
    // same lookup table: it saves the copy step from reading that table once
    // per formula, and the batch budget charges it once (see `plan_batches`).
    for refs in &mut comp_refs {
        refs.sort_unstable();
        refs.dedup();
    }

    let full_extent_cells: u64 = sheets
        .iter()
        .map(|s| s.max_row as u64 * s.max_col as u64)
        .sum();

    let mut used_names: Vec<usize> = used_names.into_iter().collect();
    used_names.sort_unstable();
    let static_names = used_names
        .into_iter()
        .map(|i| StaticName {
            name: defined_names[i].name.clone(),
            scope: defined_names[i].scope,
            target: defined_names[i].target.expect("a resolved name has a target"),
        })
        .collect();

    Topology {
        calc_settings,
        sheets,
        cells,
        texts,
        anchors,
        values,
        blanks,
        ast_refs,
        static_names,
        named_refs,
        lookup_rows,
        comp_of,
        comp_cells,
        comp_refs,
        comp_extent,
        index,
        name_only_sheets,
        full_extent_cells,
        cross_sheet,
        cross_row,
        unsupported_refs,
        dynamic_refs,
        array_formulas,
        self_refs,
        parse_errors,
        first_parse_error,
        xml_formula_cells,
        nondeterministic_fns,
        row_sensitive_fns,
        text_criteria_ifs,
        array_capable,
        t_read_ms,
        t_graph_ms: t1.elapsed().as_secs_f64() * 1000.0,
    }
}

impl Topology {
    pub fn biggest_extent_cells(&self) -> u64 {
        self.comp_extent.iter().copied().max().unwrap_or(0)
    }

    /// Conservative upper bound for rows read by lookup calls.
    ///
    /// This charges each call the full height of its table. It does not account
    /// for the engine's index for repeated exact lookups. See
    /// `partition::DEFAULT_LOOKUP_BUDGET`.
    pub fn lookup_work(&self) -> u64 {
        self.cells
            .iter()
            .map(|c| self.lookup_rows[c.ast as usize])
            .sum()
    }

    /// Whether every reference was resolved statically, which is the
    /// precondition for evaluating components in isolation.
    pub fn is_partitionable(&self) -> bool {
        self.parse_errors == 0
            && self.xml_formula_cells == self.cells.len() as u64
            && self.unsupported_refs == 0
            && self.nondeterministic_fns == 0
            && self.dynamic_refs == 0
            && self.self_refs == 0
    }
}

pub fn analyze(data: &[u8]) -> Analysis {
    let t = build(data);
    let mut top: Vec<(u64, u32, u32)> = t
        .comp_extent
        .iter()
        .enumerate()
        .map(|(c, &ext)| {
            let cells = &t.comp_cells[c];
            let (mut r0, mut r1, mut c0, mut c1) = (u32::MAX, 0, u32::MAX, 0);
            for &i in cells {
                let fc = &t.cells[i as usize];
                r0 = r0.min(fc.row);
                r1 = r1.max(fc.row);
                c0 = c0.min(fc.col);
                c1 = c1.max(fc.col);
            }
            (ext, r1 - r0 + 1, c1 - c0 + 1)
        })
        .collect();
    top.sort_by(|a, b| b.0.cmp(&a.0));
    let biggest_extent_cells = t.biggest_extent_cells();
    top.truncate(8);

    Analysis {
        n_formula_cells: t.cells.len(),
        n_components: t.comp_cells.len(),
        n_parsed: t.texts.len(),
        full_extent_cells: t.full_extent_cells,
        biggest_extent_cells,
        cross_sheet: t.cross_sheet,
        cross_row: t.cross_row,
        unsupported_refs: t.unsupported_refs,
        parse_errors: t.parse_errors,
        first_parse_error: t.first_parse_error,
        xml_formula_cells: t.xml_formula_cells,
        top,
        t_read_ms: t.t_read_ms,
        t_graph_ms: t.t_graph_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{cell_f, cell_v, xlsx, xlsx_with_defined_names};

    #[test]
    fn array_capable_screen_covers_every_spilling_shape() {
        for formula in [
            "=A1:A3",
            "=A1:A3+0",
            "=INDEX($A$1:$A$3,0)",
            "=INDEX(A1,0)",
            "=SEQUENCE(3)",
            "=IF(A1>0,{1,2},3)",
            "=NamedRange",
        ] {
            assert!(
                array_capable_ast(&parse(formula).unwrap()),
                "{formula} must be flagged"
            );
        }
        for formula in ["=A1+1", "=SUM(A1:A3)", "=VLOOKUP(A1,B1:C5,2,0)", "=IF(A1>0,1,2)"] {
            assert!(
                !array_capable_ast(&parse(formula).unwrap()),
                "{formula} must not be flagged"
            );
        }
    }

    #[test]
    fn formula_cdata_is_concatenated_without_entity_decoding() {
        let data = xlsx(&[("S", &format!("{}{}{}",
            cell_v("A1", "4"),
            cell_f("B1", "A1+<![CDATA[1]]>"),
            cell_f("C1", "<![CDATA[\"&amp;\"]]>"),
        ))]);
        let t = build(&data);
        assert_eq!(t.parse_errors, 0);
        assert_eq!(t.xml_formula_cells, 2);
        assert_eq!(t.cells.len(), 2);
        assert_eq!(t.texts[0].as_ref(), "=A1+1");
        assert_eq!(t.texts[1].as_ref(), "=\"&amp;\"");
    }

    #[test]
    fn parse_failure_keeps_source_location_and_stage() {
        let data = xlsx(&[("S", &format!("{}{}", cell_f("B7", "A1+"), cell_f("B8", "A2+")))]);
        let t = build(&data);
        assert_eq!(t.xml_formula_cells, 2);
        assert_eq!(t.parse_errors, 2);
        assert!(t.cells.is_empty());
        let f = t.first_parse_error.unwrap();
        assert_eq!((f.stage, f.sheet.as_str(), f.row, f.col), ("read", "S", 7, 2));
        assert_eq!(f.formula, "=A1+");
        assert!(!f.error.is_empty());

        let mut src = read(&xlsx(&[("S", &cell_f("C9", "1+2"))]));
        src.texts[0] = "=1+".into();
        let t = build_from(src);
        assert_eq!(t.parse_errors, 1);
        let f = t.first_parse_error.unwrap();
        assert_eq!((f.stage, f.row, f.col), ("graph", 9, 3));
        assert_eq!(f.formula, "=1+");
        assert!(!f.error.is_empty());
    }

    #[test]
    fn xml_count_includes_shared_and_unresolved_formulas() {
        let data = xlsx(&[("S", r#"<c r="A1"><f t="shared" si="0">1</f></c>
            <c r="A2"><f t="shared" si="0"/></c>
            <c r="A3"><f t="shared" si="0"></f></c>
            <c r="A4"><f t="shared" si="99"/></c>
            <c r="A5"><f/></c>"#)]);
        let t = build(&data);
        assert_eq!(t.xml_formula_cells, 5);
        assert_eq!(t.cells.len(), 3);
    }

    #[test]
    fn valueless_declared_cells_are_recorded_as_blanks() {
        // Self-closing and Start/End forms both record. Valued cells and
        // formula cells, with or without a cached value, never do: a
        // formula cell is not a blank even when its `<v>` is missing.
        let sheet = format!(
            "{}{}{}{}{}",
            r#"<c r="A1"/>"#,
            r#"<c r="A2" s="5"></c>"#,
            cell_v("A3", "7"),
            r#"<c r="A4"><f>B4</f></c>"#,
            cell_f("A5", "B5"),
        );
        let src = read(&xlsx(&[("S", &sheet)]));
        assert_eq!(src.blanks.len(), 1);
        assert_eq!(src.blanks[0], vec![(1, 1), (2, 1)]);
        assert!(src.values[0].iter().any(|&(r, c, _)| (r, c) == (3, 1)));
        assert!(
            !src.values[0].iter().any(|&(r, c, _)| (r, c) == (1, 1) || (r, c) == (2, 1)),
            "blanks carry no value: {:?}",
            src.values[0]
        );
    }

    #[test]
    fn parse_a1_handles_absolute_and_multiletter() {
        assert_eq!(parse_a1("A1"), Some((1, 1)));
        assert_eq!(parse_a1("$A$1"), Some((1, 1)));
        assert_eq!(parse_a1("B12"), Some((12, 2)));
        assert_eq!(parse_a1("AA3"), Some((3, 27)));
        assert_eq!(parse_a1("XFD1048576"), Some((1048576, 16384)));
        assert_eq!(parse_a1("A"), None);
        assert_eq!(parse_a1("1"), None);
        assert_eq!(parse_a1("1A"), None, "digits before letters is not A1");
    }

    #[test]
    fn shift_moves_relative_and_pins_absolute() {
        assert_eq!(shift(10, 5, false), Some(15));
        assert_eq!(shift(10, -3, false), Some(7));
        assert_eq!(shift(10, 5, true), Some(10), "absolute ignores offset");
        assert_eq!(shift(1, -1, false), None, "row 0 is off-sheet");
        assert_eq!(shift(1, -1, true), Some(1));
    }

    #[test]
    fn dsu_merges_transitively_and_keeps_groups_apart() {
        let mut d = Dsu::new(6);
        d.union(0, 1);
        d.union(1, 2);
        d.union(4, 5);
        assert_eq!(d.find(0), d.find(2), "0-1-2 is one set");
        assert_ne!(d.find(0), d.find(3), "3 stays alone");
        assert_ne!(d.find(0), d.find(4));
        assert_eq!(d.find(4), d.find(5));
        d.union(2, 5);
        assert_eq!(d.find(0), d.find(4), "sets merge through a shared member");
    }

    #[test]
    fn bbox_grows_to_cover_points_and_boxes() {
        let mut b = BBox::point(3, 3);
        assert_eq!(b.cells(), 1);
        b.expand(5, 4);
        assert_eq!((b.min_r, b.min_c, b.max_r, b.max_c), (3, 3, 5, 4));
        assert_eq!(b.cells(), 3 * 2);
        let mut other = BBox::point(1, 1);
        other.merge(&b);
        assert_eq!((other.min_r, other.min_c, other.max_r, other.max_c), (1, 1, 5, 4));
    }

    /// Sheet XML stores formulas without '='. The parser treats a bare string
    /// as a literal and reports no references, so the reader must add the '='
    /// back before it parses.
    #[test]
    fn formula_text_without_equals_still_yields_edges() {
        let data = xlsx(&[(
            "S",
            &format!("{}{}", cell_v("A1", "1"), cell_f("B1", "A1+1")),
        )]);
        let a = analyze(&data);
        assert_eq!(a.n_formula_cells, 1);
        assert_eq!(a.parse_errors, 0);
        assert!(
            a.biggest_extent_cells > 1,
            "B1 must reach A1; got a singleton bbox, so no reference was parsed"
        );
    }

    #[test]
    fn shared_formula_members_expand_and_reuse_one_ast() {
        // One master at B2 covering B2:B4; B3/B4 are self-closing members.
        let sheet = format!(
            r#"{}{}{}{}{}{}"#,
            cell_v("A2", "1"),
            cell_v("A3", "2"),
            cell_v("A4", "3"),
            r#"<c r="B2"><f t="shared" ref="B2:B4" si="0">A2+1</f><v>2</v></c>"#,
            r#"<c r="B3"><f t="shared" si="0"/><v>3</v></c>"#,
            r#"<c r="B4"><f t="shared" si="0"/><v>4</v></c>"#,
        );
        let a = analyze(&xlsx(&[("S", &sheet)]));
        assert_eq!(a.n_formula_cells, 3, "master plus both members are cells");
        assert_eq!(a.n_parsed, 1, "the shared formula is parsed once");
        assert_eq!(a.n_components, 3, "each row is independent");
        assert!(!a.cross_row, "each member reads its own row after shifting");
    }

    #[test]
    fn row_local_formulas_stay_separate_components() {
        let mut sheet = String::new();
        for r in 1..=5 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}+1")));
        }
        let a = analyze(&xlsx(&[("S", &sheet)]));
        assert_eq!(a.n_formula_cells, 5);
        assert_eq!(a.n_components, 5, "no formula reads another formula");
        assert_eq!(a.biggest_extent_cells, 2, "each bbox spans A..B on one row");
        assert!(!a.cross_row);
    }

    /// A column written out row by row holds one shape. Only the first formula
    /// of that column is kept; the rest reuse it with a row offset.
    #[test]
    fn repeated_column_formulas_collapse_to_one_template() {
        let mut sheet = String::new();
        for r in 1..=50 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}*2")));
            sheet.push_str(&cell_f(&format!("C{r}"), &format!("IF(B{r}>4,\"x\",A{r})")));
        }
        let a = analyze(&xlsx(&[("S", &sheet)]));
        assert_eq!(a.n_formula_cells, 100);
        assert_eq!(a.n_parsed, 2, "one template per formula column");
        assert_eq!(a.n_components, 50, "compression must not join rows");
    }

    /// Compression is only allowed when the shapes agree. A column that mixes
    /// two shapes keeps a source for each of them.
    #[test]
    fn column_with_two_shapes_keeps_both_sources() {
        let mut sheet = String::new();
        for r in 1..=4 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
        }
        sheet.push_str(&cell_f("B1", "A1*2"));
        sheet.push_str(&cell_f("B2", "A2*2"));
        sheet.push_str(&cell_f("B3", "A3+99"));
        sheet.push_str(&cell_f("B4", "A4+99"));
        let a = analyze(&xlsx(&[("S", &sheet)]));
        assert_eq!(a.n_formula_cells, 4);
        assert_eq!(a.n_parsed, 2, "the shape change starts a new template");
    }

    /// An absolute reference must not move with the template. A column of
    /// `$A$1*2` is one template, and every member still reads row 1.
    #[test]
    fn template_keeps_absolute_references_pinned() {
        let mut sheet = cell_v("A1", "10");
        for r in 1..=6 {
            sheet.push_str(&cell_f(&format!("B{r}"), "$A$1*2"));
        }
        let t = build(&xlsx(&[("S", &sheet)]));
        assert_eq!(t.cells.len(), 6);
        assert_eq!(t.texts.len(), 1, "one template covers the column");
        assert_eq!(t.comp_cells.len(), 6, "a shared anchor must not join rows");
        // B6 reaches A1, so its box spans rows 1..6 and columns A..B.
        assert_eq!(t.biggest_extent_cells(), 6 * 2);
    }

    /// A template that would move off the sheet cannot match, so the candidate
    /// keeps its own source.
    #[test]
    fn template_that_moves_off_sheet_does_not_match() {
        let sheet = format!("{}{}", cell_f("B2", "A1+1"), cell_f("B1", "A1+1"));
        let t = build(&xlsx(&[("S", &sheet)]));
        assert_eq!(t.cells.len(), 2);
        assert_eq!(
            t.texts.len(),
            2,
            "B1 shifted from the B2 template would read row 0"
        );
    }

    /// A nondeterministic call and a position-sensitive call are reported,
    /// because the scratch path cannot accept either. `TODAY` is not one: the
    /// pinned clock makes it reproduce across engines.
    #[test]
    fn nondeterministic_and_row_sensitive_calls_are_counted() {
        let t = build(&xlsx(&[("S", &cell_f("B2", "A2+RAND()"))]));
        assert_eq!(t.nondeterministic_fns, 1);
        assert_eq!(t.row_sensitive_fns, 0);

        let t = build(&xlsx(&[("S", &cell_f("B2", "A2+TODAY()"))]));
        assert_eq!(t.nondeterministic_fns, 0, "a pinned clock reproduces TODAY");

        let t = build(&xlsx(&[("S", &cell_f("B2", "A2+ROW()"))]));
        assert_eq!(t.row_sensitive_fns, 1);

        let t = build(&xlsx(&[("S", &cell_f("B2", "A2*2"))]));
        assert_eq!(t.nondeterministic_fns, 0);
        assert_eq!(t.row_sensitive_fns, 0);
    }

    #[test]
    fn chain_collapses_into_one_component_spanning_all_rows() {
        // A1 is data; A2..A5 each read the row above. The component graph
        // cannot split this chain. The scratch path can carry its last result.
        let mut sheet = cell_v("A1", "1");
        for r in 2..=5 {
            sheet.push_str(&cell_f(&format!("A{r}"), &format!("A{}+1", r - 1)));
        }
        let a = analyze(&xlsx(&[("S", &sheet)]));
        assert_eq!(a.n_formula_cells, 4);
        assert_eq!(a.n_components, 1, "the chain is a single component");
        assert_eq!(a.biggest_extent_cells, 5, "bbox covers A1:A5");
        assert!(a.cross_row, "a chain must be reported as cross-row");
    }

    #[test]
    fn absolute_reference_is_not_shifted_for_shared_members() {
        // Every member must read $A$1, not its own row.
        let sheet = format!(
            r#"{}{}{}{}"#,
            cell_v("A1", "10"),
            r#"<c r="B2"><f t="shared" ref="B2:B4" si="0">$A$1*2</f><v>20</v></c>"#,
            r#"<c r="B3"><f t="shared" si="0"/><v>20</v></c>"#,
            r#"<c r="B4"><f t="shared" si="0"/><v>20</v></c>"#,
        );
        let a = analyze(&xlsx(&[("S", &sheet)]));
        assert_eq!(a.n_formula_cells, 3);
        assert_eq!(
            a.n_components, 3,
            "a shared data anchor must not glue the formulas together"
        );
        // B4 -> A1 spans rows 1..4 and cols A..B; a shifted ref would differ.
        assert_eq!(a.biggest_extent_cells, 4 * 2);
    }

    #[test]
    fn range_reference_merges_the_formulas_it_covers() {
        // C1 sums B1:B3, which are themselves formulas, so all four join.
        let mut sheet = String::new();
        for r in 1..=3 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}+1")));
        }
        sheet.push_str(&cell_f("C1", "SUM(B1:B3)"));
        let a = analyze(&xlsx(&[("S", &sheet)]));
        assert_eq!(a.n_formula_cells, 4);
        assert_eq!(a.n_components, 1, "the aggregate pulls every B cell in");
        assert_eq!(a.biggest_extent_cells, 3 * 3, "bbox covers A1:C3");
        assert!(a.cross_row);
    }

    #[test]
    fn chained_colon_merges_to_one_range() {
        // `SUM(I8:K8:M8)` means `SUM(I8:M8)`. Collecting the endpoints
        // separately dropped L8 from the closure without a fallback.
        let mut sheet = String::new();
        for c in ["I8", "J8", "K8", "L8", "M8"] {
            sheet.push_str(&cell_v(c, "1"));
        }
        sheet.push_str(&cell_f("N8", "SUM(I8:K8:M8)"));
        let t = build(&xlsx(&[("S", &sheet)]));
        assert_eq!(t.unsupported_refs, 0);
        let refs: Vec<_> = t.comp_refs.iter().flatten().collect();
        assert!(
            refs.iter().any(|&&(_, r0, c0, r1, c1)| (r0, c0, r1, c1) == (8, 9, 8, 13)),
            "chained colon covers I8:M8, got {refs:?}"
        );
    }

    #[test]
    fn unmergeable_colon_falls_back() {
        // An endpoint the merger cannot bound (here a function call) must
        // refuse the file, not collect a partial closure.
        let t = build(&xlsx(&[("S", &cell_f("A1", "SUM(A1:OFFSET(B1,1,0))"))]));
        assert_eq!(t.unsupported_refs, 1);
    }

    #[test]
    fn sheet_names_match_case_insensitively() {
        let main = cell_f("A1", "data!A1*2");
        let data = cell_v("A1", "1");
        let t = build(&xlsx(&[("S", &main), ("Data", &data)]));
        assert_eq!(t.unsupported_refs, 0);
        assert!(
            t.comp_refs.iter().flatten().any(|&(s, _, _, _, _)| s == 1),
            "lowercase reference reaches the Data sheet"
        );
    }

    #[test]
    fn unknown_sheet_name_counts_unsupported() {
        // A sheet spelling the workbook does not hold is not an empty read.
        let t = build(&xlsx(&[("S", &cell_f("A1", "Nope!A1*2"))]));
        assert_eq!(t.unsupported_refs, 1);
    }

    #[test]
    fn repeated_data_only_range_does_not_merge_consumers() {
        let mut sheet = String::new();
        for r in 1..=3 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
        }
        sheet.push_str(&cell_f("B1", "SUM(A:A)"));
        sheet.push_str(&cell_f("C1", "SUM(A:A)"));

        let a = analyze(&xlsx(&[("S", &sheet)]));
        assert_eq!(a.n_formula_cells, 2);
        assert_eq!(
            a.n_components, 2,
            "sharing cached data-only range A:A must not join B1 and C1"
        );
    }

    #[test]
    fn repeated_formula_range_joins_cached_consumers() {
        let mut sheet = String::new();
        for r in 1..=3 {
            sheet.push_str(&cell_v(&format!("A{r}"), "1"));
            sheet.push_str(&cell_f(&format!("B{r}"), &format!("A{r}+1")));
        }
        sheet.push_str(&cell_f("C1", "SUM(B:B)"));
        sheet.push_str(&cell_f("D1", "SUM(B:B)"));

        let a = analyze(&xlsx(&[("S", &sheet)]));
        assert_eq!(a.n_formula_cells, 5);
        assert_eq!(
            a.n_components, 1,
            "the cached representative must connect every B formula and both consumers"
        );
    }

    #[test]
    fn cross_sheet_reference_is_detected_and_joined() {
        let a = analyze(&xlsx(&[
            ("One", &cell_f("A1", "Two!A1+1")),
            ("Two", &cell_f("A1", "5+1")),
        ]));
        assert_eq!(a.n_formula_cells, 2);
        assert!(a.cross_sheet, "One!A1 reads Two!A1");
        assert_eq!(a.n_components, 1, "both sheets form one component");
    }

    /// A whole-file load accepts a formula naming its own cell, but the
    /// incremental path a mini-workbook is built with rejects it, so these have
    /// to be recognised before partitioning rather than blowing up mid-run.
    #[test]
    fn self_referencing_formula_is_flagged() {
        let sheet = format!("{}{}", cell_f("A7", "ROW(A7)-6"), cell_f("B1", "C1+1"));
        let t = build(&xlsx(&[("S", &sheet)]));
        assert_eq!(t.self_refs, 1);
        assert!(!t.is_partitionable());

        // A range covering the formula's own cell counts too.
        let t = build(&xlsx(&[("S", &cell_f("A7", "SUM(A1:A7)"))]));
        assert_eq!(t.self_refs, 1);

        // Ordinary neighbours must not be mistaken for self-reference.
        let t = build(&xlsx(&[("S", &cell_f("A7", "SUM(B1:B6)"))]));
        assert_eq!(t.self_refs, 0);
        assert!(t.is_partitionable());
    }

    #[test]
    fn fixed_workbook_name_is_resolved_case_insensitively() {
        let main = format!(
            "{}{}",
            cell_v("A1", "2"),
            cell_f("B1", "VLOOKUP(A1,rates,2,FALSE)")
        );
        let lookup = format!(
            "{}{}{}{}",
            cell_v("A1", "1"),
            cell_v("B1", "10"),
            cell_v("A2", "2"),
            cell_v("B2", "20")
        );
        let data = xlsx_with_defined_names(
            &[("Main", &main), ("Lookup", &lookup)],
            r#"<definedName name="Rates">Lookup!$A$1:$B$2</definedName>"#,
        );
        let t = build(&data);

        assert_eq!(t.unsupported_refs, 0);
        assert_eq!(t.named_refs, 1);
        assert_eq!(t.static_names.len(), 1);
        assert_eq!(t.static_names[0].target, (1, 1, 1, 2, 2));
        assert_eq!(t.lookup_work(), 2);
        assert!(t.is_partitionable());
    }

    #[test]
    fn local_names_inherit_the_scope_sheet_for_bare_targets() {
        let data = xlsx_with_defined_names(
            &[("First", ""), ("Last", &cell_f("B2", "Local"))],
            r#"<definedName name="Local" localSheetId="1">$A$1</definedName>"#,
        );
        let t = build(&data);
        assert!(t.is_partitionable());
        assert_eq!(t.static_names[0].target, (1, 1, 1, 1, 1));
        assert!(t.static_names[0].scope == NameScope::Sheet(1));
    }

    #[test]
    fn omitted_sheet_parts_do_not_shift_local_name_scopes() {
        let data = xlsx_with_defined_names(
            &[("First", ""), ("Omitted", ""), ("Last", "")],
            r#"<definedName name="Local" localSheetId="2">$A$1</definedName>
               <definedName name="Missing" localSheetId="1">$A$1</definedName>"#,
        );
        let mut zip = ZipArchive::new(Cursor::new(&data)).unwrap();
        // Simulate sheet_parts omitting a sheet with an unresolved r:id.
        let parts = vec![("First".into(), "first.xml".into()), ("Last".into(), "last.xml".into())];
        let names = read_defined_names(&mut zip, &parts);
        assert!(names[0].scope == NameScope::Sheet(1));
        assert_eq!(names[0].target, Some((1, 1, 1, 1, 1)));
        assert!(names[1].scope == NameScope::Invalid);
        assert!(names[1].target.is_none());
        use formualizer::workbook::{CalamineAdapter, SpreadsheetReader};
        let mut adapter = CalamineAdapter::open_bytes(data).unwrap();
        let loaded = adapter.defined_names().unwrap();
        let local = loaded.iter().find(|n| n.name == "Local").unwrap();
        assert_eq!(local.scope_sheet.as_deref(), Some(parts[1].0.as_str()));
    }

    #[test]
    fn stray_namespaced_sheet_elements_do_not_shift_scopes() {
        use std::io::Write;
        let wb = br#"<?xml version="1.0"?><workbook xmlns:x="urn:ext"><ext><x:sheet name="Last"/></ext><sheets><sheet name="First" sheetId="1"/><sheet name="Last" sheetId="2"/></sheets><definedNames><definedName name="Local" localSheetId="1">$A$1</definedName></definedNames></workbook>"#;
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts =
            zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("xl/workbook.xml", opts).unwrap();
        w.write_all(wb).unwrap();
        let data = w.finish().unwrap().into_inner();
        let mut zip = ZipArchive::new(Cursor::new(data)).unwrap();
        let parts = vec![("First".into(), "first.xml".into()), ("Last".into(), "last.xml".into())];
        let names = read_defined_names(&mut zip, &parts);
        // localSheetId=1 must select the real "Last" sheet (parts[1]), not the
        // stray <x:sheet> injected before the sheet list.
        assert_eq!(names.len(), 1);
        assert!(names[0].scope == NameScope::Sheet(1));
        assert_eq!(names[0].target, Some((1, 1, 1, 1, 1)));
    }

    #[test]
    fn relative_and_undefined_names_stay_unsupported() {
        let data = xlsx_with_defined_names(
            &[
                ("Main", &cell_f("A1", "RelativeName+MissingName")),
                ("Lookup", ""),
            ],
            r#"<definedName name="RelativeName">Lookup!A1</definedName>"#,
        );
        let t = build(&data);
        assert_eq!(t.named_refs, 0);
        assert_eq!(t.unsupported_refs, 2);
    }

    /// The XML array annotation is diagnostic, not a partitioning refusal.
    #[test]
    fn array_formula_is_counted_without_gating() {
        let sheet = r#"<c r="B3"><f t="array" ref="B3:D3">SUMPRODUCT(A1:A9)</f><v>0</v></c>"#;
        let t = build(&xlsx(&[("S", sheet)]));
        assert_eq!(t.array_formulas, 1);
        assert!(t.is_partitionable());
    }

    /// Error literals have no dependency edge, including a broken sheet prefix.
    #[test]
    fn ref_errors_do_not_block_partitioning() {
        for formula in ["#REF!", "IFERROR(#REF!,7)", "VLOOKUP(A2,#REF!,2,0)", "'Missing Sheet'!#REF!"] {
            let t = build(&xlsx(&[("S", &cell_f("B2", formula))]));
            assert_eq!(t.parse_errors, 0, "{formula}");
            assert_eq!(t.unsupported_refs, 0, "{formula}");
            assert_eq!(t.self_refs, 0, "{formula}");
            assert!(t.is_partitionable(), "{formula}");
        }
    }

    /// Every `INDIRECT` call keeps the whole-file gates, including one whose
    /// address is a plain literal.
    ///
    /// A literal call such as `INDIRECT("A1")` could be rewritten to `$A$1`
    /// and partitioned. It is not, because it does not occur: across 13,458
    /// corpus workbooks, every `INDIRECT("` call builds its address by
    /// concatenation (`INDIRECT("A"&ROW())`), which stays dynamic.
    #[test]
    fn indirect_calls_keep_their_dynamic_and_row_gates() {
        for formula in [r#"INDIRECT("A1")"#, r#"INDIRECT("A1",TRUE)"#, r#"INDIRECT(A1)"#,
            r#"INDIRECT("A"&amp;ROW())"#, r#"SUM(INDIRECT("S!A1:A3"))"#] {
            let t = build(&xlsx(&[("S", &cell_f("B2", formula))]));
            assert!(t.dynamic_refs > 0, "{formula}");
            assert!(t.row_sensitive_fns > 0, "{formula}");
            assert!(!t.is_partitionable(), "{formula}");
        }
    }

    /// A constant name and an open-sided name both stay whole-file work.
    ///
    /// A whole-file run answers `#NAME?` for a constant name, because the
    /// loader keeps only range definitions. Resolving one here would give a
    /// different answer from the file being reproduced. Clamping an open side
    /// to the sheet extent would change `ROWS`, `COLUMNS` and `COUNTBLANK`.
    #[test]
    fn constant_and_open_sided_names_stay_unsupported() {
        let data = xlsx_with_defined_names(&[("S", &cell_f("A1", "_Order1+Tax"))],
            r#"<definedName name="_Order1">0</definedName><definedName name="Tax">-0.2</definedName>"#);
        let t = build(&data);
        assert_eq!(t.named_refs, 0);
        assert_eq!(t.unsupported_refs, 2);
        assert!(!t.is_partitionable());
        for formula in ["ROWS(OpenName)", "COLUMNS(OpenName)", "COUNTBLANK(OpenName)"] {
            let data = xlsx_with_defined_names(&[("S", &cell_f("B1", formula))],
                r#"<definedName name="OpenName">S!$A:$A</definedName>"#);
            assert_eq!(build(&data).unsupported_refs, 1);
        }
    }

    #[test]
    fn workbook_without_formulas_yields_no_components() {
        let a = analyze(&xlsx(&[("S", &cell_v("A1", "1"))]));
        assert_eq!(a.n_formula_cells, 0);
        assert_eq!(a.n_components, 0);
        assert_eq!(a.biggest_extent_cells, 0);
    }
}
