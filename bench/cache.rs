//! Read formula caches as evidence, not as authoritative Excel answers.
//! Missing/empty numeric caches must not become fabricated zeroes or blanks.

use crate::{graph, values};
use formualizer::common::value::LiteralValue;
use quick_xml::{events::Event, Reader};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

pub struct Cell {
    pub col: u32,
    pub formula: String,
    pub formula_type: String,
    pub shared_index: Option<String>,
    pub raw: Option<String>,
    pub value: Option<LiteralValue>,
    pub state: &'static str,
}

pub struct Book {
    pub sheets: BTreeMap<String, BTreeMap<u32, Vec<Cell>>>,
    pub formulas: usize,
    pub date1904: bool,
    pub calculation: Value,
}

pub fn read(data: &[u8]) -> Result<Book, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(data)).map_err(|e| e.to_string())?;
    let decoder = values::Values::open(&mut zip);
    let mut book = Book {
        sheets: BTreeMap::new(),
        formulas: 0,
        date1904: false,
        calculation: json!({}),
    };
    {
        let file = zip.by_name("xl/workbook.xml").map_err(|e| e.to_string())?;
        let mut xml = Reader::from_reader(BufReader::new(file));
        let mut buf = Vec::new();
        loop {
            match xml.read_event_into(&mut buf).map_err(|e| e.to_string())? {
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"workbookPr" => {
                    book.date1904 = values::raw_attr(&e, b"date1904")
                        .is_some_and(|v| v == b"1" || v == b"true");
                }
                Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"calcPr" => {
                    for key in ["calcMode", "calcId", "fullCalcOnLoad", "forceFullCalc"] {
                        if let Some(v) = values::raw_attr(&e, key.as_bytes()) {
                            book.calculation[key] = json!(String::from_utf8_lossy(&v));
                        }
                    }
                }
                Event::Eof => break,
                _ => (),
            }
            buf.clear();
        }
    }
    for (name, part) in graph::sheet_parts(&mut zip) {
        let file = zip.by_name(&part).map_err(|e| format!("{part}: {e}"))?;
        let mut xml = Reader::from_reader(BufReader::new(file));
        let mut buf = Vec::new();
        let mut pos = None;
        let mut ty = None;
        let mut formula = None;
        let mut formula_type = String::new();
        let mut shared_index = None;
        let mut raw = None;
        let (mut in_formula, mut in_value) = (false, false);
        loop {
            let event = xml
                .read_event_into(&mut buf)
                .map_err(|e| format!("{part}: {e}"))?;
            match &event {
                Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                    b"c" if matches!(event, Event::Start(_)) => {
                        pos = values::raw_attr(e, b"r")
                            .and_then(|v| String::from_utf8(v).ok())
                            .and_then(|s| graph::parse_a1(&s));
                        ty = values::raw_attr(e, b"t");
                        formula = None;
                        raw = None;
                        in_formula = false;
                        in_value = false;
                    }
                    // An empty `<c/>` holds no formula or value; it must not
                    // leave a dangling position that reads as truncated XML.
                    b"c" => pos = None,
                    b"f" => {
                        formula = Some(String::new());
                        formula_type = values::raw_attr(e, b"t")
                            .map(|v| String::from_utf8_lossy(&v).into_owned())
                            .unwrap_or_else(|| "normal".into());
                        shared_index = values::raw_attr(e, b"si")
                            .map(|v| String::from_utf8_lossy(&v).into_owned());
                        in_formula = matches!(event, Event::Start(_));
                    }
                    b"v" => {
                        raw = Some(String::new());
                        in_value = matches!(event, Event::Start(_));
                    }
                    _ => (),
                },
                Event::Text(t) => {
                    let text = t.unescape().map_err(|e| format!("{part}: {e}"))?;
                    if in_formula {
                        formula.as_mut().unwrap().push_str(&text);
                    }
                    if in_value {
                        raw.as_mut().unwrap().push_str(&text);
                    }
                }
                Event::CData(t) => {
                    let text = std::str::from_utf8(t.as_ref()).map_err(|e| e.to_string())?;
                    if in_formula {
                        formula.as_mut().unwrap().push_str(text);
                    }
                    if in_value {
                        raw.as_mut().unwrap().push_str(text);
                    }
                }
                Event::End(e) => match e.local_name().as_ref() {
                    b"f" => in_formula = false,
                    b"v" => in_value = false,
                    b"c" => {
                        if let Some(formula) = formula.take() {
                            let (row, col) = pos.ok_or_else(|| {
                                format!("{part}: formula with invalid cell address")
                            })?;
                            // Keep numeric caches as raw Excel serials. Date formatting
                            // is presentation; native dates are normalized at comparison.
                            let value = match (raw.as_deref(), ty.as_deref()) {
                                (None, _) => None,
                                (Some(v), None | Some(b"n")) => v
                                    .parse::<f64>()
                                    .ok()
                                    .filter(|v| v.is_finite())
                                    .map(LiteralValue::Number),
                                (Some(v), Some(b"str")) => Some(LiteralValue::Text(v.into())),
                                (Some("0"), Some(b"b")) => Some(LiteralValue::Boolean(false)),
                                (Some("1"), Some(b"b")) => Some(LiteralValue::Boolean(true)),
                                (Some(_), Some(b"b")) => None,
                                (Some(v), ty) => decoder.value(v, None, ty),
                            };
                            let state = if value.is_some() {
                                "present"
                            } else if raw.as_ref().is_none_or(String::is_empty) {
                                "missing"
                            } else {
                                "unsupported"
                            };
                            book.sheets
                                .entry(name.clone())
                                .or_default()
                                .entry(row)
                                .or_default()
                                .push(Cell {
                                    col,
                                    formula: formula.chars().take(256).collect(),
                                    formula_type: formula_type.clone(),
                                    shared_index: shared_index.clone(),
                                    raw: raw.take(),
                                    value,
                                    state,
                                });
                            book.formulas += 1;
                        }
                        pos = None;
                    }
                    _ => (),
                },
                Event::Eof => {
                    if pos.is_some() {
                        return Err(format!("{part}: incomplete cell XML"));
                    }
                    break;
                }
                _ => (),
            }
            buf.clear();
        }
    }
    Ok(book)
}
