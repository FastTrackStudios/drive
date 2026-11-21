# Drive's base site theme

The look every Drive site starts from: `templates/` (gingembre, dodeca's
Jinja-like engine: `base.html`, `index.html`, `section.html`,
`page.html`; gingembre has no `include` yet, so the nav lives in
`base.html` and the section lists in `index.html` / `section.html`),
`sass/main.scss`, and `static/`. `drive-agent site theme-init` copies it
into the space (`Sites/Themes/Base`); a site's override folder replaces
any file here by having one at the same path.
