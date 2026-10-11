# August agent

An always-on personal agent: [August's core](https://github.com/daz9e/august-core) with its
default extensions (tools, memory, skills, scheduled tasks, Telegram, a terminal chat, model
providers, ...).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/daz9e/august-agent/main/scripts/install.sh | sh
```

Linux (x86_64, arm64) and macOS. It puts `august` and its extensions into `~/.local/bin`
(`AUGUST_INSTALL_DIR` to change). `sh -s -- --ref v0.1.0` picks a release; `--source`
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

A release is a `v*` tag: CI builds the core at the commit `Cargo.lock` pins and these
extensions, one archive per target.
