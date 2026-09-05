//! Read cell values straight from the workbook XML.
//!
//! The evaluation backend can supply values, but it decodes a whole sheet into
//! its own map on the first read and keeps it, which is the largest single
//! allocation that path makes. Nothing here is kept but the shared-string table
//! and one flag per cell format, so a sheet streams through at whatever size
//! the caller stores it at.
//!
//! Values must match what the backend would have produced, because a partitioned
//! run is checked cell by cell against a whole-file one. The rules below are
//! therefore ported from the backend's reader rather than written afresh:
//!
//! - a numeric cell is a date when its cell format says so, which is what makes
//!   `43831` come back as a date rather than a number;
//! - both date and elapsed-time formats become the same serial number, since the
//!   backend passes both through the same conversion;
//! - an empty string is not a value, and neither is an empty cell;
//! - text in `<t>` is trimmed of ASCII whitespace unless `xml:space="preserve"`,
//!   and phonetic runs in `<rPh>` are not part of the string;
//! - the 1904 date system is not honoured, because the backend's reader does not
//!   expose it and always decodes against 1900.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Cursor, Read, Seek};
use std::rc::Rc;

use flate2::read::DeflateDecoder;
use formualizer::common::error::{ExcelError, ExcelErrorKind};
use formualizer::common::value::{DateSystem, LiteralValue};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use zip::{CompressionMethod, ZipArchive};

/// Does this number format render a date, a time, or an elapsed time?
///
/// Ported from the backend's reader so that the two agree on which numbers are
/// dates. A format is a date when an unquoted, unescaped, unbracketed `d`, `m`,
/// `h`, `y` or `s` appears in its first section, or when `[h]`-style elapsed
/// time is used. Only the first section counts: `;` ends the scan.
fn is_date_format(format: &str) -> bool {
    let mut escaped = false;
    let mut is_quote = false;
    let mut brackets = 0u8;
    let mut prev = ' ';
    let mut hms = false;
    let mut ap = false;
    for s in format.chars() {
        match (s, escaped, is_quote, ap, brackets) {
            (_, true, ..) => escaped = false,
            // `\` escapes, `_` skips a width and `*` fills; in each case the
            // next character is a literal rather than a format token.
            ('_' | '\\' | '*', ..) => escaped = true,
            ('"', _, true, _, _) => is_quote = false,
            (_, _, true, _, _) => (),
            ('"', _, _, _, _) => is_quote = true,
            (';', ..) => return false,
            ('[', ..) => brackets += 1,
            (']', .., 1) if hms => return true,
            (']', ..) => brackets = brackets.saturating_sub(1),
            ('a' | 'A', _, _, false, 0) => ap = true,
            ('p' | 'm' | '/' | 'P' | 'M', _, _, true, 0) => return true,
            ('d' | 'm' | 'h' | 'y' | 's' | 'D' | 'M' | 'H' | 'Y' | 'S', _, _, false, 0) => {
                return true
            }
            _ => {
                if !(hms && s.eq_ignore_ascii_case(&prev)) {
                    hms = prev == '[' && matches!(s, 'm' | 'h' | 's' | 'M' | 'H' | 'S');
                }
            }
        }
        prev = s;
    }
    false
}

/// Date formats built into the format, matched on the raw attribute bytes so
/// that a padded or non-canonical id misses here exactly as it does upstream.
fn builtin_is_date(id: &[u8]) -> bool {
    matches!(
        id,
        b"14" | b"15" | b"16" | b"17" | b"18" | b"19" | b"20" | b"21" | b"22" | b"45" | b"46"
            | b"47"
    )
}

fn error_kind(v: &str) -> ExcelErrorKind {
    match v {
        "#DIV/0!" => ExcelErrorKind::Div,
        "#N/A" => ExcelErrorKind::Na,
        "#NAME?" => ExcelErrorKind::Name,
        "#NULL!" => ExcelErrorKind::Null,
        "#NUM!" => ExcelErrorKind::Num,
        "#REF!" => ExcelErrorKind::Ref,
        _ => ExcelErrorKind::Value,
    }
}

