# Third-party notices

Russenger (the Windows client) is built on the work of others. This file lists the main
third-party components it distributes, links against or downloads, and their licenses. Each
project remains under its own license; see the linked sources for full terms.

## Source code (AGPL-3.0)

Russenger as a whole is distributed under the **GNU AGPL-3.0** (see `LICENSE`), because its helper
compiles in mautrix-meta. The complete corresponding source of this build (the Rust app in
`relay-win/`, the Go helper in `relay-helper/`, the modified library in `thirdparty/` and
`build.ps1`) is the project folder it was built from. Whoever distributes this program must give
the recipients that source (AGPL-3.0 §6) and, for a copy offered over a network, to its users
(§13).

## Helper (`relay-helper`, Go)

- **mautrix-meta** — `go.mau.fi/mautrix-meta` — **AGPL-3.0**. https://github.com/mautrix/meta
  Used to speak Meta's non-E2EE (Lightspeed) protocol. Two copies are kept in `thirdparty/`; the
  build uses `thirdparty/mautrix-meta-new`, **which has been modified** by this project: parsing of
  the `populateCommunityFolderData` operation was added (`pkg/messagix/table/folders.go` and
  `table.go`). The mautrix license and its exceptions are in `thirdparty/*/LICENSE` and
  `LICENSE.exceptions`.
- **whatsmeow** — `go.mau.fi/whatsmeow` — **MPL-2.0**. https://github.com/tulir/whatsmeow
  Used for the end-to-end encrypted channel.
- **libsignal (go.mau.fi/libsignal)**, **mautrix Go utilities (go.mau.fi/util)** (MPL-2.0),
  **beeper/argo-go**, **beeper/poly1305**, **beeper/req** — see upstream.
- **modernc.org/sqlite** — BSD-3-Clause-style; bundled SQLite is public domain.
- **rs/zerolog** (MIT), **google/uuid** (BSD-3-Clause), **gorilla/websocket** (BSD-2-Clause),
  **coder/websocket** (ISC), **tidwall/gjson·sjson·match·pretty** (MIT), **vektah/gqlparser** (MIT),
  **yuin/goldmark** (MIT) and other transitive Go modules — each under its own permissive license;
  see `relay-helper/go.sum` and the repositories.

## App (`relay-win`, Rust)

- **egui / eframe / egui_extras / epaint / emath** — MIT OR Apache-2.0. https://github.com/emilk/egui
- **wgpu** and its dependencies — MIT OR Apache-2.0. https://github.com/gfx-rs/wgpu
- **image** (and the png, jpeg, gif and webp decoders it uses) — MIT OR Apache-2.0.
- **rusqlite** — MIT; bundles **SQLite** (public domain).
- **wry**, **tao** — Apache-2.0 OR MIT. https://github.com/tauri-apps/wry
- **webview2-com**, **windows**, **windows-sys** (Microsoft) — MIT OR Apache-2.0.
- **tray-icon** (MIT OR Apache-2.0, Tauri), **arboard** (MIT OR Apache-2.0), **raw-window-handle** (MIT OR Apache-2.0 OR Zlib).
- **rfd** (MIT), **ureq** (MIT OR Apache-2.0), **chrono** (MIT OR Apache-2.0),
  **serde_json** (MIT OR Apache-2.0), **unicode-segmentation** (MIT OR Apache-2.0),
  **winresource** (MIT) and other transitive crates — see `relay-win/Cargo.lock` and each crate's
  repository for the exact terms.

## Run-time downloads and system components

- **Twemoji** — about 190 emoji pictures (emoji picker and reactions, `assets/emoji`) are compiled into the program; any other emoji is fetched once from the jsDelivr CDN and saved on the user's computer.
  Graphics © Twitter, Inc and other contributors, licensed **CC-BY 4.0**
  (https://creativecommons.org/licenses/by/4.0/); code under MIT.
  https://github.com/jdecked/twemoji
- **Microsoft Edge WebView2 Runtime** — used for the sign-in, call and player windows. Not
  distributed with Russenger; provided by Windows. `WebView2Loader.dll` (Microsoft) is shipped
  next to the program and is subject to Microsoft's license terms.
- **Segoe UI** fonts are read from the user's Windows installation; they are not distributed.

## Services

- **GIPHY** — GIF sending (when used) is subject to the GIPHY API Terms of Service
  (https://developers.giphy.com/).

---

Russenger is an independent project and is **not affiliated with, authorized, or endorsed by
Meta Platforms, Inc., Facebook, Messenger, or WhatsApp**. All trademarks are the property of
their respective owners.
