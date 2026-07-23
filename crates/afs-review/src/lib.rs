//! afs-review — reviewable diffs of Office documents (Word / PowerPoint / Excel)
//! for the afs agent-suggestion queue.
//!
//! The built-in `suggestion_diff` renders raw-byte line noise for a `.docx`,
//! `.pptx`, or `.xlsx` (they are zipped OOXML). This crate instead **projects**
//! each version into a shared model (`model`) — containers of units — and diffs
//! the models (`diff`), so a reviewer sees changed paragraphs, slides, and cells.
//! It is a pure `bytes -> DiffView` library with no afs dependency; a surface
//! (afs-api) fetches a suggestion's base/proposed bytes and calls [`review`].
//!
//! See `docs/proposals/office-suggestion-review.md` for the full design.

mod diff;
mod docx;
mod model;
mod opc;
mod pptx;
mod xlsx;
mod xmlutil;

pub use diff::{ChangeKind, ContainerDiff, DiffView, Summary, UnitChange};
pub use model::{Container, DocModel, Format, ReviewError, Unit};

/// Project one document's bytes into the unified model.
pub fn project(format: Format, bytes: &[u8]) -> Result<DocModel, ReviewError> {
    let opc = opc::Opc::parse(bytes)?;
    match format {
        Format::Docx => docx::project(&opc),
        Format::Pptx => pptx::project(&opc),
        Format::Xlsx => xlsx::project(&opc),
    }
}

/// Review a suggested change: diff the `base` (current) bytes against the
/// `proposed` bytes. `base = None` is a new document (everything added);
/// `proposed = None` is a proposed deletion (everything removed).
pub fn review(
    format: Format,
    base: Option<&[u8]>,
    proposed: Option<&[u8]>,
) -> Result<DiffView, ReviewError> {
    let empty = DocModel {
        format,
        containers: Vec::new(),
    };
    match (base, proposed) {
        (Some(b), Some(p)) => Ok(diff::diff_models(
            &project(format, b)?,
            &project(format, p)?,
        )),
        (None, Some(p)) => {
            let mut dv = diff::diff_models(&empty, &project(format, p)?);
            dv.set_note("new document");
            Ok(dv)
        }
        (Some(b), None) => {
            let mut dv = diff::diff_models(&project(format, b)?, &empty);
            dv.set_note("proposed deletion");
            Ok(dv)
        }
        (None, None) => Ok(DiffView::empty(format)),
    }
}
