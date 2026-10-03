# ContextWitness

English | [日本語](README.ja.md)

ContextWitness is a Windows daemon that turns your screen activity into long-term, queryable memory. It captures the foreground window on a fixed cadence, reads the frames with Windows OCR, groups what it saw into time-anchored episodes, and delivers them to a [Hindsight](https://github.com/vectorize-io/hindsight) memory bank — so an assistant wired to that bank can answer questions like "what was I working on Tuesday afternoon?".

Requires Windows 11 24H2 (build 26100) or later. Building requires the MSVC toolchain. Delivering episodes to Hindsight requires Hindsight v0.8.6 or later.

## What v1 does

- Captures the foreground window every few seconds with Windows Graphics Capture, storing a frame only when enough pixels actually changed. Other windows and monitors are not captured; parts of the foreground window that other windows cover are.
- Extracts on-screen text with the Windows OCR engine, using the first configured language an installed engine exists for (Japanese, then English, by default).
- Groups captures into episode windows (5 minutes by default) rendered as a time-anchored activity log. The time between stored frames is logged too: no change above the capture threshold, no new frame received, no capturable foreground window, paused, skipped by the blacklist, capture or storage failures with their error text, and stretches with nothing recorded.
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

`setup` asks for the Hindsight API URL, an optional API token, and the data directory, and writes them. `run` starts capturing with the tray icon up and keeps running until stopped. Other commands: `status` (one-screen summary of what this installation is doing), `pause [30m|2h|...]`, `resume`, `defaults` (prints every setting with its built-in default and meaning), `autostart enable|disable` (start at logon), and `capture-once` (one manual capture pass, for checking the pipeline; it ignores `pause`).

## Configuration

`%APPDATA%\ContextWitness\config.toml`. The first time `setup` or `run` needs it, it is written holding only `storage.data_dir` and `hindsight.bank_id`; a setting it leaves out follows the built-in default of the running version, so a later version's defaults reach it. `contextwitness defaults` prints every setting with its default. The defaults:

| Key | Default | Meaning |
| --- | --- | --- |
| `capture.interval_secs` | `10` | Seconds between capture attempts (1–30). |
| `capture.change_pixel_threshold` | `8` | Per-pixel luma delta at or below which a pixel counts as unchanged (0–254). |
| `capture.change_area_logical_pixels` | `600` | Store and OCR a frame once more than this many logical pixels (measured at 100% display scaling) changed. |
| `capture.webp_quality` | `75` | WebP encoding quality (0–100). |
| `ocr.languages` | `["ja", "en"]` | Languages offered to the OCR engine, most important first; the first with an installed engine is the one used (your profile languages when none is). |
| `storage.data_dir` | `""` | Data directory; empty means `%LOCALAPPDATA%\ContextWitness`, otherwise an absolute path. |
| `storage.image_retention_days` | `14` | Days to keep captured images. |
| `storage.image_retention_max_gib` | `50` | Total captured-image storage cap in GiB. |
| `privacy.process_blacklist` | `[]` | Process names for which capture is disabled. |
| `hindsight.bank_id` | `"contextwitness"` | Hindsight bank the episodes go to. For a bank the list confirms as existing, its settings are left unchanged (changes made in Hindsight persist). When the bank is absent, ContextWitness PATCHes the bank config once with the initial settings — `retain_extraction_mode="chunks"` (episodes are stored as raw text chunks instead of LLM-extracted facts), `store_document_text=false`, and an `observations_mission` describing screen-OCR sources; every other setting inherits from Hindsight. Automatic initialization requires Hindsight's bank config API (`enable_bank_config_api`); where it is disabled, create the bank with the intended settings on the server side and ContextWitness will deliver without touching it. A bank another client creates concurrently after the check can still receive them once. Still give ContextWitness a bank of its own. |
| `hindsight.context_label` | `"Time-stamped OCR text of the foreground window, with its application and window title where known. May contain OCR errors; shows what was displayed, not what the user read, wrote, or did."` | Context label sent with every episode. |
| `episode.window_minutes` | `5` | Length of an episode window in minutes (1–1440). |

With `store_document_text=false`, Hindsight keeps no document or source-chunk body for these episodes, so document/chunk text retrieval, Reflect's expand, and re-processing from the stored source text are unavailable there. The raw chunk memories themselves and ContextWitness's local episode records remain.

Hindsight credentials never live in `config.toml`. `setup` writes them to `%USERPROFILE%\.hindsight\contextwitness.json`; alternatively the `CONTEXTWITNESS_HINDSIGHT_URL` and `CONTEXTWITNESS_HINDSIGHT_TOKEN` environment variables configure delivery (when the URL variable is set, the environment is taken as the whole credential set and the file is not read; the token variable alone is refused rather than silently paired with the file's URL).

## Privacy

ContextWitness records the screen. Know what that means before running it:

- **It collects everything by default.** The foreground window is captured whatever it shows, including any parts other windows cover; all text OCR recognizes in it is stored, and each entry records the window's title and process name whenever they can be read.
- **Everything stays on your machine except delivery to Hindsight.** Episode text — window titles, process names and capture status lines with their error text included — and its metadata (episode timing, local image paths) go to the Hindsight server you configured; nothing is sent anywhere else. The captured images themselves are never uploaded.
- **`privacy.process_blacklist`** skips capture while a listed process (matched by executable name, case-insensitively) is in the foreground; the episode still records the skip and the process name that caused it. When a blacklist is configured and the foreground process cannot be determined, the tick is skipped rather than risked.
- **Pause** stops the capture loop: from the tray menu, or `contextwitness pause 30m` (no duration means until `resume`).
- **Retention** prunes stored images by age and total size. Episode text in the local database is kept indefinitely — it is the memory this tool exists to build.
- **Logs are confidential.** They can contain captured content — window titles, process names, OCR text — for example where delivery diagnostics quote server-returned error text, which can quote a refused episode back. `contextwitness status` reprints the delivery error carried by the newest affected episode; treat its output the same way.

## License

[MIT](LICENSE)
