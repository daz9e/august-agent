# August agent

An always-on personal agent: [August's core](https://github.com/daz9e/august-core) with its
default extensions (tools, memory, skills, scheduled tasks, Telegram, Discord, a terminal chat, model
providers, ...).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/daz9e/august-agent/main/scripts/install.sh | sh
```

Linux (x86_64, arm64) and macOS. It puts `august` and its extensions into `~/.local/bin`
(`AUGUST_INSTALL_DIR` to change). `sh -s -- --ref v0.1.0` picks a version; `--source`
builds with cargo instead.

Then:

```sh
august setup                    # run in the background and set it up step by step
august serve                    # (or just) run in the background (systemd / launchd)
august                          # chat in this terminal
```

In the chat, `/login` signs in to a model provider and `/login telegram` or `/login discord` connects a bot.
`august logs`, `august restart` and `august stop` manage the service. Update with `/update`
in the chat (also the extensions installed from git), or by running the installer again: it
asks before updating and offers to restart August.

`august uninstall` removes August: it offers a backup (`~/august-backup-<time>.tar.gz`), asks for
a typed `yes`, then deletes the data and the binaries.

## Develop

```sh
cargo test
./august setup      # `august` from this checkout: builds it, keeps its data in .dev/
./august reset      # wipe that data; ./august clean also drops the builds
```

`./august` rebuilds the extensions on every run and the core when `Cargo.lock` pins another
commit of it, and runs its own background service, apart from an installed August.

## Versions

A version of august-agent pins one version of the core: the tag of `august-ext` in
`Cargo.toml` (and `extensions/llm/Cargo.toml`). Installing a version brings that core with it,
so the two always match.

- New core: tag august-core (`v0.2.0`), point `august-ext` at that tag here, then
  `cargo update -p august-ext`.
- Release: bump `version` in `Cargo.toml`, tag the same (`v0.3.0`) and push the tag. CI builds
  the extensions and the pinned core, one archive per target, with a `VERSIONS` file.
