# Hexy Endpoint

A small tray app for Windows, macOS and Linux that keeps the shares on a
[HexOS](https://hexos.com) server mounted on your computer.

Give it your server's address and a HexOS username and password. It then:

- mounts every share that account can see (the default), or only the ones you tick;
- starts when you log in and stays in the system tray;
- asks the server for its share list once a minute, mounting shares that were
  added and unmounting ones that were removed;
- updates itself from this repository's GitHub Releases.

It only ever unmounts shares it mounted itself, so anything you mounted by hand
is left alone.

## How it works

| | Share discovery | Mounting | Where shares appear |
|---|---|---|---|
| macOS | SMB (built in) | NetFS framework | `/Volumes/<share>` |
| Windows | SMB (built in) | Windows networking API | Drive letters, from `Z:` downwards |
| Linux | SMB (built in) | `gio mount` (gvfs) | Your file manager's network locations |

Shares are discovered over SMB itself, so any account that can open the shares
works; it does not need to be a HexOS admin. The password is stored in the
operating system's keychain (Keychain, Credential Manager, or Secret Service),
never in the config file and never on a command line.

Linux needs the gvfs SMB backend (`gvfs-smb` or `gvfs-backends`, depending on
the distribution) and a Secret Service provider such as GNOME Keyring or
KWallet. Auto-update on Linux works for the AppImage build only.

## Development

Requires [Rust](https://rustup.rs), Node.js, and the
[Tauri prerequisites](https://tauri.app/start/prerequisites/) for your platform.

```bash
npm install
npm run dev
```

The Rust core is in `src-tauri/src` (`lib.rs` for the sync loop and settings,
`remote.rs` for share discovery, `platform/` for per-OS mounting). The settings
window is plain HTML/CSS/JS in `ui/`, with no build step.

## Releasing

1. Bump `version` in `src-tauri/tauri.conf.json`, `src-tauri/Cargo.toml` and `package.json`.
2. Commit, then tag and push: `git tag v0.2.0 && git push --tags`.

The Release workflow builds installers for all three platforms and publishes
them with a signed `latest.json`. Installed copies check that file at startup
and every six hours, and install the new version automatically unless that is
turned off in the app.

Updates are signed. The workflow needs the private key in the repository
secret `TAURI_SIGNING_PRIVATE_KEY` (and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`,
empty if the key has no password). The matching public key is in
`src-tauri/tauri.conf.json`. Losing the private key means existing installs can
no longer be updated, so keep a backup.

The builds are not code-signed with Apple or Microsoft certificates, so macOS
Gatekeeper and Windows SmartScreen will warn on first launch.
