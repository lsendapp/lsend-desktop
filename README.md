# LSend

Lightweight [LocalSend](https://localsend.org)-compatible desktop client that lives in your menu bar. No main window, no Dock icon -- just a tray accessory for sending files and text to nearby devices, and an always-on receiver.

Currently macOS only (universal arm64 + x86_64).

## Features

- **Menu-bar resident** -- `LSUIElement` accessory: no Dock icon, no main window. Status, send, and receive all from the tray.
- **Always-on receiver** -- starts on launch, auto-accepts incoming files (saved to `~/Downloads`), can be paused/resumed from the tray.
- **Send from the tray** -- "Send file to..." opens a native file picker; "Send text to..." sends the clipboard or typed text.
- **Live device scan** -- polls for LocalSend peers on the LAN; each discovered device gets its own send submenu entry.
- **Transfer animations** -- the tray icon animates during send / receive so you can see activity at a glance.
- **Native notifications** -- macOS Notification Center alerts on completed transfers and errors.
- **Shares identity with `lsend` CLI** -- reads the same `~/.config/lsend/` config, so the desktop app and CLI use one TLS identity and alias.

## Requirements

- macOS 11.0+
- Rust 1.75+ (for building from source)

> The desktop crate depends on `lsend-cli` (`lsend_core`) for the LocalSend protocol implementation. In the monorepo layout it lives at `../cli`; the standalone `lsendapp/lsend-desktop` repo's CI checks out `lsendapp/cli` as a sibling.

## Download

Pre-built universal DMGs are published on the [Releases](../../releases) page:

- `LSend-<version>-universal.dmg` -- signed with a Developer ID Application certificate and notarized by Apple. Opens without Gatekeeper warnings.

Verify a download against `SHA256SUMS`:

```bash
shasum -a 256 LSend-*-universal.dmg
# compare against the matching line in SHA256SUMS
```

## Build from source

```bash
git clone https://github.com/lsendapp/lsend-desktop.git
cd lsend-desktop

# The Cargo.toml references ../cli for the lsend-core protocol crate.
# Clone the CLI repo as a sibling:
git clone https://github.com/lsendapp/cli.git ../cli

# Build a release .app bundle (universal arm64 + x86_64):
scripts/make-app-bundle.sh universal
# -> dist/LSend.app

# Or open it immediately:
scripts/make-app-bundle.sh run
```

Other build profiles:

```bash
scripts/make-app-bundle.sh              # release, host arch only
scripts/make-app-bundle.sh debug        # debug build
scripts/make-app-bundle.sh universal    # universal (arm64 + x86_64) release
```

## Usage

Launch `LSend.app`. A tray icon appears in the menu bar showing your device alias.

**Tray menu:**

| Item | Description |
|------|-------------|
| Status line | Receiver state + alias (e.g. "Receiving - My Mac - port 53317") |
| Send file to... | Per-device submenu; opens a file picker for the chosen peer |
| Send text to... | Per-device submenu; sends the clipboard text to the chosen peer |
| Refresh devices | Force a rescan for nearby LocalSend peers |
| Pause / Resume receiving | Stop or restart the resident receiver |
| Quit LSend | Exit the app |

**Receiving:** Files are auto-accepted and saved to `~/Downloads`. A system notification fires on each completed transfer. The tray icon animates during receive.

**Sending:** Pick a peer from "Send file to..." or "Send text to...", choose a file (or paste text), and the transfer runs in the background. The tray icon animates during send.

## Configuration

LSend desktop shares its identity and alias with the `lsend` CLI, stored under:

- macOS/Linux: `~/.config/lsend/`
- Windows: `%APPDATA%\lsend\`

Files:

- `cert.pem`, `key.pem`, `fingerprint.txt` -- TLS identity (auto-generated on first run)
- `alias.txt` -- device display name seen by LocalSend peers

To change the alias, use the CLI:

```bash
lsend alias set "My Mac"
```

Then restart LSend desktop for the new alias to take effect.

## License

`lsend-desktop` is licensed under the [Apache License 2.0](LICENSE).

The LocalSend protocol implementation is provided by the [`lsend-cli`](https://github.com/lsendapp/cli) `core` crate, which vendors [`localsend-core`](https://github.com/localsend/localsend) -- also Apache 2.0, Copyright 2022-2024 Tien Do Nam.
