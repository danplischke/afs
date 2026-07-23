//! The format-agnostic diff engine: it operates purely on [`DocModel`] and never
//! imports a format library, so adding a format is a new projector, nothing here.
//!
//! Containers align by `key` (so a reordered slide/sheet is a *move*, not
//! add+delete). Within a changed container, units align by key when they carry
//! stable ones (cells, shapes, paragraphs with ids), else by sequence via
//! `similar`. Unchanged containers (equal `part_sha`) are skipped outright.

use crate::model::{Container, DocModel, Format, Unit};
use serde::Serialize;
use similar::{capture_diff_slices, Algorithm, DiffTag};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
    Moved,
    Unchanged,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub containers_changed: usize,
    pub containers_added: usize,
    pub containers_removed: usize,
    pub containers_moved: usize,
    pub units_added: usize,
    pub units_removed: usize,
    pub units_changed: usize,
}

/// One changed unit (paragraph / shape / cell).
#[derive(Debug, Clone, Serialize)]
pub struct UnitChange {
    pub kind: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_formula: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_formula: Option<String>,
    /// The formula is unchanged but its cached value differs — a recalculation,
    /// not an edit. Lets a UI de-emphasize it.
    #[serde(skip_serializing_if = "is_false")]
    pub recalc_only: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerDiff {
    pub key: String,
    pub label: String,
    pub status: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_order: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_order: Option<usize>,
    pub changes: Vec<UnitChange>,
}

/// The reviewable diff of a whole document — what the API serializes for a UI.
#[derive(Debug, Clone, Serialize)]
pub struct DiffView {
    pub format: Format,
    pub summary: Summary,
    pub containers: Vec<ContainerDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl DiffView {
    pub fn empty(format: Format) -> Self {
        Self {
            format,
            summary: Summary::default(),
            containers: Vec::new(),
            note: None,
        }
    }

    pub fn set_note(&mut self, note: &str) {
        self.note = Some(note.to_string());
    }
}

/// Diff two projected documents.
pub fn diff_models(base: &DocModel, proposed: &DocModel) -> DiffView {
    let mut summary = Summary::default();
    let mut containers = Vec::new();
    let mut proposed_by_key: HashMap<&str, &Container> = proposed
        .containers
        .iter()
        .map(|c| (c.key.as_str(), c))
        .collect();

    // Base containers, in base order: matched (compare) or removed.
    for b in &base.containers {
        if let Some(p) = proposed_by_key.remove(b.key.as_str()) {
            let content_changed = b.part_sha != p.part_sha;
            let moved = b.order != p.order;
            let changes = if content_changed {
                diff_units(&b.units, &p.units, b.keyed && p.keyed, &mut summary)
            } else {
                Vec::new()
            };
            let status = if content_changed {
                summary.containers_changed += 1;
                ChangeKind::Changed
            } else if moved {
                summary.containers_moved += 1;
                ChangeKind::Moved
            } else {
                ChangeKind::Unchanged
            };
            containers.push(ContainerDiff {
                key: b.key.clone(),
                label: p.label.clone(),
                status,
                from_order: moved.then_some(b.order),
                to_order: moved.then_some(p.order),
                changes,
            });
        } else {
            summary.containers_removed += 1;
            summary.units_removed += b.units.len();
            containers.push(ContainerDiff {
                key: b.key.clone(),
                label: b.label.clone(),
                status: ChangeKind::Removed,
                from_order: Some(b.order),
                to_order: None,
                changes: b.units.iter().map(unit_removed).collect(),
            });
        }
    }

    // Whatever proposed containers went unmatched are additions (in their order).
    let mut added: Vec<&Container> = proposed_by_key.into_values().collect();
    added.sort_by_key(|c| c.order);
    for p in added {
        summary.containers_added += 1;
        summary.units_added += p.units.len();
        containers.push(ContainerDiff {
            key: p.key.clone(),
            label: p.label.clone(),
            status: ChangeKind::Added,
            from_order: None,
            to_order: Some(p.order),
            changes: p.units.iter().map(unit_added).collect(),
        });
    }

    DiffView {
        format: proposed.format,
        summary,
        containers,
        note: None,
    }
}

fn diff_units(
    base: &[Unit],
    proposed: &[Unit],
    keyed: bool,
    summary: &mut Summary,
) -> Vec<UnitChange> {
    let changes = if keyed {
        diff_units_keyed(base, proposed)
    } else {
        diff_units_seq(base, proposed)
    };
    for c in &changes {
        match c.kind {
            ChangeKind::Added => summary.units_added += 1,
            ChangeKind::Removed => summary.units_removed += 1,
            ChangeKind::Changed => summary.units_changed += 1,
            _ => {}
        }
    }
    changes
}

fn diff_units_keyed(base: &[Unit], proposed: &[Unit]) -> Vec<UnitChange> {
    let mut out = Vec::new();
    let mut prop: HashMap<&str, &Unit> = proposed
        .iter()
        .filter_map(|u| u.key.as_deref().map(|k| (k, u)))
        .collect();
    // Removed / changed, in base order.
    for b in base {
        let k = b.key.as_deref().unwrap_or_default();
        if let Some(p) = prop.remove(k) {
            if let Some(uc) = unit_changed(b, p) {
                out.push(uc);
            }
        } else {
            out.push(unit_removed(b));
        }
    }
    // Added: proposed units whose key never matched, in proposed order.
    for p in proposed {
        if p.key.as_deref().is_some_and(|k| prop.contains_key(k)) {
            out.push(unit_added(p));
            prop.remove(p.key.as_deref().unwrap());
        }
    }
    out
}

fn diff_units_seq(base: &[Unit], proposed: &[Unit]) -> Vec<UnitChange> {
    let bsig: Vec<String> = base.iter().map(sig).collect();
    let psig: Vec<String> = proposed.iter().map(sig).collect();
    let mut out = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, &bsig, &psig) {
        match op.tag() {
            DiffTag::Equal => {}
            DiffTag::Delete => out.extend(op.old_range().map(|i| unit_removed(&base[i]))),
            DiffTag::Insert => out.extend(op.new_range().map(|j| unit_added(&proposed[j]))),
            DiffTag::Replace => {
                let old: Vec<usize> = op.old_range().collect();
                let new: Vec<usize> = op.new_range().collect();
                let common = old.len().min(new.len());
                for k in 0..common {
                    if let Some(uc) = unit_changed(&base[old[k]], &proposed[new[k]]) {
                        out.push(uc);
                    }
                }
                out.extend(old[common..].iter().map(|&i| unit_removed(&base[i])));
                out.extend(new[common..].iter().map(|&j| unit_added(&proposed[j])));
            }
        }
    }
    out
}

/// Compare two aligned units; `None` if identical.
fn unit_changed(b: &Unit, p: &Unit) -> Option<UnitChange> {
    if b.text == p.text && b.formula == p.formula {
        return None;
    }
    let recalc_only = b.formula.is_some() && b.formula == p.formula && b.text != p.text;
    Some(UnitChange {
        kind: ChangeKind::Changed,
        key: p.key.clone().or_else(|| b.key.clone()),
        label: p.label.clone(),
        before: Some(b.text.clone()),
        after: Some(p.text.clone()),
        before_formula: b.formula.clone(),
        after_formula: p.formula.clone(),
        recalc_only,
    })
}

fn unit_added(u: &Unit) -> UnitChange {
    UnitChange {
        kind: ChangeKind::Added,
        key: u.key.clone(),
        label: u.label.clone(),
        before: None,
        after: Some(u.text.clone()),
        before_formula: None,
        after_formula: u.formula.clone(),
        recalc_only: false,
    }
}

fn unit_removed(u: &Unit) -> UnitChange {
    UnitChange {
        kind: ChangeKind::Removed,
        key: u.key.clone(),
        label: u.label.clone(),
        before: Some(u.text.clone()),
        after: None,
        before_formula: u.formula.clone(),
        after_formula: None,
        recalc_only: false,
    }
}

/// A comparison signature for sequence alignment (text + formula).
fn sig(u: &Unit) -> String {
    match &u.formula {
        Some(f) => format!("{}\u{0}{f}", u.text),
        None => u.text.clone(),
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}