/// Decode element text the way an XML parser must.
///
/// Line endings are normalised before entities are expanded, per XML 1.0
/// section 2.11: a literal CRLF or CR in the file becomes a single LF, while an
/// escaped `&#13;` stays a carriage return because it is not a line ending
/// until it is decoded. Doing this in the other order turns text a whole-file
/// load reports as `"a\nb"` into `"a\r\nb"`, which real files do contain.
pub fn decode_text(raw: &[u8]) -> String {
    let raw = String::from_utf8_lossy(raw);
    let normalised = if raw.contains('\r') {
        std::borrow::Cow::Owned(raw.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        raw
    };
    match quick_xml::escape::unescape(&normalised) {
        Ok(s) => s.into_owned(),
        Err(_) => normalised.into_owned(),
    }
}

pub fn raw_attr(e: &BytesStart<'_>, key: &[u8]) -> Option<Vec<u8>> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| a.value.into_owned())
}

/// Concatenate the text of a `<si>` or `<is>` element, which may be a single
/// `<t>` or a sequence of formatted runs. Phonetic guides are skipped: they are
/// pronunciation hints, not part of the cell's string.
pub fn read_text_element<R: std::io::BufRead>(rdr: &mut Reader<R>, closing: &[u8], buf: &mut Vec<u8>) -> String {
    let mut out = String::new();
    let mut in_phonetic = false;
    let mut depth = 0i32;
    loop {
        buf.clear();
        match rdr.read_event_into(buf) {
            Ok(Event::Start(e)) => {
                let name = e.name().as_ref().to_vec();
                if name == b"rPh" {
                    in_phonetic = true;
                } else if name == b"t" && !in_phonetic {
                    let preserve = raw_attr(&e, b"xml:space").as_deref() == Some(b"preserve");
                    let mut text = String::new();
                    let mut inner = Vec::new();
                    loop {
                        inner.clear();
                        match rdr.read_event_into(&mut inner) {
                            Ok(Event::Text(t)) => {
                                text.push_str(&decode_text(t.as_ref()));
                            }
                            Ok(Event::CData(t)) => {
                                text.push_str(&String::from_utf8_lossy(&t));
                            }
                            Ok(Event::End(end)) if end.name().as_ref() == b"t" => break,
                            Ok(Event::Eof) | Err(_) => return out,
                            _ => (),
                        }
                    }
                    if preserve {
                        out.push_str(&text);
                    } else {
                        out.push_str(text.trim_matches([' ', '\t', '\r', '\n']));
                    }
                } else if name == closing {
                    depth += 1;
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                if name.as_ref() == b"rPh" {
                    in_phonetic = false;
                } else if name.as_ref() == closing {
                    if depth == 0 {
                        return out;
                    }
                    depth -= 1;
                }
            }
            Ok(Event::Eof) | Err(_) => return out,
            _ => (),
        }
    }
}

/// Workbook-level tables a sheet's cells refer to.
pub struct Values {
    strings: Vec<Box<str>>,
    /// One entry per `cellXfs` record: does that format render a date?
    date_formats: Vec<bool>,
}

/// A data value in compact form.
///
/// `LiteralValue` is 104 bytes because of its widest variants, which data cells
/// never use. At one entry per cell that dominates the store. The common scalar
/// cases are held inline and anything else is boxed, so no value is
/// reinterpreted and dates round-trip exactly as the backend produces them.
pub enum Val {
    Int(i64),
    Number(f64),
    Boolean(bool),
    Text(Box<str>),
    Other(Box<LiteralValue>),
}

impl Val {
    pub fn pack(v: LiteralValue) -> Val {
        match v {
            LiteralValue::Int(i) => Val::Int(i),
            LiteralValue::Number(n) => Val::Number(n),
            LiteralValue::Boolean(b) => Val::Boolean(b),
            LiteralValue::Text(s) => Val::Text(s.into_boxed_str()),
            other => Val::Other(Box::new(other)),
        }
    }

    pub fn unpack(&self) -> LiteralValue {
        match self {
            Val::Int(i) => LiteralValue::Int(*i),
            Val::Number(n) => LiteralValue::Number(*n),
            Val::Boolean(b) => LiteralValue::Boolean(*b),
            Val::Text(s) => LiteralValue::Text(s.to_string()),
            Val::Other(v) => (**v).clone(),
        }
    }
}

impl Values {
    pub fn open<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Values {
        Values {
            strings: read_shared_strings(zip),
            date_formats: read_date_formats(zip),
        }
    }

    fn is_date(&self, style: Option<&[u8]>) -> bool {
        match style {
            // A cell without a style carries the default format, which is not a
            // date. An index past the table is treated the same way.
            None => false,
            Some(s) => std::str::from_utf8(s)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .and_then(|i| self.date_formats.get(i).copied())
                .unwrap_or(false),
        }
    }

    /// Decode a `<v>` payload given the cell's style and type attributes.
    pub fn value(&self, v: &str, style: Option<&[u8]>, ty: Option<&[u8]>) -> Option<LiteralValue> {
        match ty {
            Some(b"s") => {
                let idx = v.parse::<usize>().ok()?;
                match self.strings.get(idx) {
                    Some(s) if s.is_empty() => None,
                    Some(s) => Some(LiteralValue::Text(s.to_string())),
                    None => None,
                }
            }
            Some(b"b") => Some(LiteralValue::Boolean(v != "0")),
            // An ISO date in the file stays text: the backend does not convert it.
            Some(b"d") => Some(LiteralValue::Text(v.to_string())),
            Some(b"e") => Some(LiteralValue::Error(ExcelError::new(error_kind(v)))),
            Some(b"str") => {
                if v.is_empty() {
                    None
                } else {
                    Some(LiteralValue::Text(v.to_string()))
                }
            }
            Some(b"n") | None => {
                if v.is_empty() {
                    return None;
                }
                match v.parse::<f64>() {
                    Ok(n) if self.is_date(style) => Some(
                        LiteralValue::try_from_serial_number_for(DateSystem::Excel1900, n)
                            .unwrap_or_else(LiteralValue::Error),
                    ),
                    Ok(n) => Some(LiteralValue::Number(n)),
                    // An untyped cell holding something unparseable is text;
                    // one explicitly numeric is malformed and has no value.
                    Err(_) if ty.is_none() => Some(LiteralValue::Text(v.to_string())),
                    Err(_) => None,
                }
            }
            _ => None,
        }
    }

    /// Stream one worksheet part, handing every non-empty value to `sink`.
    pub fn read_sheet<R: Read + Seek, F: FnMut(u32, u32, LiteralValue)>(
        &self,
        zip: &mut ZipArchive<R>,
        part: &str,
        mut sink: F,
    ) {
        let file = match zip.by_name(part) {
            Ok(f) => f,
            Err(_) => return,
        };
        let mut cells = Cells::new(BufReader::new(file));
        while let Some((r, c, v)) = cells.next_cell(self) {
            sink(r, c, v);
        }
    }
}

/// Decodes the `<c>` elements of one worksheet part, one value at a time.
///
/// Two readers need the same decoding rules. `Values::read_sheet` reads a whole
/// part into a store. `SheetStream` reads the same part one row at a time. The
/// event loop therefore lives here, and both readers drive it.
struct Cells<R: BufRead> {
    rdr: Reader<R>,
    buf: Vec<u8>,
    text_buf: Vec<u8>,
    pos: Option<(u32, u32)>,
    style: Option<Vec<u8>>,
    ty: Option<Vec<u8>>,
    in_v: bool,
    v_text: String,
}

impl<R: BufRead> Cells<R> {
    fn new(inner: R) -> Cells<R> {
        Cells {
            rdr: Reader::from_reader(inner),
            buf: Vec::new(),
            text_buf: Vec::new(),
            pos: None,
            style: None,
            ty: None,
            in_v: false,
            v_text: String::new(),
        }
    }

    /// The next cell that carries a value, in the order the part holds them.
    fn next_cell(&mut self, values: &Values) -> Option<(u32, u32, LiteralValue)> {
        // The fields are taken apart here because the reader and the buffer are
        // borrowed at the same time.
        let Cells { rdr, buf, text_buf, pos, style, ty, in_v, v_text } = self;
        loop {
            buf.clear();
            match rdr.read_event_into(buf) {
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                    b"c" => {
                        *pos = raw_attr(&e, b"r")
                            .and_then(|r| String::from_utf8(r).ok())
                            .and_then(|r| crate::graph::parse_a1(&r));
                        *style = raw_attr(&e, b"s");
                        *ty = raw_attr(&e, b"t");
                    }
                    b"v" => {
                        *in_v = true;
                        v_text.clear();
                    }
                    // Inline strings hold their text here, and any <v> beside
                    // them is redundant.
                    b"is" => {
                        let text = read_text_element(rdr, b"is", text_buf);
                        let cell = pos.take();
                        if let Some((r, c)) = cell {
                            if !text.is_empty() {
                                return Some((r, c, LiteralValue::Text(text)));
                            }
                        }
                    }
                    _ => (),
                },
                Ok(Event::Text(t)) if *in_v => {
                    v_text.push_str(&decode_text(t.as_ref()));
                }
                Ok(Event::End(e)) => match e.name().as_ref() {
                    b"v" => {
                        *in_v = false;
                        if let Some((r, c)) = *pos {
                            if let Some(val) =
                                values.value(v_text, style.as_deref(), ty.as_deref())
                            {
                                return Some((r, c, val));
                            }
                        }
                    }
                    b"c" => *pos = None,
                    _ => (),
                },
                Ok(Event::Eof) | Err(_) => return None,
                _ => (),
            }
        }
    }
}

/// One worksheet part, read row by row, with its own reader.
///
/// A consumer that works in row order can read one chunk of rows, use it, and
/// drop it before it reads the next chunk. That consumer must hold the reader
/// between chunks, and a zip archive cannot be borrowed for that long. The part
/// is therefore copied out of the archive compressed and is inflated here. The
/// copy is the compressed size of one sheet, not the size of the sheet.
pub struct SheetStream {
    values: Rc<Values>,
    cells: Cells<BufReader<Box<dyn Read>>>,
    /// The first cell of the next row. It is read before the current row ends.
    pending: Option<(u32, u32, LiteralValue)>,
}

impl SheetStream {
    /// Open one worksheet part for row-by-row reading.
    ///
    /// Returns `None` when the part is absent, or when it uses a compression
    /// method this reader cannot inflate. The caller then uses the store path.
    pub fn open<R: Read + Seek>(
        zip: &mut ZipArchive<R>,
        part: &str,
        values: Rc<Values>,
    ) -> Option<SheetStream> {
        let index = zip.index_for_name(part)?;
        let mut raw = zip.by_index_raw(index).ok()?;
        let method = raw.compression();
        let mut bytes: Vec<u8> = Vec::with_capacity(raw.compressed_size() as usize);
        raw.read_to_end(&mut bytes).ok()?;
        let inner: Box<dyn Read> = match method {
            CompressionMethod::Stored => Box::new(Cursor::new(bytes)),
            CompressionMethod::Deflated => Box::new(DeflateDecoder::new(Cursor::new(bytes))),
            _ => return None,
        };
        Some(SheetStream {
            values,
            cells: Cells::new(BufReader::new(inner)),
            pending: None,
        })
    }

    /// The next row that carries at least one value.
    ///
    /// The cells come back in the order the part holds them. Rows that hold no
    /// value at all are absent from the part and are therefore never returned.
    pub fn next_row(&mut self) -> Option<(u32, Vec<(u32, LiteralValue)>)> {
        let (row, col, value) = match self.pending.take() {
            Some(cell) => cell,
            None => self.cells.next_cell(&self.values)?,
        };
        let mut out = vec![(col, value)];
        loop {
            match self.cells.next_cell(&self.values) {
                Some((r, c, v)) if r == row => out.push((c, v)),
                Some(cell) => {
                    self.pending = Some(cell);
                    break;
                }
                None => break,
            }
        }
        Some((row, out))
    }
}

fn read_shared_strings<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Vec<Box<str>> {
    let mut out = Vec::new();
    let file = match zip.by_name("xl/sharedStrings.xml") {
        Ok(f) => f,
        Err(_) => return out,
    };
    let mut rdr = Reader::from_reader(BufReader::new(file));
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match rdr.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"si" => {
                let s = read_text_element(&mut rdr, b"si", &mut Vec::new());
                out.push(s.into_boxed_str());
            }
            // <si/> is a valid empty string.
            Ok(Event::Empty(e)) if e.name().as_ref() == b"si" => out.push(String::new().into()),
            Ok(Event::Eof) | Err(_) => break,
            _ => (),
        }
    }
    out
}

