# Changelog

## 1.1.0 - 2026-10-19

The database is migrated, and 1.0.0 cannot open it afterwards. Back up `db.sqlite3` in the data directory first if you may go back.

- Audio recording, off by default: the speech applications play is recorded as text. See [Audio recording](README.md#audio-recording).
- A bank mission written by 1.0.0 is replaced with one that also covers audio.
- Building from source needs a fetch script first. See [Building from source](README.md#building-from-source).
- rustls is updated to 0.23.46 (RUSTSEC-2026-0346 to 0349).

## 1.0.0 - 2026-10-11

First release.
