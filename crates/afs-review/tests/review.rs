//! End-to-end tests over synthetic OOXML packages.
//!
//! Fixtures are built in-process as STORE (uncompressed) zips of hand-written
//! XML parts — no binary blobs in the repo, and full control over exactly what
//! changed between "base" and "proposed". `afs_review::Opc` reads STORE and
//! DEFLATE alike, so these exercise the real projection + diff path.

use afs_review::{review, ChangeKind, Format};

// --- a minimal STORE-method zip writer (mirror of the OPC reader) ------------

fn zip_store(files: &[(&str, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    for (name, data) in files {
        offsets.push(out.len() as u32);
        let (nb, db) = (name.as_bytes(), data.as_bytes());
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local file header sig
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method = STORE
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc32 (reader ignores)
        out.extend_from_slice(&(db.len() as u32).to_le_bytes()); // compressed size
        out.extend_from_slice(&(db.len() as u32).to_le_bytes()); // uncompressed size
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes()); // name len
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(nb);
        out.extend_from_slice(db);
    }
    let cd_off = out.len() as u32;
    let mut central = Vec::new();
    for (i, (name, data)) in files.iter().enumerate() {
        let (nb, db) = (name.as_bytes(), data.as_bytes());
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central header sig
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0u16.to_le_bytes()); // flags
        central.extend_from_slice(&0u16.to_le_bytes()); // method
        central.extend_from_slice(&0u16.to_le_bytes()); // time
        central.extend_from_slice(&0u16.to_le_bytes()); // date
        central.extend_from_slice(&0u32.to_le_bytes()); // crc
        central.extend_from_slice(&(db.len() as u32).to_le_bytes()); // comp size
        central.extend_from_slice(&(db.len() as u32).to_le_bytes()); // uncomp size
        central.extend_from_slice(&(nb.len() as u16).to_le_bytes()); // name len
        central.extend_from_slice(&0u16.to_le_bytes()); // extra len
        central.extend_from_slice(&0u16.to_le_bytes()); // comment len
        central.extend_from_slice(&0u16.to_le_bytes()); // disk #
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offsets[i].to_le_bytes()); // local header offset
        central.extend_from_slice(nb);
    }
    let cd_size = central.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // EOCD sig
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
    out.extend_from_slice(&(files.len() as u16).to_le_bytes()); // entries this disk
    out.extend_from_slice(&(files.len() as u16).to_le_bytes()); // total entries
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_off.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

// --- docx --------------------------------------------------------------------

fn docx(paragraphs: &[(&str, &str)]) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|(id, text)| format!(r#"<w:p w14:paraId="{id}"><w:r><w:t>{text}</w:t></w:r></w:p>"#))
        .collect();
    let doc = format!(
        r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml"><w:body>{body}</w:body></w:document>"#
    );
    zip_store(&[("word/document.xml", &doc)])
}

