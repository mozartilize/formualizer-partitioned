//! Build a minimal xlsx in memory for tests.
//!
//! Only the parts this crate reads are written: `workbook.xml`, its
//! relationships, `styles.xml` (date format 14 at style index 1), and one
//! worksheet part per sheet. A test therefore states the exact cell and
//! formula XML under test. That matters for shared formulas, which spreadsheet
//! writers such as openpyxl cannot emit.

use std::io::{Cursor, Write};

/// Built-in date format 14 is style index 1 (`cell_date`). Index 0 is general.
const STYLES_XML: &[u8] = br#"<?xml version="1.0"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font/></fonts><fills count="1"><fill/></fills><borders count="1"><border/></borders><cellStyleXfs count="1"><xf numFmtId="0"/></cellStyleXfs><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="14" applyNumberFormat="1"/></cellXfs></styleSheet>"#;

fn attach_styles(types: &mut String, rels: &mut String, rid: usize) {
    types.push_str(
        r#"<Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>"#,
    );
    rels.push_str(&format!(
        r#"<Relationship Id="rId{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#,
    ));
}

/// Build a workbook from `(sheet name, inner `<sheetData>` XML)` pairs.
pub fn xlsx(sheets: &[(&str, &str)]) -> Vec<u8> {
    xlsx_with_defined_names(sheets, "")
}

/// Build a workbook whose `cell_s` cells read from a shared string table.
pub fn xlsx_with_shared_strings(sheets: &[(&str, &str)], strings: &[&str]) -> Vec<u8> {
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
    let shared = sheets.len() + 1;
    rels.push_str(&format!(
        r#"<Relationship Id="rId{shared}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>"#
    ));
    types.push_str(
        r#"<Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>"#,
    );
    attach_styles(&mut types, &mut rels, sheets.len() + 2);
    types.push_str("</Types>");
    wb.push_str("</sheets></workbook>");
    rels.push_str("</Relationships>");

    let mut sst = format!(
        r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{}" uniqueCount="{}">"#,
        strings.len(),
        strings.len()
    );
    for s in strings {
        sst.push_str("<si><t>");
        sst.push_str(&s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"));
        sst.push_str("</t></si>");
    }
    sst.push_str("</sst>");

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
    w.start_file("xl/sharedStrings.xml", opts).unwrap();
    w.write_all(sst.as_bytes()).unwrap();
    w.start_file("xl/styles.xml", opts).unwrap();
    w.write_all(STYLES_XML).unwrap();
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

/// Build a workbook and put the supplied entries inside `<definedNames>`.
pub fn xlsx_with_defined_names(sheets: &[(&str, &str)], defined_names: &str) -> Vec<u8> {
    xlsx_with_settings(sheets, defined_names, "")
}

/// Build a workbook with a supplied `<calcPr>` element.
pub fn xlsx_with_calc_pr(sheets: &[(&str, &str)], calc_pr: &str) -> Vec<u8> {
    xlsx_with_settings(sheets, "", calc_pr)
}

fn xlsx_with_settings(sheets: &[(&str, &str)], defined_names: &str, calc_pr: &str) -> Vec<u8> {
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
    attach_styles(&mut types, &mut rels, sheets.len() + 1);
    types.push_str("</Types>");
    wb.push_str("</sheets>");
    if !defined_names.is_empty() {
        wb.push_str("<definedNames>");
        wb.push_str(defined_names);
        wb.push_str("</definedNames>");
    }
    wb.push_str(calc_pr);
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
    w.start_file("xl/styles.xml", opts).unwrap();
    w.write_all(STYLES_XML).unwrap();
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

/// A numeric cell using built-in date format 14 (style index 1).
pub fn cell_date(addr: &str, serial: &str) -> String {
    format!(r#"<c r="{addr}" s="1"><v>{serial}</v></c>"#)
}

/// A cell that holds a shared string index.
pub fn cell_s(addr: &str, index: usize) -> String {
    format!(r#"<c r="{addr}" t="s"><v>{index}</v></c>"#)
}
