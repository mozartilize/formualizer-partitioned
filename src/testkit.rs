//! Build a minimal xlsx in memory for tests.
//!
//! Only the parts this crate reads are written: `workbook.xml`, its
//! relationships, and one worksheet part per sheet. A test therefore states the
//! exact cell and formula XML under test. That matters for shared formulas,
//! which spreadsheet writers such as openpyxl cannot emit.

use std::io::{Cursor, Write};

/// Build a workbook from `(sheet name, inner `<sheetData>` XML)` pairs.
pub fn xlsx(sheets: &[(&str, &str)]) -> Vec<u8> {
    xlsx_with_defined_names(sheets, "")
}

/// Build a workbook and put the supplied entries inside `<definedNames>`.
pub fn xlsx_with_defined_names(sheets: &[(&str, &str)], defined_names: &str) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

    let mut wb = String::from(r#"<?xml version="1.0"?><workbook><sheets>"#);
    let mut rels = String::from(r#"<?xml version="1.0"?><Relationships>"#);
    let mut types = String::from(
        r#"<?xml version="1.0"?><Types><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>"#,
    );
    for (i, (name, _)) in sheets.iter().enumerate() {
        let n = i + 1;
        wb.push_str(&format!(
            r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#
        ));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{n}.xml"/>"#
        ));
        types.push_str(&format!(
            r#"<Override PartName="/xl/worksheets/sheet{n}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
        ));
    }
    types.push_str("</Types>");
    wb.push_str("</sheets>");
    if !defined_names.is_empty() {
        wb.push_str("<definedNames>");
        wb.push_str(defined_names);
        wb.push_str("</definedNames>");
    }
    wb.push_str("</workbook>");
    rels.push_str("</Relationships>");

    // The engine's loader (calamine) requires the package relationships and
    // content types that this crate's own streaming reader never looks at.
    w.start_file("_rels/.rels", opts).unwrap();
    w.write_all(
        br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
    )
    .unwrap();
    w.start_file("[Content_Types].xml", opts).unwrap();
    w.write_all(types.as_bytes()).unwrap();
    w.start_file("xl/workbook.xml", opts).unwrap();
    w.write_all(wb.as_bytes()).unwrap();
    w.start_file("xl/_rels/workbook.xml.rels", opts).unwrap();
    w.write_all(rels.as_bytes()).unwrap();
    for (i, (_, data)) in sheets.iter().enumerate() {
        w.start_file(format!("xl/worksheets/sheet{}.xml", i + 1), opts)
            .unwrap();
        w.write_all(
            format!(r#"<?xml version="1.0"?><worksheet><sheetData>{data}</sheetData></worksheet>"#)
                .as_bytes(),
        )
        .unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// A cell that holds a formula, with a cached result beside it as Excel writes.
pub fn cell_f(addr: &str, formula: &str) -> String {
    format!(r#"<c r="{addr}"><f>{formula}</f><v>0</v></c>"#)
}

/// A cell that holds a number.
pub fn cell_v(addr: &str, value: &str) -> String {
    format!(r#"<c r="{addr}"><v>{value}</v></c>"#)
}

/// A cell that holds a shared string index.
pub fn cell_s(addr: &str, index: usize) -> String {
    format!(r#"<c r="{addr}" t="str"><v>{index}</v></c>"#)
}