/// Build the per-format date flags from `xl/styles.xml`.
///
/// Only `<xf>` records inside `<cellXfs>` count; the ones in `<cellStyleXfs>`
/// describe named styles and are not what a cell's `s` attribute indexes.
fn read_date_formats<R: Read + Seek>(zip: &mut ZipArchive<R>) -> Vec<bool> {
    let mut custom: HashMap<Vec<u8>, bool> = HashMap::new();
    let mut out = Vec::new();
    let file = match zip.by_name("xl/styles.xml") {
        Ok(f) => f,
        Err(_) => return out,
    };
    let mut rdr = Reader::from_reader(BufReader::new(file));
    let mut buf = Vec::new();
    let mut in_cell_xfs = false;
    loop {
        buf.clear();
        match rdr.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"numFmt" => {
                    if let (Some(id), Some(code)) =
                        (raw_attr(&e, b"numFmtId"), raw_attr(&e, b"formatCode"))
                    {
                        let code = quick_xml::escape::unescape(&String::from_utf8_lossy(&code))
                            .map(|c| c.into_owned())
                            .unwrap_or_default();
                        custom.insert(id, is_date_format(&code));
                    }
                }
                b"cellXfs" => in_cell_xfs = true,
                b"xf" if in_cell_xfs => {
                    let is_date = match raw_attr(&e, b"numFmtId") {
                        Some(id) => match custom.get(&id) {
                            Some(&flag) => flag,
                            None => builtin_is_date(&id),
                        },
                        None => false,
                    };
                    out.push(is_date);
                }
                _ => (),
            },
            Ok(Event::End(e)) if e.name().as_ref() == b"cellXfs" => in_cell_xfs = false,
            Ok(Event::Eof) | Err(_) => break,
            _ => (),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_formats_are_told_apart_from_ordinary_numbers() {
        for f in ["DD/MM/YY", "H:MM:SS;@", "m\"M\"d\"D\";@", "[h]:mm:ss", "yyyy-mm-dd"] {
            assert!(is_date_format(f), "{f} should read as a date");
        }
        for f in [
            "#,##0",
            "0.00",
            "General",
            // A quoted `m` is a literal, not a month.
            "\"Y: \"0.00\"m\";\"Y: \"-0.00\"m\"",
            // An escaped `d` is a literal too.
            "0\\d",
            // A currency code in brackets is not elapsed time.
            "#,##0\\ [$\\u20bd-46D]",
        ] {
            assert!(!is_date_format(f), "{f} should not read as a date");
        }
    }

    #[test]
    fn line_endings_are_normalised_before_entities_are_decoded() {
        assert_eq!(decode_text(b"a\r\nb"), "a\nb");
        assert_eq!(decode_text(b"a\rb"), "a\nb");
        assert_eq!(decode_text(b"a&amp;b"), "a&b");
        // An escaped carriage return is not a line ending in the source text,
        // so it survives decoding as itself.
        assert_eq!(decode_text(b"a&#13;b"), "a\rb");
    }

    #[test]
    fn builtin_date_ids_match_on_raw_bytes() {
        assert!(builtin_is_date(b"14"));
        assert!(builtin_is_date(b"22"));
        assert!(builtin_is_date(b"46"));
        assert!(!builtin_is_date(b"0"));
        assert!(!builtin_is_date(b"23"));
        // Matching is on the raw attribute, so a padded id is not a date.
        assert!(!builtin_is_date(b"014"));
    }
}
