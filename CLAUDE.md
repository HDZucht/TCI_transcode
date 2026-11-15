# TCI_transcode — for AI instances

- **What:** translates a TCI receiver (SDR Oxide first) into the SDRconnect WebSocket API 1.0.3, so msdr and other SDRconnect clients run on it. Start with `README.md`, then `TCI_transcode - Executive Summary.md` (German, stages P1–P4).
- **Own product (HDZ 2026-11-14):** no patch to msdr, nothing in SDR Oxide. Own code from the published protocols only; do not copy SDR Oxide, msdr or SDRconnect code. Licence MIT (HDZ 2026-11-15).
- **Build cache on the Samsung:** `CARGO_TARGET_DIR=/Volumes/Samsung9100Pro/build-cache/tci_transcode`, Rust from `$(brew --prefix rustup)/bin`. Never on the internal disk.
- **Tests first:** `cargo test` must stay green, including the end-to-end run with the fake TCI server in `src/main.rs`.
- **Wrong demodulator mode is not a bug** (HDZ 2026-11-14: „dann funktioniert der Decoder halt nicht, das ist kein Drama“). No mode checks in the bridge; a hint per decoder on the workbench is enough.
- Vault folder is the source of truth. The GitHub repo `HDZucht/TCI_transcode` (private since 2026-11-15, tag v0.1.0) is a snapshot: edit here, then `bash tools/snapshot.sh` → `~/repos/TCI_transcode` → commit there. Making it public: only after HDZ says so.
