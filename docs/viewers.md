# Viewers

Drive owns a look at every file, and a quick edit where one is cheap, the
way Quick Look does on a Mac. Drive's inspector and its full-screen
preview use the viewers, and any app (Studio, Season) can embed the same
components. Heavy editing stays in each kind's own app. Written
2026-11-21.

## The crate: `drive-view`

| Feature | What it adds | Builds for |
|---|---|---|
| `ui` (default) | The Dioxus components: `Viewer`, `CSS` | web (and native, as a check) |
| `pdf` (default) | PDF pages, text, search, existing comments (pdfcraft-render) | web and native |
| `edit` | PDF page edits: rotate, delete, move (pdfcraft-organize) | native |
| `sign` | PDF signing with a PKCS #12 digital ID (pdfcraft-sign; aws-lc) | native only |

`Viewer { kind, parent, name, url, size, bearer, compact, on_anchor, on_save, on_pdf_edit }`
picks the viewer by the file's kind (the ids of `drive_spine::kinds`, with the
broader kind as a fallback for kinds apps register) and its name:

| Kind | Viewer | Quick edit |
|---|---|---|
| `text` `.md` | Markdown, rendered (raw HTML shown as text, links only to http(s), mailto, the same site) or as source | Text editor, saved as a new version |
| `text`, `sheet` `.csv/.tsv` | Plain text with line numbers | Text editor |
| `code`, `chart` `.kf/.musicxml` | Source with line numbers | Text editor |
| `image` (png, jpg, gif, webp, svg, avif, bmp) | The browser's `<img>`, fit or 100% | — |
| `audio` (not MIDI) | Waveform from the file's `peaks` sidecar ([sidecars.md](sidecars.md)), the file streamed to play; the browser decodes it only when there are no peaks | — |
| `video` | The browser's `<video>`, a time readout | — |
| `pdf` | pdfcraft: one page at a time on a canvas, zoom, find, the PDF's own comments | Rotate, delete, move a page (natively, on the node) |

Anything else shows its icon. HEIC and TIFF wait for a decoder (below).

### Anchors (for comments)

The viewers don't draw comments. They say where a person is pointing, as an
`Anchor` sent to `on_anchor` (serde, tagged `kind`):

- `text { start, end }`: a selection in a text, in UTF-16 units, read from
  the editor or from the page's selection;
