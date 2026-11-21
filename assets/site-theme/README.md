# Drive's base site theme

The look every Drive site starts from: `templates/` (gingembre, dodeca's
Jinja-like engine: `base.html`, `index.html`, `section.html`,
`page.html`; gingembre has no `include` yet, so the nav lives in
`base.html` and the section lists in `index.html` / `section.html`),
`sass/main.scss`, and `static/`. `drive-agent site theme-init` copies it
into the space (`Sites/Themes/Base`); a site's override folder replaces
any file here by having one at the same path.

gingembre refuses a field that is not there, so Drive's converter gives
every page and section `extra.description` and `extra.tags` (empty when a
note has none): test those for truthiness (`{% if page.extra.tags %}`);
other `extra` fields exist only on the notes that set them.
