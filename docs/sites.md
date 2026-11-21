# Sites

Static sites published from the space's files with
[dodeca](https://github.com/bearcove/dodeca) (`ddc`: incremental builds,
gingembre templates, built-in search, link checking, image optimisation).
Written 2026-11-21.

## The pieces

| Piece | What it is |
|---|---|
| **Site** | A Loro document, `<space>/drive:site:<id>` (`drive_files::sites`): title, the folder of Markdown pages, the theme folder, an override folder, the public storage it goes to (or none: the node only), its base URL, the device that builds it, whether it rebuilds by itself, and its last build (time, ok, pages, broken links, unresolved wikilinks, input hash, where it went). The registry lists sites (`sites`: id → title). |
| **Output** | `<space>/drive:siteout:<id>`: every built file's path → blob, as the node serves it, and what each public storage was last sent. |
| **Source** | A folder of the file tree (`drive_sites::source::TreeFolder`). `Source` is a trait: Loro page documents can be a second source (their text is `Content::Text`). |
| **Theme** | A folder with `templates/`, `sass/`, `static/` (the starter is `assets/site-theme`, copied into `Sites/Themes/Base` by `drive-agent site theme-init`). The override folder lays files of the same paths on top. No theme folder: the built-in starter. |
| **Generator** | `crates/drive-sites`: converts the pages (Obsidian frontmatter and `[[wikilinks]]` included), writes a dodeca project on the building computer (only changed files), runs `ddc build`, places the output under its path, publishes it. |

## Who builds

The agent, not the node: it has the files, it holds the public storage's
keys (they never leave the device that runs the storage), and `ddc` is
installed for its user (`~/.local/bin/ddc`, or `$DRIVE_DDC`). The node
only serves what an agent uploaded: `/sites/<space>/<id>/…` reads the
output document and sends the blob (hashed assets cached for good, pages
`no-cache`), readable without signing in, like share links.

- `drive-agent site build ID [--force]` builds now.
- `drive-agent serve` (the service) rebuilds a site four seconds after its
  pages, theme or settings change, when a site is added, when the app asks
  (`requested` later than the last build), and checks every minute. A site
  is built by its `builder` device; one with none is claimed by the first
  agent that sees it.
- The same inputs are never built twice: the input hash covers the
  generator, `ddc`'s version, the settings and every input file's hash.

## Obsidian → dodeca

- Folders become sections; a folder note (`index.md`, `README.md`, the
  note named like its folder, `Home.md` at the top) is the section page.
- Names become slugs (`Circle of Fifths.md` → `/basics/circle-of-fifths/`).
- YAML frontmatter → TOML: `title`, `weight` (or `order`) at the top, the
  rest under `[extra]`, which always has `description` and `tags`
  (gingembre refuses missing fields). No title: the first `# Heading`
  (then dropped from the body), or the file name. `publish: false` or
  `draft: true` leaves a note out.
- `[[Page]]`, `[[Page|label]]`, `[[Page#Heading]]`, `[[folder/Page]]`,
  aliases, `![[image.png]]` (and `|300` sizes) and relative Markdown links
  to notes or files resolve to routes; what resolves to nothing stays as
  text (`span.missing-link`) and is reported in the build.
- Callouts (`> [!tip] Title`) become a quote with a bold title.
- Code blocks and spans are left alone.

## Paths

dodeca writes root-absolute URLs. A Drive site lives under a prefix, so
the output is rewritten per target (`drive_sites::rewrite`): HTML and CSS
URLs get the prefix, and a small script places what the search widget
fetches and links at run time. For a public bucket, page links also name
`index.html` (static buckets do not resolve folders). Public copies go to
`<storage root>/sites/<id>/`, at `<public URL>/sites/<id>/` unless the site
sets its own base URL.

## The app

Sites (under Space): every site, its last build, theme picker, Build now,
Open (the node's copy) and the public link; New site from a folder, a
theme and where it publishes.

## Next

- Loro Markdown page documents as a source (`Source` for them).
- `ddc`'s `include` is not implemented yet (0.17): the nav and lists are
  inline in the templates.
- `robots.txt` still gets a hashed name; `stable_assets` needs checking.
- A site's own domain (a bucket or worker per site), and removing a
  published copy when a site is deleted.
- Site access: sites are public on the node (no sign-in); a private
  (tailnet-only, signed-in) site would need the node to check a session
  cookie.
