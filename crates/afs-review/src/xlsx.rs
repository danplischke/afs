//! Excel (`.xlsx`) → the unified model.
//!
//! Worksheets are containers keyed by sheet name (ordered per `workbook.xml`);
//! each non-empty cell is a [`Unit`] keyed by its **cell reference** (`A1`) — a
//! naturally stable 2-D address. Shared-string indices are resolved to literal
//! text (they renumber on every save), and a cell's formula is captured
//! alongside its cached value so the diff can tell an edit from a recalculation.

use crate::model::{Container, DocModel, Format, ReviewError, Unit};
use crate::opc::{read_rels, resolve_target, Opc};
use crate::xmlutil::{attr, decode, unique_keys, xml_err};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

pub fn project(opc: &Opc) -> Result<DocModel, ReviewError> {
    let shared = shared_strings(opc)?;

    let wb = opc
        .get_str("xl/workbook.xml")
        .ok_or_else(|| ReviewError::MissingPart("xl/workbook.xml".into()))?;
    let mut sheets: Vec<(String, String)> = Vec::new();
    let mut reader = Reader::from_str(wb.as_ref());
    loop {
        match reader.read_event().map_err(xml_err)? {
            Event::Empty(e) | Event::Start(e) if e.local_name().as_ref() == b"sheet" => {
                let name = attr(&e, b"name").unwrap_or_default();
                if let Some(rid) = attr(&e, b"r:id") {
                    sheets.push((name, rid));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    let rels = read_rels(opc, "xl/_rels/workbook.xml.rels");
    let mut containers = Vec::new();
    for (idx, (name, rid)) in sheets.iter().enumerate() {
        let Some(target) = rels.get(rid) else {
            continue;
        };
        let part = resolve_target("xl", target);
        let Some(bytes) = opc.get(&part) else {
            continue;
        };
        let part_sha = blake3::hash(bytes).to_hex().to_string();
        let xml = String::from_utf8_lossy(bytes);
        let units = cells(xml.as_ref(), &shared)?;
        containers.push(Container {
            key: name.clone(),
            label: format!("Sheet: {name}"),
            order: idx,
            part_sha,
            keyed: unique_keys(&units),
            units,
        });
    }

    Ok(DocModel {
        format: Format::Xlsx,
        containers,
    })
}

/// The shared-string table: index → literal text (rich-text runs concatenated).
fn shared_strings(opc: &Opc) -> Result<Vec<String>, ReviewError> {
    let mut out = Vec::new();
    let Some(xml) = opc.get_str("xl/sharedStrings.xml") else {
        return Ok(out);
    };
    let mut reader = Reader::from_str(xml.as_ref());
    let mut in_si = false;
    let mut in_t = false;
    let mut cur = String::new();
    loop {
        match reader.read_event().map_err(xml_err)? {
            Event::Start(e) => match e.local_name().as_ref() {
                b"si" => {
                    in_si = true;
                    cur.clear();
                }
                b"t" if in_si => in_t = true,
                _ => {}
            },
            Event::Text(t) if in_t => cur.push_str(&decode(t)?),
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_t = false,
                b"si" => {
                    in_si = false;
                    out.push(std::mem::take(&mut cur));
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// One unit per non-empty cell on a sheet.
fn cells(xml: &str, shared: &[String]) -> Result<Vec<Unit>, ReviewError> {
    let mut reader = Reader::from_str(xml);
    let mut units = Vec::new();
    let mut order = 0usize;

    let mut cell_ref: Option<String> = None;
    let mut cell_type = String::new();
    let mut in_v = false;
    let mut in_f = false;
    let mut in_is_t = false;
    let mut val = String::new();
    let mut formula = String::new();

    loop {
        match reader.read_event().map_err(xml_err)? {
            Event::Start(e) => match e.local_name().as_ref() {
                b"c" => {
                    cell_ref = attr(&e, b"r");
                    cell_type = attr(&e, b"t").unwrap_or_default();
                    val.clear();
                    formula.clear();
                }
                b"v" => in_v = true,
                b"f" => in_f = true,
                b"t" => in_is_t = true, // inline string <is><t>…</t></is>
                _ => {}
            },
            Event::Text(t) => {
                let s = decode(t)?;
                if in_v {
                    val.push_str(&s);
                } else if in_f {
                    formula.push_str(&s);
                } else if in_is_t {
                    val.push_str(&s);
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"v" => in_v = false,
                b"f" => in_f = false,
                b"t" => in_is_t = false,
                b"c" => {
                    if let Some(r) = cell_ref.take() {
                        let text = resolve(&cell_type, &val, shared);
                        let formula = (!formula.is_empty()).then(|| formula.clone());
                        if !text.is_empty() || formula.is_some() {
                            units.push(Unit {
                                key: Some(r.clone()),
                                label: r,
                                order,
                                text,
                                formula,
                            });
                            order += 1;
                        }
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(units)
}

/// Resolve a cell's stored value to display text by its type.
fn resolve(cell_type: &str, val: &str, shared: &[String]) -> String {
    match cell_type {
        "s" => val
            .parse::<usize>()
            .ok()
            .and_then(|i| shared.get(i))
            .cloned()
            .unwrap_or_default(),
        "b" => match val {
            "1" => "TRUE".into(),
            "0" => "FALSE".into(),
            other => other.to_string(),
        },
        // number, formula-string ("str"), inline string — value is literal.
        _ => val.to_string(),
    }
}
