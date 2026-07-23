//! Word (`.docx`) → the unified model.
//!
//! One `body` container; one [`Unit`] per non-empty paragraph. Text comes only
//! from `<w:t>` runs (so runs coalesce and revision/proofing attributes are
//! ignored — the normalization that keeps a resave from looking like an edit).
//! Paragraphs key on `w14:paraId` when Word stamped one; otherwise the container
//! is unkeyed and the diff aligns paragraphs by sequence.

use crate::model::{Container, DocModel, Format, ReviewError, Unit};
use crate::opc::Opc;
use crate::xmlutil::{attr, decode, unique_keys, xml_err};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

pub fn project(opc: &Opc) -> Result<DocModel, ReviewError> {
    let bytes = opc
        .get("word/document.xml")
        .ok_or_else(|| ReviewError::MissingPart("word/document.xml".into()))?;
    let part_sha = blake3::hash(bytes).to_hex().to_string();
    let xml = String::from_utf8_lossy(bytes);
    let mut reader = Reader::from_str(xml.as_ref());

    let mut units: Vec<Unit> = Vec::new();
    let mut in_text = false;
    let mut buf = String::new();
    let mut para_id: Option<String> = None;
    let mut order = 0usize;

    loop {
        match reader.read_event().map_err(xml_err)? {
            Event::Start(e) => match e.local_name().as_ref() {
                b"p" => {
                    buf.clear();
                    para_id = attr(&e, b"w14:paraId");
                }
                b"t" => in_text = true,
                _ => {}
            },
            Event::Text(t) if in_text => buf.push_str(&decode(t)?),
            Event::End(e) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" => {
                    let text = buf.trim().to_string();
                    if !text.is_empty() {
                        units.push(Unit {
                            key: para_id.take(),
                            label: format!("¶{}", order + 1),
                            order,
                            text,
                            formula: None,
                        });
                        order += 1;
                    } else {
                        para_id = None;
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }

    let keyed = unique_keys(&units);
    Ok(DocModel {
        format: Format::Docx,
        containers: vec![Container {
            key: "body".into(),
            label: "Document body".into(),
            order: 0,
            part_sha,
            keyed,
            units,
        }],
    })
}
