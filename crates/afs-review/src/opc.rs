//! A minimal read-only OPC (Open Packaging Conventions) reader.
//!
//! `.docx`/`.pptx`/`.xlsx` are ZIP packages. We only need to pull named XML
//! parts out, so this is a small central-directory-driven ZIP reader (STORE +
//! DEFLATE via `flate2`) rather than a full zip dependency. Sizes and offsets
//! are read from the **central directory**, which sidesteps streaming
//! data-descriptor quirks. Everything is bounds-checked: the bytes come from a
//! suggestion, so a corrupt or hostile package must error, never panic.

use crate::model::ReviewError;
use crate::xmlutil::attr;
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;

const EOCD_SIG: u32 = 0x0605_4b50;
const CDH_SIG: u32 = 0x0201_4b50;
const LFH_SIG: u32 = 0x0403_4b50;
/// A ZIP64 sentinel in the 32-bit size/offset fields; unsupported here.
const ZIP64_SENTINEL: usize = 0xFFFF_FFFF;

/// A parsed package: part name → decompressed bytes.
pub struct Opc {
    entries: HashMap<String, Vec<u8>>,
}

impl Opc {
    /// Parse a ZIP/OPC package from its raw bytes.
    pub fn parse(data: &[u8]) -> Result<Opc, ReviewError> {
        let eocd = find_eocd(data)
            .ok_or_else(|| ReviewError::Zip("no end-of-central-directory record".into()))?;
        let total = u16le(data, eocd + 10).ok_or_else(trunc)?;
        let cd_off = u32le(data, eocd + 16).ok_or_else(trunc)?;

        let mut entries = HashMap::new();
        let mut pos = cd_off;
        for _ in 0..total {
            if u32le(data, pos).ok_or_else(trunc)? != CDH_SIG as usize {
                break;
            }
            let method = u16le(data, pos + 10).ok_or_else(trunc)?;
            let comp_size = u32le(data, pos + 20).ok_or_else(trunc)?;
            let name_len = u16le(data, pos + 28).ok_or_else(trunc)?;
            let extra_len = u16le(data, pos + 30).ok_or_else(trunc)?;
            let comment_len = u16le(data, pos + 32).ok_or_else(trunc)?;
            let lho = u32le(data, pos + 42).ok_or_else(trunc)?;
            let name = data.get(pos + 46..pos + 46 + name_len).ok_or_else(trunc)?;
            let name = String::from_utf8_lossy(name).into_owned();

            if comp_size == ZIP64_SENTINEL || lho == ZIP64_SENTINEL {
                return Err(ReviewError::Zip(format!("ZIP64 entry unsupported: {name}")));
            }

            // Locate the data via the *local* header (its name/extra lengths can
            // differ from the central directory's).
            if u32le(data, lho).ok_or_else(trunc)? != LFH_SIG as usize {
                return Err(ReviewError::Zip(format!("bad local header for {name}")));
            }
            let l_name_len = u16le(data, lho + 26).ok_or_else(trunc)?;
            let l_extra_len = u16le(data, lho + 28).ok_or_else(trunc)?;
            let start = lho + 30 + l_name_len + l_extra_len;
            let raw = data.get(start..start + comp_size).ok_or_else(trunc)?;

            let bytes = match method {
                0 => raw.to_vec(),
                8 => inflate(raw)?,
                m => {
                    return Err(ReviewError::Zip(format!(
                        "unsupported compression {m} for {name}"
                    )))
                }
            };
            entries.insert(name, bytes);

            pos += 46 + name_len + extra_len + comment_len;
        }
        Ok(Opc { entries })
    }

    /// The raw bytes of a part, if present.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.entries.get(name).map(Vec::as_slice)
    }

    /// A part decoded as text (lossy UTF-8 — OOXML parts are UTF-8).
    pub fn get_str(&self, name: &str) -> Option<Cow<'_, str>> {
        self.entries.get(name).map(|b| String::from_utf8_lossy(b))
    }
}

/// Parse an OPC `.rels` part into `RelationshipId → Target`.
pub fn read_rels(opc: &Opc, part: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(xml) = opc.get_str(part) else {
        return map;
    };
    let mut reader = Reader::from_str(xml.as_ref());
    loop {
        match reader.read_event() {
            Ok(Event::Empty(e)) | Ok(Event::Start(e))
                if e.local_name().as_ref() == b"Relationship" =>
            {
                if let (Some(id), Some(target)) = (attr(&e, b"Id"), attr(&e, b"Target")) {
                    map.insert(id, target);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    map
}

/// Resolve a relationship `Target` (relative to the `.rels` part's owning
/// directory) to a package part name. Handles `/`-absolute targets and `..`.
pub fn resolve_target(base_dir: &str, target: &str) -> String {
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut segs: Vec<&str> = base_dir.split('/').filter(|s| !s.is_empty()).collect();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            p => segs.push(p),
        }
    }
    segs.join("/")
}

fn inflate(raw: &[u8]) -> Result<Vec<u8>, ReviewError> {
    let mut out = Vec::new();
    flate2::read::DeflateDecoder::new(raw)
        .read_to_end(&mut out)
        .map_err(|e| ReviewError::Zip(format!("inflate failed: {e}")))?;
    Ok(out)
}

/// Find the End Of Central Directory record by scanning backward (its trailing
/// comment is variable-length, so we can't index it directly). Bounded to the
/// last 64 KiB + header, the maximum a comment can push it back.
fn find_eocd(data: &[u8]) -> Option<usize> {
    let min = 22;
    if data.len() < min {
        return None;
    }
    let scan_start = data.len().saturating_sub(min + 0xFFFF);
    (scan_start..=data.len() - min)
        .rev()
        .find(|&i| u32le(data, i) == Some(EOCD_SIG as usize))
}

fn u16le(b: &[u8], off: usize) -> Option<usize> {
    b.get(off..off + 2)
        .map(|s| s[0] as usize | (s[1] as usize) << 8)
}

fn u32le(b: &[u8], off: usize) -> Option<usize> {
    b.get(off..off + 4).map(|s| {
        s[0] as usize | (s[1] as usize) << 8 | (s[2] as usize) << 16 | (s[3] as usize) << 24
    })
}

fn trunc() -> ReviewError {
    ReviewError::Zip("truncated or malformed zip structure".into())
}
