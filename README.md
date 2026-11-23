# Drive

One local-first system for everything a person or a business keeps:
tasks, projects, the calendar, people, mail, files and media, and the
data of every app built on it.

Apps are separate, focused products (Studio, Mail, Meals, …) that embed
Drive's client; none deploys a server. Every device holds its data and
works offline; devices sync with each other directly (Loro over iroh);
servers (a hosted cloud, your own nodes, your own storage) make it easier
but are never required. Anyone can be invited by link, live, with nothing
installed.

- [docs/design.md](docs/design.md): the design.
- [PLAN.md](PLAN.md): the prototype plan.
- [docs/apps/meals.md](docs/apps/meals.md): the Meals app.
- [docs/sites.md](docs/sites.md): static sites from folders of Markdown (dodeca).
- [docs/tags.md](docs/tags.md): tags, fields and tag search.
- [docs/rules.md](docs/rules.md): file rules, synced and run by every device (dry run first; nothing deleted).
- [docs/sidecars.md](docs/sidecars.md): thumbnails and waveforms, derived once per file content.
- [docs/mount.md](docs/mount.md): one folder for everything: the Drive tree, and every device's locations and disks under `Devices/`.
- [docs/browse.md](docs/browse.md): every computer's disks, browsed live and read-only.
- [docs/transfer.md](docs/transfer.md): file bytes between devices over iroh-blobs, by range, in place.
- [docs/app-files.md](docs/app-files.md): any Drive file as a real local path at disk speed, for apps (an audio engine), wherever it lives.
- [docs/kache.md](docs/kache.md): Rust build caches (kache) shared peer to peer between the owner's computers.
- [docs/agents.md](docs/agents.md): Drive as an MCP server, so AI agents can see and organise files safely.
- [PROTOTYPE.md](PROTOTYPE.md): the running prototype (Drive and four apps, live).

Status: a working prototype (see PROTOTYPE.md); the plan is the way to the
real thing. The first apps on it: Studio (`../studio-proto`), Meals
(`../meals`) and Frame (`../frame-app`).

Licence: to be decided.