/// A docx with an extra volatile part (docProps) that the projector must ignore.
fn docx_with_props(paragraphs: &[(&str, &str)], modified: &str) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|(id, text)| format!(r#"<w:p w14:paraId="{id}"><w:r><w:t>{text}</w:t></w:r></w:p>"#))
        .collect();
    let doc = format!(
        r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:w14="http://schemas.microsoft.com/office/word/2010/wordml"><w:body>{body}</w:body></w:document>"#
    );
    let core = format!(r#"<coreProperties><modified>{modified}</modified></coreProperties>"#);
    zip_store(&[("word/document.xml", &doc), ("docProps/core.xml", &core)])
}

#[test]
fn docx_paragraph_edit_is_a_single_change() {
    let base = docx(&[("p1", "Hello world"), ("p2", "Second paragraph")]);
    let proposed = docx(&[("p1", "Hello world"), ("p2", "Second paragraph, edited")]);
    let dv = review(Format::Docx, Some(&base), Some(&proposed)).unwrap();

    assert_eq!(dv.summary.units_changed, 1);
    assert_eq!(dv.summary.units_added, 0);
    assert_eq!(dv.summary.units_removed, 0);
    let body = dv
        .containers
        .iter()
        .find(|c| c.status == ChangeKind::Changed)
        .expect("body changed");
    assert_eq!(body.changes.len(), 1);
    let ch = &body.changes[0];
    assert_eq!(ch.kind, ChangeKind::Changed);
    assert_eq!(ch.before.as_deref(), Some("Second paragraph"));
    assert_eq!(ch.after.as_deref(), Some("Second paragraph, edited"));
}

#[test]
fn docx_resave_without_content_change_is_empty() {
    // Same paragraphs, different volatile docProps timestamp: normalization (we
    // only read word/document.xml) must yield no changes.
    let base = docx_with_props(&[("p1", "Alpha"), ("p2", "Beta")], "2026-01-01T00:00:00Z");
    let proposed = docx_with_props(&[("p1", "Alpha"), ("p2", "Beta")], "2026-07-23T12:00:00Z");
    let dv = review(Format::Docx, Some(&base), Some(&proposed)).unwrap();

    assert_eq!(dv.summary.units_changed, 0);
    assert_eq!(dv.summary.units_added, 0);
    assert_eq!(dv.summary.units_removed, 0);
    assert!(dv
        .containers
        .iter()
        .all(|c| c.status == ChangeKind::Unchanged));
}

#[test]
fn docx_insert_paragraph_without_paraids_aligns_by_sequence() {
    // No w14:paraId => unkeyed => sequence alignment: inserting a middle
    // paragraph must read as one add, not "everything after shifted".
    let base_doc = r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>One</w:t></w:r></w:p><w:p><w:r><w:t>Three</w:t></w:r></w:p></w:body></w:document>"#;
    let prop_doc = r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>One</w:t></w:r></w:p><w:p><w:r><w:t>Two</w:t></w:r></w:p><w:p><w:r><w:t>Three</w:t></w:r></w:p></w:body></w:document>"#;
    let base = zip_store(&[("word/document.xml", base_doc)]);
    let proposed = zip_store(&[("word/document.xml", prop_doc)]);
    let dv = review(Format::Docx, Some(&base), Some(&proposed)).unwrap();

    assert_eq!(dv.summary.units_added, 1);
    assert_eq!(dv.summary.units_changed, 0);
    assert_eq!(dv.summary.units_removed, 0);
}

// --- pptx --------------------------------------------------------------------

fn slide(title: &str) -> String {
    format!(
        r#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{title}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
    )
}

fn pptx(order: &[&str], slides: &[(&str, &str)]) -> Vec<u8> {
    // order: rIds in sldIdLst order; slides: (rId->part filename, title)
    let sld_ids: String = order
        .iter()
        .enumerate()
        .map(|(i, rid)| format!(r#"<p:sldId id="{}" r:id="{rid}"/>"#, 256 + i))
        .collect();
    let pres = format!(
        r#"<p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst>{sld_ids}</p:sldIdLst></p:presentation>"#
    );
    let rels_body: String = slides
        .iter()
        .enumerate()
        .map(|(i, (rid, _))| {
            format!(
                r#"<Relationship Id="{rid}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{}.xml"/>"#,
                i + 1
            )
        })
        .collect();
    let rels = format!(
        r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels_body}</Relationships>"#
    );
    let mut files: Vec<(String, String)> = vec![
        ("ppt/presentation.xml".into(), pres),
        ("ppt/_rels/presentation.xml.rels".into(), rels),
    ];
    for (i, (_, title)) in slides.iter().enumerate() {
        files.push((format!("ppt/slides/slide{}.xml", i + 1), slide(title)));
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    zip_store(&refs)
}

#[test]
fn pptx_slide_title_edit_is_a_change() {
    let base = pptx(&["rId1", "rId2"], &[("rId1", "Intro"), ("rId2", "Details")]);
    let proposed = pptx(
        &["rId1", "rId2"],
        &[("rId1", "Introduction"), ("rId2", "Details")],
    );
    let dv = review(Format::Pptx, Some(&base), Some(&proposed)).unwrap();

    assert_eq!(dv.summary.containers_changed, 1);
    let changed = dv
        .containers
        .iter()
        .find(|c| c.status == ChangeKind::Changed)
        .unwrap();
    assert_eq!(changed.changes[0].before.as_deref(), Some("Intro"));
    assert_eq!(changed.changes[0].after.as_deref(), Some("Introduction"));
}

#[test]
fn pptx_reordering_slides_is_a_move_not_rewrite() {
    let base = pptx(&["rId1", "rId2"], &[("rId1", "First"), ("rId2", "Second")]);
    // Same slides, swapped order in sldIdLst.
    let proposed = pptx(&["rId2", "rId1"], &[("rId1", "First"), ("rId2", "Second")]);
    let dv = review(Format::Pptx, Some(&base), Some(&proposed)).unwrap();

    assert_eq!(dv.summary.containers_moved, 2);
    assert_eq!(dv.summary.containers_changed, 0);
    assert_eq!(dv.summary.units_changed, 0);
    assert!(dv.containers.iter().all(|c| c.status == ChangeKind::Moved));
}

// --- xlsx --------------------------------------------------------------------

fn xlsx(shared: &[&str], cells: &str) -> Vec<u8> {
    let sst_items: String = shared
        .iter()
        .map(|s| format!("<si><t>{s}</t></si>"))
        .collect();
    let sst = format!(
        r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{n}" uniqueCount="{n}">{sst_items}</sst>"#,
        n = shared.len()
    );
    let workbook = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;
    let sheet = format!(
        r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{cells}</sheetData></worksheet>"#
    );
    zip_store(&[
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", rels),
        ("xl/sharedStrings.xml", &sst),
        ("xl/worksheets/sheet1.xml", &sheet),
    ])
}

#[test]
fn xlsx_cell_value_edit_resolves_shared_strings() {
    let base = xlsx(
        &["Revenue"],
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>100</v></c></row>"#,
    );
    let proposed = xlsx(
        &["Revenue"],
        r#"<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>150</v></c></row>"#,
    );
    let dv = review(Format::Xlsx, Some(&base), Some(&proposed)).unwrap();

    assert_eq!(dv.summary.units_changed, 1);
    let sheet = dv
        .containers
        .iter()
        .find(|c| c.status == ChangeKind::Changed)
        .unwrap();
    let ch = &sheet.changes[0];
    assert_eq!(ch.label, "B1");
    assert_eq!(ch.before.as_deref(), Some("100"));
    assert_eq!(ch.after.as_deref(), Some("150"));
    // A1 (the shared string "Revenue") is unchanged and not reported.
    assert!(sheet.changes.iter().all(|c| c.label != "A1"));
}

#[test]
fn xlsx_formula_recalc_is_flagged_not_treated_as_an_edit() {
    // Same formula, different cached value => recalc_only.
    let base = xlsx(
        &[],
        r#"<row r="1"><c r="B2"><f>B1*2</f><v>200</v></c></row>"#,
    );
    let proposed = xlsx(
        &[],
        r#"<row r="1"><c r="B2"><f>B1*2</f><v>300</v></c></row>"#,
    );
    let dv = review(Format::Xlsx, Some(&base), Some(&proposed)).unwrap();

    let ch = &dv
        .containers
        .iter()
        .find(|c| c.status == ChangeKind::Changed)
        .unwrap()
        .changes[0];
    assert!(
        ch.recalc_only,
        "same formula, changed cached value => recalc"
    );
    assert_eq!(ch.after_formula.as_deref(), Some("B1*2"));
}

#[test]
fn xlsx_added_cell_is_an_addition() {
    let base = xlsx(&[], r#"<row r="1"><c r="A1"><v>1</v></c></row>"#);
    let proposed = xlsx(
        &[],
        r#"<row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row>"#,
    );
    let dv = review(Format::Xlsx, Some(&base), Some(&proposed)).unwrap();

    assert_eq!(dv.summary.units_added, 1);
    let sheet = &dv.containers[0];
    let added = sheet
        .changes
        .iter()
        .find(|c| c.kind == ChangeKind::Added)
        .unwrap();
    assert_eq!(added.label, "B1");
    assert_eq!(added.after.as_deref(), Some("2"));
}

// --- review() edge cases + serialization ------------------------------------

#[test]
fn format_detection() {
    assert_eq!(Format::from_path("/reports/q3.docx"), Some(Format::Docx));
    assert_eq!(Format::from_path("/deck.PPTX"), Some(Format::Pptx));
    assert_eq!(Format::from_path("/model.xlsx"), Some(Format::Xlsx));
    assert_eq!(Format::from_path("/notes.txt"), None);
    assert_eq!(Format::from_path("/no-extension"), None);
}

#[test]
fn new_document_and_deletion_are_noted() {
    let doc = docx(&[("p1", "Only line")]);
    let created = review(Format::Docx, None, Some(&doc)).unwrap();
    assert_eq!(created.note.as_deref(), Some("new document"));
    assert!(created.summary.units_added >= 1);

    let deleted = review(Format::Docx, Some(&doc), None).unwrap();
    assert_eq!(deleted.note.as_deref(), Some("proposed deletion"));
    assert!(deleted.summary.units_removed >= 1);
}

#[test]
fn diffview_serializes_to_json() {
    let base = xlsx(&["A"], r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#);
    let proposed = xlsx(&["B"], r#"<row r="1"><c r="A1" t="s"><v>0</v></c></row>"#);
    let dv = review(Format::Xlsx, Some(&base), Some(&proposed)).unwrap();
    let json = serde_json::to_value(&dv).unwrap();
    assert_eq!(json["format"], "xlsx");
    assert!(json["summary"].is_object());
    assert!(json["containers"].is_array());
}

#[test]
fn corrupt_package_errors_cleanly() {
    let err = review(Format::Docx, Some(b"not a zip at all"), Some(b"still not")).unwrap_err();
    // A garbage package is a plain error, never a panic.
    assert!(matches!(err, afs_review::ReviewError::Zip(_)));
}
