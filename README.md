<div align="center">

# Asmoday

**Personal Media & Device Control Hub · Ultra-lightweight Single Binary Panel**

[![Build](https://img.shields.io/badge/build-passing-brightgreen?style=flat-square)](#)
[![Version](https://img.shields.io/badge/version-v3.15.0-blue?style=flat-square)](#)
[![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey?style=flat-square)](#)
[![License](https://img.shields.io/badge/license-MIT-green?style=flat-square)](#)

*Rust(Axum) + Vue 3(Vite) + TailwindCSS v4*

**[English](README.md) | [简体中文](README.zh.md)**

</div>

---

> **Why Asmoday?**  
> Say goodbye to the clunky combination of `FileBrowser + ttyd + Nginx`. We use an extremely restrained design to perfectly integrate **file management**, **native terminal**, **media gallery**, and **external service aggregation** into a **~6MB** cross-platform single binary.

## ✨ Core Features

- 🪶 **Ultra-lightweight**: Single binary (with embedded UI), ~6MB in size, extremely low idle memory footprint, perfect for Raspberry Pi and low-power ARM devices.
- 🖥️ **Native PTY Terminal**: Real web terminal with multi-tab support, disconnect survival, Termux-style mobile keyboard, and "Open Terminal Here" context menu in file manager.
- 🖼️ **Immersive Media Preview**: Masonry gallery, image/audio/video preview, external subtitles, global music player (with LRC lyrics and volume memory).
- 📂 **Modern File Management**: No sidebar tree. Multi-mount cards + breadcrumb navigation. Built-in CodeMirror 6 editor with EOL and encoding safety.
- 🔒 **Public Network Security**: 2FA, independent WebDAV Tokens, rate limiting, CSRF protection, reverse proxy, and SSO support.
- 🎨 **Premium UI/UX**: Glassmorphism UI based on TailwindCSS v4, seamless light/dark theme switching, i18n support.

> **📸 Screenshots**  
> *(Replace with your actual screenshots)*  
> `<img src=".shots/home.png" width="800" alt="Home">`

## 🚀 Quick Start

### Method 1: Download Binary (Recommended)
Go to [Releases](https://github.com/Asmoday-lab/asmoday/releases), download the archive for your OS, extract and run:

```bash
# Linux / macOS
chmod +x asmoday
./asmoday

# Windows
asmoday.exe
```
Access `http://127.0.0.1:4242`. The first visit will guide you to set up an admin account.

### Method 2: Docker
```bash
docker run -d \
  -p 4242:4242 \
  -v /path/to/data:/app/data \
  -v /path/to/media:/mnt/media \
  --name asmoday \
  asmoday/asmoday:latest
```

### CLI Commands
```powershell
# Scripted deploy (prints one-time password)
asmoday --init

# Forgot password: reset from terminal (no backdoor in config)
asmoday --reset-password

# Custom config & data directory
asmoday -c ./config.toml -d ./data -p 4242

# Hot backup the database (works while the server is running; VACUUM INTO keeps it consistent)
asmoday --backup ./asmoday-backup.db
```

## 🏗️ Development

Requirements: Rust (MSVC toolchain), Node.js (v20+), pnpm.

```bash
# 1. Build frontend
cd web
pnpm install
pnpm run build

# 2. Build backend (auto-embeds frontend)
cd ..
cargo build --release

# 3. Dev mode (frontend HMR)
cd web && pnpm run dev  # http://localhost:5173
```

**Cross-compilation** (Windows -> Linux ARM64):
```bash
cargo zigbuild --release --target aarch64-unknown-linux-musl
```

## 🛡️ Security & Boundaries

1. **Zero External CDN**: All assets, fonts, and icons are embedded. No supply chain attacks.
2. **TOML Only**: Config is TOML with comments preserved.
3. **No Backdoor**: Auth cannot be disabled structurally. Reset only via CLI.
4. **Path Traversal Protection**: Whitelist + `..` rejection + `canonicalize` check.
5. **File Op Limits**: copy/move/delete > 100 items or > 2 GiB are rejected; cross-mount moves use streaming to avoid corruption.

## 📈 Star History

If this project helps you, please give it a ⭐️ Star!

[![Star History Chart](https://api.star-history.com/svg?repos=Asmoday-lab/asmoday&type=Date)](https://star-history.com/#Asmoday-lab/asmoday&Date)

## 📚 Appendix

<details>
<summary><b>API Overview</b></summary>

| Method | Path | Description |
| --- | --- | --- |
| GET | `/api/files/mounts` | List mounts (readonly/offline status) |
| GET | `/api/files/list` | List dir (sort, cursor, hidden filter) |
| POST | `/api/files/mkdir` | Create dir (no silent overwrite) |
| POST | `/api/files/move` `/copy` | Batch move/copy (3 conflict strategies) |
| POST | `/api/files/delete` | Batch delete (requires confirm_name/token) |
| GET/PUT | `/api/files/raw` | Stream read/write (Range & ETag) |
| GET | `/api/media/browse` | Browse media (ThumbHash & subtitle probe) |
| WS | `/api/terminal/ws` | Terminal WebSocket (PTY stream) |
| GET/POST | `/api/config` | Read/write config |
| GET/POST | `/api/auth/totp` | 2FA setup |
| any | `/proxy/{id}/*` | Reverse proxy |
| any | `/dav/<mount_id>/*` | WebDAV server (Token required) |
| GET | `/api/system/backup` | Download a hot DB backup (requires a recent password check) |

</details>

---

<div align="center">
  <sub>Built with ❤️ by Asmoday-lab</sub>
</div>