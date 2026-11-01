# Activity

Application source: apps/activity. Owned domain crates: none.
Keep all existing runtime integration resources and public protocols.

Commands: `cargo build --release --locked --manifest-path Cargo.toml`; `./install.sh`

Use the public, pinned shared crates; never import another application's internals.
Keep application names, IDs, paths, D-Bus interfaces, and data formats stable.
Use canonical STYLE.md from the pinned nyx-design dependency. Reuse existing
widgets, theme tokens, icons, and shortcuts. Media Player defines video controls.
Run essential targeted checks only. Do not download models or run inference for
routine verification. Install app updates without opening/restarting them. Restarting background services after installation is allowed.
Common guidance: https://github.com/design-nexus/nyx/blob/main/docs/AGENTS_COMMON.md
This file carries the essential common rules so offline work remains possible.

Shared dependencies use immutable GitHub revisions. Coordinate public API changes
with the suite manifest and affected consumers in design-nexus/nyx.
Historical application tags preserve provenance; standalone builds use main
or subsequent releases. Original release assets remain in the umbrella repo.
