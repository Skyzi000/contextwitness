# ContextWitness

English | [日本語](README.ja.md)

ContextWitness is a Windows daemon that turns your screen activity into long-term, queryable memory. It captures every connected monitor on a fixed cadence, reads the frames with Windows OCR, groups what it saw into time-anchored episodes, and delivers them to a [Hindsight](https://github.com/vectorize-io/hindsight) memory bank — so an assistant wired to that bank can answer questions like "what was I working on Tuesday afternoon?".

Requires Windows 11 24H2 (build 26100) or later. Building requires the MSVC toolchain. Delivering episodes to Hindsight requires Hindsight v0.8.6 or later.

## What v1 does

- Captures all monitors every few seconds (DXGI desktop duplication, with a Windows Graphics Capture fallback per monitor), storing a frame only when enough pixels actually changed.
- Extracts on-screen text with the Windows OCR engine, using the first configured language an installed engine exists for (Japanese, then English, by default).
- Groups captures into episode windows (5 minutes by default) rendered as a time-anchored activity log.
- Stores everything locally first: episodes in SQLite, frames as WebP images with retention limits (14 days / 50 GiB by default).
- Delivers episodes to Hindsight through a persistent outbox: if the server is down, episodes wait and are delivered when it returns.
- Runs with a tray icon; `pause`/`resume` from the tray or the command line; optional start at logon.

## Install

Either:

- **Portable ZIP** — download `contextwitness-vX.Y.Z-windows-x86_64.zip` from [Releases](https://github.com/Skyzi000/contextwitness/releases), unzip anywhere, and run `contextwitness.exe` from a terminal.
- **From source** — `cargo install --git https://github.com/Skyzi000/contextwitness cw-daemon` (needs the Rust MSVC toolchain).

The binaries are unsigned open-source builds, so Windows SmartScreen may warn the first time you run one.

## Quickstart

```text
contextwitness setup
contextwitness run
```

`setup` asks for the Hindsight API URL, an optional API token, and the data directory, and writes them. `run` starts capturing with the tray icon up and keeps running until stopped. Other commands: `status` (one-screen summary of what this installation is doing), `pause [30m|2h|...]`, `resume`, `autostart enable|disable` (start at logon), and `capture-once [--wgc]` (one manual capture pass, for checking the pipeline; it ignores `pause`).

## Configuration

`%APPDATA%\ContextWitness\config.toml`, written with the defaults below the first time `setup` or `run` needs it:

| Key | Default | Meaning |
| --- | --- | --- |
| `capture.interval_secs` | `2` | Seconds between capture attempts (1–30). |
| `capture.change_pixel_threshold` | `8` | Per-pixel luma delta at or below which a pixel counts as unchanged (0–254). |
| `capture.change_area_logical_pixels` | `600` | Store and OCR a frame once more than this many logical pixels (measured at 100% display scaling) changed. |
| `capture.webp_quality` | `75` | WebP encoding quality (0–100). |
| `ocr.languages` | `["ja", "en"]` | Languages offered to the OCR engine, most important first; the first with an installed engine is the one used (your profile languages when none is). |
| `storage.data_dir` | `""` | Data directory; empty means `%LOCALAPPDATA%\ContextWitness`, otherwise an absolute path. |
| `storage.image_retention_days` | `14` | Days to keep captured images. |
| `storage.image_retention_max_gib` | `50` | Total captured-image storage cap in GiB. |
| `privacy.process_blacklist` | `[]` | Process names for which capture is disabled. |
| `hindsight.bank_id` | `"contextwitness"` | Hindsight bank the episodes go to. The daemon writes this bank's retain settings (mission, extraction mode, chunk size), so give ContextWitness a bank of its own. |
| `hindsight.context_label` | `"screen capture"` | Context label sent with every episode. |
| `episode.window_minutes` | `5` | Length of an episode window in minutes (1–1440). |

Hindsight credentials never live in `config.toml`. `setup` writes them to `%USERPROFILE%\.hindsight\contextwitness.json`; alternatively the `CONTEXTWITNESS_HINDSIGHT_URL` and `CONTEXTWITNESS_HINDSIGHT_TOKEN` environment variables configure delivery (when the URL variable is set, the environment is taken as the whole credential set and the file is not read; the token variable alone is refused rather than silently paired with the file's URL).

## Privacy

ContextWitness records the screen. Know what that means before running it:

- **It collects everything by default.** All connected monitors are captured, all readable on-screen text is extracted and stored, and every entry records the foreground window's title and process name.
- **Everything stays on your machine except delivery to Hindsight.** Episode text — window titles and process names included — and its metadata (episode timing, monitor identifiers, local image paths) go to the Hindsight server you configured; nothing is sent anywhere else. The captured images themselves are never uploaded.
- **`privacy.process_blacklist`** skips capture on *all* monitors while a listed process (matched by executable name, case-insensitively) is in the foreground. When a blacklist is configured and the foreground process cannot be determined, the tick is skipped rather than risked.
- **Pause** stops the capture loop: from the tray menu, or `contextwitness pause 30m` (no duration means until `resume`).
- **Retention** prunes stored images by age and total size. Episode text in the local database is kept indefinitely — it is the memory this tool exists to build.
- **Logs** contain no captured content, with one exception: delivery diagnostics quote server-returned error text, and a server refusing an episode can quote that episode back — window titles, process names, and OCR text alike. `contextwitness status` reprints the delivery error carried by the newest affected episode. Treat both as sensitive.

## License

[MIT](LICENSE)