- `time { at }`: audio or video, when paused or clicked on the waveform;
- `time_range { start, end }`: a drag across the waveform;
- `region { page, rect, user, text }`: a click (no size) or a drag on a PDF
  page or an image. For a PDF, `rect` is in the page's **view space**
  (points, y down, after `/Rotate`), `user` the same region in the PDF's
  own user space (y up, what an annotation's `/Rect` is in), and `text` the
  text it covers. For an image, `rect` is in the image's own pixels.

Drive's full-screen preview shows the latest anchor in its footer ("Page 2,
62×8 at 168, 211", "0:01.0 – 0:02.4") as a stand-in for the comment UI.

### In the Drive app

- **Inspector**: the picked file's compact viewer replaces its big icon
  when reading it is cheap: a file of the space's tree, a file kept on the
  node, or one of 8 MB or less on a computer (fetched through its agent).
  Bigger ones wait for **Preview**.
- **Full-screen preview**: **Preview**, or a double-click on a file. Esc
  closes it. For a file of the space's tree, the quick edits save a new
  version: the bytes go to `PUT /blobs/{hash}` and the entry points at the
  new hash (the same as an upload). A PDF page edit is `POST
  /pdf/edit/{hash}` on the node, which answers `{hash, size}`.
- **The node** answers `Range:` requests on `/blobs` (206), so video and
  long audio seek.

### PDF signing: the seam

`drive_view::sign::sign(pdf, p12, password, &SignRequest, unix_secs)` signs
natively. It is tested with a throwaway ID (`DRIVE_VIEW_TEST_P12`; see the
test). The private key must stay on the computer that has it, so the
browser never signs and the node doesn't either. The wiring is not built yet:

1. The browser asks `POST /pdf/sign/{hash}` with a `SignRequest` (field or
   page and rect, reason, location), and no key.
2. The node passes it to the signer's own agent over the connection the
   agent already keeps, the way `/fetch` asks an agent for a file.
3. The agent holds the PKCS #12 file and its password in its own config
   (mode 600, like its session). It confirms with the person, calls
   `sign`, and puts the result as a new blob. The app points the file at
   it.

`sign` is a non-default feature (aws-lc is a C build), so only the agent
turns it on.

## pdfcraft

[storytold/pdfcraft](https://github.com/storytold/pdfcraft), MIT OR
Apache-2.0, **v0.21.1**, pinned by its tag's commit
`b503b2e62dfeec5598f344e4e287d9c9cbdb34fa`. Crates used:

- `pdfcraft-render` (browser and native): pages to RGBA, text layer,
  search, page geometry, existing annotations (`inspect`).
- `pdfcraft-cos` and `pdfcraft-organize` (`edit`, native).
- `pdfcraft-sign` (`sign`, native).

Not `pdfcraft-engine`: OCR, JavaScript and XFA stay out of the browser.

pdfcraft builds against its own vendored hayro, lopdf and vello. Those are
`[patch.crates-io]` entries in its workspace, and a dependent does not
inherit them. Drive's root `Cargo.toml` therefore patches the same crates
from the same revision. Bump them together with the pin.

**Size**: the release web build went from 3.54 MB to 9.56 MB of wasm
(1.41 → 3.90 MB gzipped; the baseline is the live build of the same base
commit; `wasm-opt -Oz` saves nothing more). Most of it is hayro and vello_cpu
(the rasterizer), skrifa and harfrust (fonts), lopdf and craft-pdf, and the
fonts pdfcraft embeds (Inter, the Foxit base-14 faces, CMaps). The fix is to load
the PDF engine only when a PDF is opened: a second wasm module (a worker,
which also takes rendering off the main thread). Until then an embedder
that doesn't need PDFs can drop the `pdf` feature, and PDFs then show in
the browser's own viewer in an iframe.

**Speed**: rendering runs on the main thread. A photo-heavy page at
1100 CSS px on a 2× screen takes about 2 s in the release build, and the
page is blocked meanwhile. The worker above fixes this too.

## The other craft apps as engines

Surveyed read-only on 2026-11-21: soundcraft, photocraft, filmcraft,
vectorcraft, figcraft, wordcraft, gridcraft, deckcraft, designcraft and
lightcraft, plus storytold/craft-libs.

**What they share:**

- **Licence and toolchain:** each is `MIT OR Apache-2.0`, edition 2024,
  `rust-version` 1.95.
- **Layout:** each is laid out like pdfcraft: engine crates, an egui UI,
  and `x`, `x-cli` and `x-web` apps.
- **Where the engines are:** the format and codec engines are in
  **craft-libs** (about 230 `craft-*` crates). These are git-only and
  **untagged**: pin by rev (HEAD then: `6a061a30bdd4a6cc24329334d5e44241896edba6`).
- **One rev for all:** use one craft-libs rev for every craft dependency.
  Two revs build two copies of the shared types, which won't unify.
- **wasm:** each app checks its engine layers and web app for wasm32 in
  `cargo xtask ci`. craft-libs CI checks only some crates for wasm32:
  craft-audio, craft-media, craft-video, craft-jpeg, craft-pdf and
  craft-svg-paths.
- **MCP:** the apps ship MCP servers. These drive an editing session
  (run a command, inspect, screenshot, click), so they are of no use to a
  viewer.

| Drive kind | Recommendation |
|---|---|
| Audio waveform | **Now**: the browser decodes and `drive_view::peaks` draws a 600-column overview (WAV, MP3, AAC, FLAC, Ogg: whatever the browser plays). **Next**: `craft-audio` (craft-libs, `default-features = false, features = ["decode-native", "peaks"]`; add `decode` for FLAC and Vorbis through symphonia) **on the node or agent**: `craft_audio::decode(bytes, ext)`, then `peaks::Peaks::build(&channel)` (multi-resolution, O(log n) range queries) gives peaks for files too big to read in the browser (over 96 MB here), kept beside the blob and zoomable. Not adopted in the browser: it always pulls craft-media, and the browser already decodes off the main thread. |
| Image | The browser's `<img>`. For HEIC, TIFF, EXR, HDR, PSD and camera raw, and for thumbnails: `craft-image-codecs::decode(&[u8]) -> Image`, then `Image::to_rgba8()`, natively on the node (not wasm-checked in CI), plus `craft-psd` and `craft-raw`. |
| Video | The browser's `<video>` (with the node's ranges). For poster frames and thumbnails: `soundcraft-video` (soundcraft v0.20.1): `Movie::open(bytes)?.thumbnail_at(secs, max_h)` gives RGBA. It covers H.264, ProRes and MJPEG in MP4/MOV and is on soundcraft's wasm list. |
| Vector (SVG) | The browser's `<img>` (no scripts run there). For rasterizing on the server: `craft-svg` with `default-features = false, features = ["tree-render"]`. |
| DOCX, ODT, RTF | Not built. The way in: `craft_docx::read`, `craft_odf::text::import` or `craft_rtf::doc::import`, then `craft_html::doc::export_fragment`. Show the result sandboxed, as markdown is shown. For faithful pages later: wordcraft's layout and render (needs fonts and its patch table). |
| Spreadsheets | Not built. Read with `craft-xlsx`, `craft-ods`, `craft-xls` or `craft-csv` (or `gridcraft_xlsx::read_xlsx` for all four), format numbers with `gridcraft-numfmt::format_value`, and show the cached values (no recalculation). |
| Markdown | Done with pulldown-cmark; `craft-markdown` covers only part of CommonMark. |
| Any file | `craft-sniff::identify(&[u8])` (no dependencies) names a format from its first bytes. Use it to pick a viewer when an extension lies or is missing. |

## Left to do

- The PDF engine as its own lazily loaded module in a worker (size, and
  rendering off the main thread).
- The sign flow through the node and the agent (the seam above).
- ~~Waveform peaks made natively for big audio~~: done, as sidecars
  ([sidecars.md](sidecars.md); symphonia, streamed, not `craft-audio`).
- DOCX/ODT/RTF and spreadsheet viewers, and decoders for HEIC and TIFF (above).
- The comments UI on top of the anchors, being built separately.
- Quick edits for files on other computers (today, only files of the
  space's tree are saved; a file on a computer is read-only here).
- The PDF view renders one page at a time; continuous scrolling and
  thumbnails can follow once rendering is off the main thread.
