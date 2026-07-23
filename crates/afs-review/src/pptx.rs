//! PowerPoint (`.pptx`) → the unified model.
//!
//! Slides are ordered by `<p:sldIdLst>` in `presentation.xml` (the `slideN.xml`
//! filename is *not* the display order), resolved to parts through the
//! presentation `.rels`. Each slide is a container keyed by its **part name** —
//! stable across reordering, so moving a slide reads as a move, not add+delete.
//! One [`Unit`] per text-bearing shape, keyed by the shape's non-visual id.

use crate::model::{Container, DocModel, Format, ReviewError, Unit};
use crate::opc::{read_rels, resolve_target, Opc};
use crate::xmlutil::{attr, decode, unique_keys, xml_err};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

pub fn project(opc: &Opc) -> Result<DocModel, ReviewError> {
    let pres = opc
        .get_str("ppt/presentation.xml")
        .ok_or_else(|| ReviewError::MissingPart("ppt/presentation.xml".into()))?;

    // Slide display order = the r:id sequence under <p:sldIdLst>.
    let mut rids: Vec<String> = Vec::new();
    let mut reader = Reader::from_str(pres.as_ref());
    loop {
        match reader.read_event().map_err(xml_err)? {
            Event::Empty(e) | Event::Start(e) if e.local_name().as_ref() == b"sldId" => {
                if let Some(rid) = attr(&e, b"r:id") {
                    rids.push(rid);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    let rels = read_rels(opc, "ppt/_rels/presentation.xml.rels");
    let mut containers = Vec::new();
    for (idx, rid) in rids.iter().enumerate() {
        let Some(target) = rels.get(rid) else {
            continue;
        };
        let part = resolve_target("ppt", target);
        let Some(bytes) = opc.get(&part) else {
            continue;
        };
        let part_sha = blake3::hash(bytes).to_hex().to_string();
        let xml = String::from_utf8_lossy(bytes);
        let units = shapes(xml.as_ref())?;
        let keyed = unique_keys(&units);
        containers.push(Container {
            key: part,
            label: format!("Slide {}", idx + 1),
            order: idx,
            part_sha,
            keyed,
            units,
        });
    }

    Ok(DocModel {
        format: Format::Pptx,
        containers,
    })
}

/// One unit per text-bearing shape on a slide.
fn shapes(xml: &str) -> Result<Vec<Unit>, ReviewError> {
    let mut reader = Reader::from_str(xml);
    let mut units = Vec::new();
    let mut order = 0usize;

    let mut in_sp = false;
    let mut in_atext = false;
    let mut id: Option<String> = None;
    let mut name: Option<String> = None;
    let mut ph: Option<String> = None;
    let mut text = String::new();

    loop {
        match reader.read_event().map_err(xml_err)? {
            Event::Start(e) => match e.local_name().as_ref() {
                b"sp" => {
                    in_sp = true;
                    id = None;
                    name = None;
                    ph = None;
                    text.clear();
                }
                b"cNvPr" if in_sp => {
                    id = attr(&e, b"id");
                    name = attr(&e, b"name");
                }
                b"ph" if in_sp => ph = attr(&e, b"type"),
                // A new <a:p> paragraph within the shape: keep line structure.
                b"p" if in_sp && !text.is_empty() => text.push('\n'),
                b"t" if in_sp => in_atext = true,
                _ => {}
            },
            Event::Empty(e) if in_sp => match e.local_name().as_ref() {
                b"cNvPr" => {
                    id = attr(&e, b"id");
                    name = attr(&e, b"name");
                }
                b"ph" => ph = attr(&e, b"type"),
                _ => {}
            },
            Event::Text(t) if in_atext => text.push_str(&decode(t)?),
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_atext = false,
                b"sp" => {
                    in_sp = false;
                    let body = text.trim().to_string();
                    if !body.is_empty() {
                        let label = ph
                            .clone()
                            .map(|p| format!("{p} placeholder"))
                            .or_else(|| name.clone())
                            .unwrap_or_else(|| format!("Shape {}", order + 1));
                        units.push(Unit {
                            key: id.take(),
                            label,
                            order,
                            text: body,
                            formula: None,
                        });
                        order += 1;
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
