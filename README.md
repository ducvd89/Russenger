# Russenger

An unofficial **Facebook Messenger client for Windows and macOS**, written in Rust ([egui](https://github.com/emilk/egui))
with a Go helper that speaks Meta's chat protocols. It started as a Windows port of the macOS client *Relay*.

> Not affiliated with, authorized or endorsed by Meta Platforms, Inc., Facebook, Messenger or WhatsApp.
> Using an unofficial client may break Meta's terms of service; you use it at your own risk.

## Features

- Telegram-style layout: chat list, tabs (chats, marketplace, requests, archive, communities), dark and light themes
- Text, replies, edits, unsend, reactions, colour emoji (Twemoji) and an emoji picker
- Pictures, link cards, stickers, video and audio playback, picture viewer with zoom
- End-to-end encrypted chats (restore older messages with your Secure Storage PIN)
- Voice and video calls through Facebook's web client in a WebView2 window
- Toast notifications that are grouped (one summary toast when many messages arrive)
- Message search, drag-and-drop and paste of pictures, "seen" marks
- Picture/video cache with a size limit you set in Settings (least recently used files are removed)
- Tray icon: closing the window keeps Russenger running in the background

## How it is built

| Part | Folder | Language |
| --- | --- | --- |
| App (UI, storage, notifications, sign-in/call/player windows) | `relay-win/` | Rust |
| Helper (Messenger protocols, JSON lines over stdio) | `relay-helper/` | Go |
| Modified mautrix-meta library | `thirdparty/mautrix-meta-new/` | Go |

## Build

### Windows

Needs Windows 10/11, Rust (GNU toolchain with MinGW on `PATH`), Go and the
[WebView2 runtime](https://developer.microsoft.com/microsoft-edge/webview2/).

```powershell
.\build.ps1
```

The result is in `dist\Russenger\` (run `russenger.exe`).

### macOS

Install Rust 1.89 or later, Go 1.26.4 or later, and the Xcode Command Line Tools.
Run `./build-macos.sh` and copy `dist/Russenger.app` to Applications. The script
builds for the host architecture and signs the app for local use.

Facebook sign-in uses WKWebView and stores the session in Keychain. Conversation
data is stored under `~/Library/Application Support/Russenger`. Voice and video
call buttons open the conversation in the default browser, where you can start
the call with your Facebook browser session.

## Signing in

Russenger signs in with your Facebook session cookies, taken from a sign-in window it opens itself.
On Windows, cookies are stored encrypted with DPAPI under `%LOCALAPPDATA%\Relay`. Messages are kept in a local SQLite database
in the same folder. Nothing is sent anywhere except to Meta's servers (and the Twemoji CDN for emoji pictures).

## License

**AGPL-3.0** (see `LICENSE`), because the helper builds on [mautrix-meta](https://github.com/mautrix/meta).
Third-party components and their licenses are listed in `THIRD-PARTY-NOTICES.md`.
