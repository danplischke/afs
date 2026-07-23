# Proposal: reviewable suggestions for Office documents (Word / PowerPoint / Excel)

> Status: **proposal / RFC**. Not yet implemented. Extends the agent-suggestion
> review queue (`docs/DESIGN.md` §6, `crates/afs-core/src/suggest.rs`) so a
> reviewer can see a *meaningful* before/after for a `.docx`, `.pptx`, or `.xlsx`
> — not the byte noise the built-in text diff produces for a zipped binary.

## 1. Goal & non-goals

**Goal.** When an agent `suggest`s an edit to an Office document, a human
reviewing it in a UI should see a human-readable diff (changed paragraphs,
changed slides, changed cells) and accept/reject with confidence — for Word,
PowerPoint, and Excel, through one code path.

**Non-goals.**
- **No document semantics in `afs-core`.** The storage engine stays byte-opaque
  (a core invariant — see `CLAUDE.md`, "never put document semantics in the
  engine"). Everything here is a new layer *above* `afs-sdk`.
- Not real-time co-editing (that's the CRDT path in `DESIGN.md` §4e). This is
  propose → review → accept on whole-file suggestions.
- Not a general document-conversion product. We project only enough to *review*.

## 2. Why the built-in diff fails here

`Fs::suggestion_diff` (`suggest.rs:204`) does `String::from_utf8_lossy(bytes)`
then a line diff. A `.docx`/`.pptx`/`.xlsx` is an **OPC package** — a ZIP of XML
parts — so that projection is garbage and the diff is unreviewable. afs correctly
treats these as opaque content-addressed bytes; making them reviewable is a
*projection* concern, and projection is what this proposal adds.

## 3. The one idea: project every format into a shared model

Three formats, three internal shapes:

| Format | Native shape | OPC parts that carry meaning |
|---|---|---|
| Word `.docx` | linear stream of blocks | `word/document.xml` (`<w:p>`→`<w:r>`→`<w:t>`), headers/footers/footnotes |
| PowerPoint `.pptx` | slides → shapes → text | `ppt/presentation.xml` order + `ppt/slides/slideN.xml` (`<a:p>`→`<a:r>`→`<a:t>`) |
| Excel `.xlsx` | sheets → 2-D cell grid | `xl/workbook.xml` order + `xl/worksheets/sheetN.xml` (`<c r="A1">`→`<v>`/`<f>`) + `xl/sharedStrings.xml` |

Rather than three viewers, every projector emits **one model**:

```
DocModel   { kind, containers: [Container] }
Container  { key, label, order, part_sha, units: [Unit] }   // section | slide | sheet
Unit       { key, order, text, formula?, style?, }          // paragraph | shape-text | cell
```

- **Container** = the natural grouping: a Word section (or the single body), a
  slide, a worksheet. `part_sha` is the hash of the decompressed OPC part, so an
  unchanged container is detected — and skipped — *without* diffing or rendering.
- **Unit** = the smallest reviewable thing: a paragraph, a shape's text, a cell.
  `key` is a **stable address** (see §5) so the diff aligns semantically instead
  of by position. `formula` carries an Excel cell's `<f>`; `style` is an optional
  normalized formatting fingerprint.

One model ⇒ **one diff engine and one API** cover all three formats; per-format
code is confined to the projector that fills the model and the renderer that
presents it.

## 4. Format-agnostic diff engine

```
diff(base: DocModel, proposed: DocModel) -> DiffView:
  align containers by key           # sheet name / slide part / "body" — survives reorder
  for each aligned (a, b):
    if a.part_sha == b.part_sha: continue          # unchanged: no work
    align units by key (fallback: content similarity when no stable key)
    classify each unit: Added | Removed | Changed{text,formula,style} | Moved{Δorder}
  containers only on one side -> Added / Removed (+ Moved by order delta)
DiffView { containers: [{label, changes:[UnitChange]}], summary: {added,removed,changed,moved} }
```

The engine never imports a format library — it operates purely on `DocModel`.
Adding a fourth format later (`.odt`, Google export, …) is a new `Projector`,
nothing else.

## 5. Per-format projectors (the only format-specific code)

Each projector: unzip the OPC package (Rust `zip`), read the meaningful parts
(`quick-xml`/`roxmltree`; **`calamine`** does all of xlsx cell/formula reading),
emit `Container`/`Unit`s with stable keys, and **normalize** so cosmetic churn
doesn't show as a change. Normalization is the projector's real value — the apps
rewrite the whole package on every save.

**Word (`DocxProjector`).**
- Containers: one `body` (optionally split by `Heading` for nicer grouping).
- Unit key: `w14:paraId` when present (Word stamps stable paragraph IDs); else
  align by content similarity (Myers over paragraph text) so a lightly-edited
  paragraph reads as *Changed*, not Removed+Added.
- Normalize: coalesce adjacent `<w:r>` runs with equal formatting; drop revision
  attrs (`w:rsidR`, proofing state); ignore `docProps/*`.

**PowerPoint (`PptxProjector`).**
- Containers: slides, **ordered by `<p:sldIdLst>` in `presentation.xml`** (the
  `slideN.xml` filename is *not* the display order); container key = the slide
  part name (survives reorder → detects "slide moved 3→5").
- Unit key: shape non-visual id `<p:cNvPr id=…>` / placeholder type (`title`,
  `body`); text from `<a:t>`.
- Normalize: ignore slide `modId`; drop `docProps/*`.

**Excel (`XlsxProjector`).**
- Containers: worksheets, ordered by `<sheets>` in `workbook.xml`; key = sheet
  name (fallback `sheetId`).
- Unit key: **cell reference** (`A1`, `B2`) — a naturally stable 2-D address.
  `text` = the resolved value (shared strings from `sharedStrings.xml` resolved to
  literals); `formula` = `<f>` if present.
- Normalize: **resolve shared-string indices to text** (indices renumber on every
  save); **ignore `xl/calcChain.xml`** (pure recalculation order); when a cell has
  a formula, diff the **formula** and treat a changed cached `<v>` with an
  unchanged formula as *recalculation*, not an edit (flag separately, off by
  default); compare a style fingerprint, not the volatile style index.

A correctness property falls out of good normalization: **open the file and save
it with no edits → the projected diff is empty.** That's the determinism test
(§9).

## 6. Rendering tiers (same model, richer view)

The UI asks for a tier; each is a renderer over the `DiffView`:

1. **Text** (default, cheap) — Word: prose unified/side-by-side; PowerPoint:
   per-slide text; Excel: a changed-cells list `Sheet!A1: 42 → 45`.
2. **Structural** — styles, moves, formula-vs-value for Excel, optional Word
   tracked-changes markup.
3. **Visual** — render base & proposed (or just changed containers) to images:
   Word/PowerPoint via LibreOffice headless (`soffice --headless --convert-to pdf`
   → `pdftoppm`); Excel as a highlighted HTML/PNG grid (a page render paginates
   badly for sheets). Lazy and cached; unchanged containers skipped via
   `part_sha`. Requires LibreOffice on the host → behind a feature flag / optional
   service.

## 7. Caching — content addressing makes it free

A projection is a pure function of the CAS hash, so it never needs invalidation:

```
projection_cache[(kind, projector_version, content_hash)] = DocModel
diffview_cache[(base_hash, proposed_hash, tier)]           = DiffView
```

`proposed_hash` is immutable; a given `base_hash` projects once, ever. The cache
can even live in the CAS itself (projections are just more content) or a simple
side table — start with a local cache dir.

## 8. Where it lives & API surface

- **New crate `crates/afs-review`** — the projectors, the unified model, the diff
  engine, the renderers. Depends on `afs-sdk`; **`afs-core` untouched.**
- **`afs-api` gains routes** (thin wrappers over `afs-review`):
  - `GET /content/{hash}` — raw bytes by content hash (read-only, cacheable
    forever). *This is the missing primitive today:* the suggestion DTO exposes
    `base_hash`/`proposed_hash` but there is no way to fetch those bytes; `GET
    /files/{path}` only returns the current working tree (and never the proposed
    body). ~15 lines over the CAS.
  - `GET /suggestions/{id}/review?tier=text|structural|visual` → the `DiffView`
    JSON (image URLs for the visual tier). This is what the UI renders.
  - `GET /suggestions/{id}/preview/{container}` → a rendered page/slide/grid PNG.
- Optionally surfaced through `afs-py` / the FastAPI router for Python UIs.

## 9. Testing

- **Golden fixtures** per format: a committed base + an edited proposed
  `.docx/.pptx/.xlsx`; assert the `DiffView` (which units changed).
- **Determinism test:** resave a fixture with no content change → assert an
  **empty** diff. This is what proves normalization strips volatile churn
  (docProps timestamps, shared-string/style renumbering, run reflow, calcChain).
- **Reorder test:** move a slide / rename a sheet → assert *Moved*, not
  Removed+Added.

## 10. Caveats & tradeoffs (carried from the design discussion)

- **Compression defeats afs's sub-file dedup for Office files.** A one-word change
  re-DEFLATEs the whole package, so FastCDC sees mostly-new bytes and cross-version
  chunk sharing is poor. Correctness is unaffected; snapshot economics aren't as
  cheap as for plain text. (An advanced projector could store a normalized/unzipped
  form for better diffing — out of scope here.)
- **Blame doesn't map onto the model.** afs blame is byte ranges over the *zip*,
  meaningless per-paragraph/cell. Block-level attribution comes from *the diff*
  (which units changed base→proposed) credited to the suggestion's `actor_id`, not
  from `GET /blame`.
- **Visual tier needs LibreOffice** → optional/feature-flagged; text & structural
  tiers are pure Rust and always available.

## 11. UI flow (unchanged suggestion workflow underneath)

1. **Inbox** — tail the change feed (`subscribe` on Postgres / `watch` on SQLite),
   filter `kind ∈ {suggest, accept, reject}`; badge pending suggestions.
2. **Open** — `GET /suggestions/{id}/review?tier=…`; render the diff/preview.
3. **Resolve** — Accept/Reject → `POST /suggestions/{id}/accept|reject` with the
   reviewer's **server-resolved** identity. Surface the two invariants already
   enforced in `suggest.rs`: **reviewer ≠ author**, and a **stale base** (the doc
   changed since the proposal) returns `Conflict` → show "document changed,
   re-review." Accept credits the original author in blame.

## 12. Phased plan

| Phase | Deliverable | Unlocks |
|---|---|---|
| **0** | `GET /content/{hash}` + `/suggestions/{id}/{base,proposed}` byte routes in `afs-api` | Any UI can fetch base & proposed bytes — unblocks everything, tiny diff |
| **1** | `afs-review` crate: unified model + diff engine + **text-tier** projectors for docx/pptx/xlsx; `GET /suggestions/{id}/review` | Real before/after for all three formats |
| **2** | Structural tier: styles, moves, Excel formula-vs-value, Word tracked-changes | Higher-fidelity review |
| **3** | Visual tier: LibreOffice render + Excel grid images, lazy + cached | Slide thumbnails / page & grid previews |
| **4** | Reference review UI over the change feed | End-to-end "see what the agent changed" |

## 13. Alternatives considered

- **Python projection service** (python-docx / python-pptx / openpyxl). Richest
  extraction and fastest to prototype, and consistent with the existing `afs-py`
  surface — but adds a Python runtime dependency to the review path. **Chosen:**
  Rust `afs-review` (zip + quick-xml + `calamine`) keeps it in-workspace and
  in-language; only the optional visual tier shells out. A Python service remains
  a viable drop-in for the projection layer if extraction needs outgrow the Rust
  libraries.
- **Diff the raw XML parts** instead of an extracted model. Rejected: OOXML is
  non-canonical (apps rewrite packages on save), so XML-text diffs are noisy;
  normalization into a model is the point.
- **Teach `afs-core` about content types.** Rejected: violates the byte-opaque
  storage invariant; projection belongs in a surface layer.
