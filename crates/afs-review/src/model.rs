//! The unified projection model — one shape every Office format projects into.
//!
//! A `.docx`, `.pptx`, and `.xlsx` have different internal structure (a linear
//! block stream, slides→shapes, sheets→cells), but all three project into the
//! same [`DocModel`]: a list of [`Container`]s (a Word section, a slide, a
//! worksheet), each holding [`Unit`]s (a paragraph, a shape's text, a cell). One
//! model means one diff engine (`diff.rs`) and one API serve every format; the
//! only format-specific code is the projector that fills this model.

use serde::Serialize;

/// A supported Office document format, detected from a path's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Docx,
    Pptx,
    Xlsx,
}

impl Format {
    /// Detect the format from a file path's extension (case-insensitive), or
    /// `None` for anything that isn't a supported Office document.
    pub fn from_path(path: &str) -> Option<Format> {
        match path.rsplit('.').next()?.to_ascii_lowercase().as_str() {
            "docx" => Some(Format::Docx),
            "pptx" => Some(Format::Pptx),
            "xlsx" => Some(Format::Xlsx),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Format::Docx => "docx",
            Format::Pptx => "pptx",
            Format::Xlsx => "xlsx",
        }
    }
}

/// A whole document, projected into containers of units.
#[derive(Debug, Clone, Serialize)]
pub struct DocModel {
    pub format: Format,
    pub containers: Vec<Container>,
}

/// A grouping within a document: a Word section (or the single body), a slide,
/// or a worksheet. `key` is a **stable identity** (slide part name, sheet name,
/// `"body"`) so the diff aligns containers across versions and can tell a
/// *moved* slide from an add+delete.
#[derive(Debug, Clone, Serialize)]
pub struct Container {
    /// Stable identity used to align base↔proposed (not the display order).
    pub key: String,
    /// Human label, e.g. `"Slide 3"` or `"Sheet: Q3"`.
    pub label: String,
    /// Display position (0-based). A change in `order` alone is a *move*.
    pub order: usize,
    /// Hash of the decompressed source part(s); equal hashes ⇒ skip diffing.
    pub part_sha: String,
    /// Whether `units` carry stable, unique keys (so they align by key rather
    /// than by sequence).
    pub keyed: bool,
    pub units: Vec<Unit>,
}

/// The smallest reviewable thing: a paragraph, a shape's text, or a cell.
#[derive(Debug, Clone, Serialize)]
pub struct Unit {
    /// Stable address when the format provides one (cell ref `A1`, a paragraph
    /// id, a shape id); `None` falls back to sequence alignment.
    pub key: Option<String>,
    /// Human locus, e.g. `"A1"`, `"¶3"`, `"title placeholder"`.
    pub label: String,
    pub order: usize,
    /// Normalized display text (shared strings resolved, runs coalesced).
    pub text: String,
    /// A cell's formula (`=SUM(...)`), when present. `None` for prose.
    pub formula: Option<String>,
}

/// A projection/diff failure. The bytes come from a suggestion (agent- or
/// user-supplied), so a malformed package is an ordinary error, never a panic.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("not a valid zip/OPC package: {0}")]
    Zip(String),
    #[error("missing required part: {0}")]
    MissingPart(String),
    #[error("xml parse error: {0}")]
    Xml(String),
}
