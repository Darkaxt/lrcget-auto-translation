# LRCGET AutoTranslation

Unofficial fork of [LRCGET](https://github.com/tranxuanthang/lrcget) with local, user-owned lyric translation support.

LRCGET scans your music library, finds existing lyric state, and can download lyrics from [LRCLIB](https://lrclib.net). This fork keeps the upstream library/search/download/edit behavior and adds optional automatic translation rows stored locally alongside the original lyrics.

## Fork Features

- Automatic lyric translation after download.
- Manual/bulk translation for lyrics already stored in the local database.
- Same-language detection, so English lyrics targeting English are marked `Already English` instead of being sent to a provider.
- Translation status badges: `Pending`, `Translated`, `Already English`, and `Failed`.
- Export modes for original lyrics, translated lyrics, or dual timestamp lyrics.
- Provider support for Gemini, DeepL, Google Cloud Translate, Microsoft Translator, and OpenAI-compatible chat completion endpoints.
- Gemini defaults to `gemini-flash-latest`.
- Provider retries for transient timeout/rate-limit/server failures.

Translations are local/user-owned data and are not published back to LRCLIB.

## Download

Latest packaged fork build: [v2.1.0-at.9](https://github.com/Darkaxt/lrcget-auto-translation/releases/tag/v2.1.0-at.9)

Windows:

- EXE installer, recommended: [LRCGET-AutoTranslation-v2.1.0-at.9-win-x64-setup.exe](https://github.com/Darkaxt/lrcget-auto-translation/releases/download/v2.1.0-at.9/LRCGET-AutoTranslation-v2.1.0-at.9-win-x64-setup.exe)
- MSI installer: [LRCGET-AutoTranslation-v2.1.0-at.9-win-x64.msi](https://github.com/Darkaxt/lrcget-auto-translation/releases/download/v2.1.0-at.9/LRCGET-AutoTranslation-v2.1.0-at.9-win-x64.msi)
- Checksums: [SHA256SUMS.txt](https://github.com/Darkaxt/lrcget-auto-translation/releases/download/v2.1.0-at.9/SHA256SUMS.txt)

Linux and macOS fork binaries are not currently published. Use the upstream [LRCGET releases](https://github.com/tranxuanthang/lrcget/releases) if you need an official non-Windows build without the translation fork changes.

## Experimental Auto-Sync

The Qwen/ASR auto-sync work was intentionally removed from `main` before shipping this fork. It is preserved for future recovery in:

[archive/autosync-experiment-2026-05-01](https://github.com/Darkaxt/lrcget-auto-translation/tree/archive/autosync-experiment-2026-05-01)

That branch is experimental and not part of the packaged release.

## Screenshots

![Tracks view](screenshots/01.png)

<details>
<summary>More screenshots</summary>

![Albums view](screenshots/02.png)

![Artists view](screenshots/03.png)

![LRCLIB view](screenshots/04.png)

</details>

## Upstream Project

This fork is based on [tranxuanthang/lrcget](https://github.com/tranxuanthang/lrcget), the official client for [LRCLIB](https://lrclib.net).

For upstream documentation, official multi-platform binaries, and general LRCGET support, see:

- Upstream repository: [tranxuanthang/lrcget](https://github.com/tranxuanthang/lrcget)
- Upstream releases: [tranxuanthang/lrcget/releases](https://github.com/tranxuanthang/lrcget/releases)
- LRCLIB: [lrclib.net](https://lrclib.net)

## Troubleshooting

**App will not open on Windows 10/11**

LRCGET depends on Microsoft WebView2. If Microsoft Edge or WebView2 was removed from Windows, reinstalling Microsoft Edge/WebView2 can fix startup issues. See upstream issue [tranxuanthang/lrcget#45](https://github.com/tranxuanthang/lrcget/issues/45).

**Audio cannot be played on Linux**

Try installing `pipewire-alsa`. For Ubuntu or Debian-based distros:

```shell
sudo apt install pipewire-alsa
```

**Scrollbar is invisible on Linux KDE Plasma**

This upstream issue can usually be fixed by changing the GNOME/GTK application style away from Breeze in KDE settings. See upstream comment [tranxuanthang/lrcget#44](https://github.com/tranxuanthang/lrcget/issues/44#issuecomment-1962998268).

## Development

LRCGET is made with [Tauri](https://tauri.app).

Development prerequisites:

- Microsoft Visual Studio C++ Build Tools on Windows
- Rust 1.81.0 or higher
- Node.js 16.18.0 or higher
- Tauri prerequisites for your OS: [Tauri prerequisites](https://tauri.app/start/prerequisites/)

Start the development app:

```shell
git clone https://github.com/Darkaxt/lrcget-auto-translation.git
cd lrcget-auto-translation
npm install
npm run tauri dev
```

Run checks:

```shell
npm run lint
npm run build
cd src-tauri
cargo test -- --nocapture
```

## Building

Build the app:

```shell
npm install
npm run tauri build
```

Built binaries are written under:

```text
src-tauri/target/release/
```

For platform-specific details, see the [Tauri distribution guide](https://tauri.app/distribute/).
