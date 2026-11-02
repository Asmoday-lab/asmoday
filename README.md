<div align="center">

# Asmoday

**Self-hosted file manager, web terminal and media library**

[![Release](https://img.shields.io/github/v/release/Asmoday-lab/asmoday?include_prereleases&style=flat-square)](https://github.com/Asmoday-lab/asmoday/releases)
[![License](https://img.shields.io/badge/license-MIT-green?style=flat-square)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.89%2B-orange?style=flat-square)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64%20%7C%20Linux%20x64%2Farm64-lightgrey?style=flat-square)](#quick-start)

[Features](#features) · [Quick start](#quick-start) · [Configuration](#configuration) · [CLI](#cli) · [FAQ](#faq) · [Development](#development) · [简体中文](README.zh.md)

</div>

---

Asmoday is a lightweight self-hosted panel that bundles **file management**, a **web terminal**, **photo / video / music libraries**, **share links** and **WebDAV** into a single executable. The backend is Rust (axum), the frontend is Vue 3, and all frontend assets are embedded into the binary at build time — at runtime it only needs one SQLite file.

It is built for the "own machine + browser" setup: a NAS, a Raspberry Pi, a mini PC, an old laptop, or the Windows desktop you already use. No Node runtime, no database server, no external CDN.

## Features

- 📂 **File management** — multiple mounts (individually read-only), grid and list views, drag-and-drop upload, batch copy/move/delete, whole-folder ZIP download, recursive search.
- ✏️ **Built-in editor** — CodeMirror 6 with syntax highlighting, Markdown preview, read-only CSV and PDF previews; line endings (LF / CRLF) and encoding are written back unchanged.
- 🖥️ **Web terminal** — a real PTY via `portable-pty`, multiple tabs, sessions that survive disconnects, an on-screen key bar on phones, and "Open terminal here" from the file manager.
- 🖼️ **Photo library** — albums and timeline, virtual scrolling, ThumbHash placeholders, slideshow and favorites.
- 🎬 **Cinema library** — folder cards, resume from the last position, external subtitles (`.srt` converted to WebVTT on the fly, or `.vtt`), Video.js with a native-player switch.
- 🎵 **Music library** — ID3v2 / FLAC / MP4 / Ogg tags and LRC lyrics, a persistent bottom player bar, list loop / single loop / shuffle.
- 🔗 **Share links** — unguessable links for files or folders, with password, expiry date, download toggle and Referer allowlist.
- 🗄️ **WebDAV** — the full method set with real locks; tokens can be scoped to one mount. Works with phone sync apps and system file managers.
- 🔒 **Security** — Argon2id hashing, TOTP two-factor auth, sliding sessions, three independent rate-limit pools (login / share / WebDAV), path traversal protection and hardened response headers.
- 🌗 **Interface** — Tailwind CSS v4 glass surfaces, dark and light themes, Chinese and English, responsive down to phones.
- 🧩 **Service hub** — `[[services]]` entries put other self-hosted apps on the home page through the built-in reverse proxy, with optional header injection.
- 🪶 **Lightweight** — one executable of roughly 10 MB including the UI, no runtime dependencies, happy to stay running.

## Quick start

### Download and run

Grab the archive for your platform from [Releases](https://github.com/Asmoday-lab/asmoday/releases):

| Platform | Artifact |
| --- | --- |
| Windows x64 | `asmoday-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `asmoday-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `asmoday-aarch64-unknown-linux-musl.tar.gz` |

```bash
# Linux / macOS
tar -xzf asmoday-*.tar.gz && chmod +x asmoday
./asmoday

# Windows: extract and double-click asmoday.exe, or run it from a shell
asmoday.exe
```

Open `http://127.0.0.1:4242`; the first visit walks you through creating the admin account.

The first start writes `config.toml` and `data/` into the current directory. With no mount configured, the working directory is used as a fallback root; to manage other disks, add a mount under Settings → Advanced or edit the config:

```toml
[[system.mounts]]
name = "Media"
path = "D:/Media"        # on Linux, e.g. /mnt/media
readonly = false
```

> macOS has no prebuilt package — build from source (see [Development](#development)).

### Build from source

Requires Rust 1.89+, Node.js 20+ and pnpm:

```bash
cd web && pnpm install && pnpm run build && cd ..
cargo build --release          # output: target/release/asmoday
```

### Run under systemd (Linux)

```ini
[Unit]
Description=Asmoday
After=network.target

[Service]
User=asmoday
WorkingDirectory=/opt/asmoday
ExecStart=/opt/asmoday/asmoday -c /opt/asmoday/config.toml -d /opt/asmoday/data
Restart=on-failure
RestartSec=3

[Install]
WantedBy=multi-user.target
```

### Behind a reverse proxy (optional)

```caddyfile
asmoday.example.com {
    reverse_proxy 127.0.0.1:4242
}
```

When running behind HTTPS, set `[server].host` to `127.0.0.1` and enable `[server.proxy].trust_proxy_headers` with a `trusted_ips` list as needed. For a sub-path deployment (e.g. `https://example.com/asmoday`) set `base_url`. Asmoday does not implement ACME — use your own certificate or let Caddy / Nginx handle it.

## Configuration

Configuration is TOML. Resolution order: `-c` flag → `ASMODAY_CONFIG_PATH` → `config.toml` in the working directory; the data directory is `-d` flag → `ASMODAY_DATA_DIR` → `data/` in the working directory. The settings page offers a form view and a raw source view; saving keeps comments and existing ordering.

| Section | Purpose |
| --- | --- |
| `[server]` | Listen address and port, TLS, proxy compatibility, batch limits |
| `[auth]` | Local login switch, proxy trust (SSO) |
| `[system]` | Mounts (`[[system.mounts]]`), symlink policy |
| `[media]` | Thumbnail cache size and age, FFmpeg path, upload size limit |
| `[terminal]` | Custom shell and args, idle keep-alive, concurrent sessions |
| `[log]` | Console and file levels, access log, text or JSON output |
| `[[services]]` | External services on the home page and their proxy options |

```toml
[server]
host = "0.0.0.0"
port = 4242

[media]
ffmpeg_path = ""            # empty = detect from PATH; used only for video first frames
thumb_cache_max_size_mb = 1024

[[system.mounts]]
name = "Media"
path = "/mnt/media"
readonly = false

[[services]]
id = "grafana"
name = "Monitoring"
icon = "chart"
target_url = "http://192.168.1.50:3000"
strip_prefix = true
```

Environment variables (take precedence over the config file):

| Variable | Purpose |
| --- | --- |
| `ASMODAY_CONFIG_PATH` | Config file path |
| `ASMODAY_DATA_DIR` | Data directory |
| `ASMODAY_HOST` / `ASMODAY_PORT` | Listen address and port |
| `ASMODAY_FFMPEG_PATH` | FFmpeg path |
| `ASMODAY_RESET_PASSWORD` | Non-interactive reset with `--reset-password` (remove it afterwards) |

## CLI

| Command | Description |
| --- | --- |
| `asmoday --init` | Write a default config and data directory, print a one-time admin password |
| `asmoday --reset-password` | Reset the admin password, print a one-time new one |
| `asmoday --reset-2fa` | Clear the TOTP secret and disable 2FA (requires the current password) |
| `asmoday --check-config` | Validate the config and exit without starting the server |
| `asmoday --backup ./asmoday-backup.db` | Write a consistent database copy and exit (works while running) |
| `asmoday -c ./config.toml -d ./data -p 4242 -H 0.0.0.0` | Custom config, data directory, port and address |

## WebDAV

Add the server in your file manager, Cyberduck, rclone or a phone sync app:

- URL: `http://<host>:4242/dav/` (`/dav/` lists the mounts visible to the credentials)
- Auth: a WebDAV token (scoped to one mount, optionally read-only) or the admin account and login password
- Methods: `OPTIONS`, `PROPFIND`, `GET`, `HEAD`, `PUT`, `DELETE`, `MKCOL`, `MOVE`, `COPY`, `LOCK`, `UNLOCK`

## FAQ

**I forgot my password.**
Run `asmoday --reset-password` on the server — there is no passwordless backdoor in the config. Lost your TOTP device? `asmoday --reset-2fa`.

**Other devices on my LAN cannot connect.**
Make sure `[server].host` is `0.0.0.0` and the firewall allows the port. If you use a reverse proxy, narrow it back to `127.0.0.1`.

**The port is already in use.**
Override it with `asmoday -p 8080`, or change `[server].port`.

**A video will not play.**
Playback is decoded by the browser; there is no transcoding. Whether HEVC / AV1 works depends on the browser and the system decoders.

**Thumbnails stay as placeholders.**
Video covers need an external FFmpeg — set `ffmpeg_path` or put `ffmpeg` on `PATH`. Image thumbnails do not need it.

**Copy or move says it exceeds the limit.**
The default cap is 100 selected items and 2 GiB per operation, adjustable under `[server.limits]`. Larger jobs belong in the terminal with `cp` / `mv`.

**Where is my data, and how do I back it up?**
Everything lives in the data directory: `data/asmoday.db` (users, sessions, shares, index), `data/logs/`, `data/thumbnails/`. Use `asmoday --backup` or the settings page for a consistent copy while running.

**Multiple users? Can I undo a delete?**
No multi-user support: one admin account per instance. Deletes are physical and there is no recycle bin. Deleted shares keep a 30-day tombstone so old links return `410`.

## Tech stack

| Layer | Technology |
| --- | --- |
| Backend | Rust 2024, axum 0.8, tokio, rusqlite (bundled, WAL) |
| Terminal | portable-pty + xterm.js (ConPTY mitigation, keep-alive) |
| Media | External FFmpeg (frame grabs only), lofty (audio tags), Video.js |
| WebDAV | dav-server (protocol) + custom auth and boundary checks |
| Frontend | Vue 3.5, Vite, Tailwind CSS v4, CodeMirror 6, vue-i18n |
| Packaging | rust-embed for the UI; GitHub Actions builds three targets |

## Development

```bash
# Frontend (hot reload)
cd web && pnpm install && pnpm run dev     # http://localhost:5173

# Backend
cargo run                                   # http://127.0.0.1:4242

# Release build (embeds web/dist)
cd web && pnpm run build && cd .. && cargo build --release
```

Before submitting, run the same gates CI uses:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cd web && pnpm exec vue-tsc -b && pnpm run build
```

Layout:

```
src/            Rust backend (api/ routes and handlers, core/ config, auth, filesystem, logging)
web/            Vue 3 frontend (views/, components/, locales/)
Design/         product and system design notes
.github/        release workflow (triggered by v* tags)
```

## Contributing

Issues and pull requests are welcome. Before you open one:

1. Discuss larger changes in an issue first, so nobody wastes work;
2. Pass the gates above and add tests for new behaviour;
3. Keep UI strings in `vue-i18n` — no hardcoded Chinese or English in components;
4. Use `type(scope): summary` commit messages, e.g. `fix(player): correct bar height`.

## License

[MIT](LICENSE) © Asmoday-lab

## Acknowledgements

Thanks to axum, rusqlite, portable-pty, dav-server, lofty, Vue, Vite, Tailwind CSS, CodeMirror, xterm.js and Video.js.

<div align="center">
  <sub>If this project helps you, a ⭐️ is appreciated.</sub>
</div>
