# August agent

An always-on personal agent: [August's core](https://github.com/daz9e/august-core) with its
default extensions (tools, memory, skills, scheduled tasks, Telegram, a terminal chat, model
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
august serve                    # run in the background (systemd / launchd)
loginctl enable-linger $USER    # on a server: keep it running after you log out
august                          # chat in this terminal
```

In the chat, `/login` signs in to a model provider and `/login telegram` connects a bot.
`august logs`, `august restart` and `august stop` manage the service. Update by running the
installer again, then `august restart`.

## Develop

```sh
cargo build
cargo test
```

## Versions

A version of august-agent pins one version of the core: the tag of `august-ext` in
`Cargo.toml` (and `extensions/llm/Cargo.toml`). Installing a version brings that core with it,
so the two always match.

- New core: tag august-core (`v0.2.0`), point `august-ext` at that tag here, then
  `cargo update -p august-ext`.
- Release: bump `version` in `Cargo.toml`, tag the same (`v0.3.0`) and push the tag. CI builds
  the extensions and the pinned core, one archive per target, with a `VERSIONS` file.
