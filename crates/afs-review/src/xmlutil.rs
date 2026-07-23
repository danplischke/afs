//! Small shared helpers over quick-xml used by every projector.

use crate::model::{ReviewError, Unit};
use quick_xml::events::{BytesStart, BytesText};
use std::collections::HashSet;

/// The value of an attribute by its full (possibly prefixed) name, e.g.
/// `b"r:id"`, `b"name"`, `b"Target"`. OOXML attribute values are ASCII
/// identifiers/refs in the cases we read, so a lossy UTF-8 decode is exact here;
/// element **text** (which can carry entities) is unescaped separately.
pub fn attr(e: &BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// Map any error with a `Display` into [`ReviewError::Xml`].
pub fn xml_err<E: std::fmt::Display>(e: E) -> ReviewError {
    ReviewError::Xml(e.to_string())
}

/// Decode a text node's content and resolve XML entities (`&amp;` → `&`). OOXML
/// parts are UTF-8. Named `decode` (not `text`) so it doesn't collide with the
/// `text` accumulators the projectors keep.
pub fn decode(t: BytesText) -> Result<String, ReviewError> {
    let raw = t.into_inner();
    let s = std::str::from_utf8(raw.as_ref()).map_err(xml_err)?;
    Ok(quick_xml::escape::unescape(s)
        .map_err(xml_err)?
        .into_owned())
}

/// Whether every unit carries a `Some` key and all such keys are unique — the
/// condition under which the diff engine aligns units by key instead of by
/// sequence. Vacuously true for an empty unit list.
pub fn unique_keys(units: &[Unit]) -> bool {
    let mut seen = HashSet::new();
    for u in units {
        match &u.key {
            Some(k) if seen.insert(k.as_str()) => {}
            _ => return false,
        }
    }
    true
}
