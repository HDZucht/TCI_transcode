---
title: TCI_transcode — a TCI receiver offered as the SDRconnect WebSocket API
type: software
version: 0.1.0
status: mvp-stage-1
created: 2026-11-14
author: Hans-Dieter Zucht with Argus (33feb1a0) (Claude Opus 5.5)
licence: MIT
tags: [sdr, tci, sdrconnect, decoder, bridge, rust]
---

# TCI_transcode

**TCI_transcode lets programs written for the SDRconnect WebSocket API run on any receiver that speaks TCI.** The first TCI source is SDR Oxide; the first client is msdr, which in turn runs multimon-ng, dsd-fme, rtl_433, direwolf and jt9. The decoders stay separate programs; the bridge only translates.

```
SDR Oxide (TCI server, port 50001) ──► TCI_transcode ──► SDRconnect API (port 5454) ──► msdr, ft8-module, TandemSDR …
```

## What it does (0.1.0)

- **TCI in:** connects to the TCI server, reads its status (centre, VFO, mode, filter, IQ rate), subscribes IQ and RX audio after `ready;`, reconnects with backoff.
- **SDRconnect out (API 1.0.3):** `get_property`/`set_property` with string values, `property_changed` to every client, stream switches per connection; binary payload 1 (audio PCM16 stereo 48 kHz), 2 (IQ int16, sent in 20 ms frames so that a client's frame queue holds enough signal), 3 (spectrum, 512 uint8 bins at about 23 frames/s, computed from the IQ). `signal_power` and `signal_snr` come from the same FFT.
- **Who tunes:** `--lead frontend` (default) answers a client's tuning with the unchanged value; `--lead client` forwards it as `vfo:`, `dds:` or `modulation:`. The IQ rate (`device_sample_rate`) is the bridge's own subscription and is always the client's to choose.
- Properties without a TCI counterpart (squelch, AGC, NR, LNA, antenna, display range, filter bandwidth) are stored and echoed; they act on nothing.

Not yet: the workbench web page and process API, a file source, a second tuner, device selection, recording, profiles.

## Run

```sh
# SDR Oxide: the station radio serves TCI by default (127.0.0.1:50001). Tune as usual.
cargo run --release -- --tci 127.0.0.1:50001 --listen 127.0.0.1:5454 --iq-rate 500000
msdr --host 127.0.0.1 --port 5454
```

Use 500 kS/s for rtl_433 in msdr: it wants ±100 kHz around the VFO inside the IQ window, and SDR Oxide places the IQ centre beside the VFO (about 28 kHz off).

`RUST_LOG=tci_transcode=debug` logs the TCI conversation both ways and counts the stream frames.

`cargo run --release --example probe -- ws://127.0.0.1:5454 4` checks any SDRconnect endpoint (the bridge or a real SDRconnect): reads the properties msdr reads, then counts spectrum, IQ and audio frames for four seconds.

## Kit (in progress)

`kit/tci-kit.toml` holds the kit's settings and one true/false line for each of msdr's 49 decoders, grouped by what msdr can run together (raw IQ, or audio in one receiver mode). `kit/make_decoder_config.py`, run with msdr's Python, regenerates it from msdr's own decoder registry and keeps the flags already set. The start script that reads it (bridge, msdr, decoders, TCI SPOT back to the receiver) comes next.

## Test

`cargo test`: 22 unit tests (both protocols, state, spectrum) and one end-to-end run without hardware: a fake TCI server sends a tone 20 kHz above the centre, an SDRconnect client reads the properties, finds the tone in the right spectrum bin, receives IQ and retunes through the bridge.

## Sources

- SDRconnect WebSocket API 1.0.3, SDRplay: https://www.sdrplay.com/websocket-api
- TCI, Expert Electronics: https://github.com/ExpertSDR3/TCI
- msdr's notes on how the real SDRconnect server behaves: https://github.com/gdamdam/msdr (docs/research)

Own code only. No SDR Oxide, SDRconnect or msdr code is linked or copied.

## Licence

MIT, see `LICENSE`. The programs it talks to keep their own licences (SDR Oxide and msdr: GPL-3); TCI_transcode reaches them over the network only.
