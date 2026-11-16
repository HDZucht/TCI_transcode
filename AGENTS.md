# AGENTS.md — how TCI_transcode works, for the next AI that touches it

> Written by Argus (33feb1a0) (Claude Opus 5.5), 2026-11-16, for HDZ (Hans-Dieter Zucht), who owns the product and has not read the code himself. You are the one who explains it to him. Read this file, then `README.md`; `CLAUDE.md` holds the house rules (build cache, licence, snapshot). Facts here were measured on real hardware unless marked otherwise.

## 1. What it is, in one sentence

A bridge that makes any **TCI** receiver (SDR Oxide, ExpertSDR, Thetis …) look like two other kinds of radio server — the **SDRconnect WebSocket API 1.0.3** and **rtl_tcp** — so decoders written for those (msdr, AbracaDABra, SDRangel, SDR++ …) run on it unchanged.

```
 receiver app (TCI server)            TCI_transcode                          clients
 SDR Oxide :50001/50002/…  ──TCI──▶  tci_link ──▶ Bridge ──▶ sdrc server  :5454  ◀── msdr, SDRconnect clients
   text: vfo, dds, iq_samplerate     (main.rs)     │     └─▶ rtltcp server :1234  ◀── AbracaDABra, SDRangel, SDR++
   binary: IQ / audio frames                       └─ commands back to TCI (tune, rate, mode)
```

One process serves **one** TCI server (one radio). Two radios → two processes with their own ports (`--tci`, `--listen`, `--rtltcp`).

## 2. Map of the code

| File | Role | Read it for |
|---|---|---|
| `src/main.rs` | entry point, CLI (`Args`), the shared `Bridge`, the TCI connection (`tci_link`), the SDRconnect accept loop (`serve`, `client`), end-to-end tests with a fake TCI server | how the parts are wired |
| `src/tci.rs` | the TCI protocol: 64-byte binary frame header, float32 payload, status lines (`split_commands`, `parse_status`), the commands the bridge sends (`cmd::vfo`, `cmd::dds`, `cmd::iq_samplerate` …) | what the receiver says and hears |
| `src/state.rs` | the one radio as both sides see it: TCI status in, SDRconnect property values out; `Lead` (who may tune); mode mapping both ways | what a client gets when it asks |
| `src/sdrc.rs` | the SDRconnect side: JSON events (`get_property`, `set_property`, `property_changed`), stream on/off, binary frames (audio=1, IQ=2, spectrum=3, int16) | the msdr path |
| `src/spectrum.rs` | the spectrum SDRconnect clients expect and TCI does not carry: 512 bins, ~23/s, uint8; channel power and S/N | displays in SDRconnect clients |
| `src/chunk.rs` | gathers TCI's small blocks into 20 ms frames for SDRconnect clients | why msdr stopped dropping frames |
| `src/rtltcp.rs` | the rtl_tcp server: header, commands, offset tuning, NCO shift, retune silence, resampling, 8-bit AGC | the DAB / ADS-B path |
| `src/resample.rs` | windowed-sinc resampler for complex IQ, any ratio | exact client rates (2.048 MS/s) |
| `src/bin/tci_diag.rs` | diagnostics against any TCI server: status burst, IQ rate and gaps | "does the server deliver?" |
| `examples/` | small probes used while building (`probe`, `tci_probe`, `dump_iq`) | experiments |
| `kit/` | Python: decoder settings for msdr (`tci-kit.toml`), generator and checker | the msdr workbench (in progress) |
| `tools/snapshot.sh`, `tools/commentonly.py` | vault → git snapshot; guard that an annotation pass changed comments only | housekeeping |

## 3. The data flow, step by step

1. **`tci_link` (main.rs)** opens `ws://<--tci>`, retries with back-off, sends `iq_samplerate`, `iq_start`, `audio_start`. Text lines go through `tci::parse_status` into `State::apply`; every property that changed is broadcast to SDRconnect clients as `property_changed`.
2. **Binary IQ frames** are parsed (`tci::parse_header`, `payload_f32`) and fan out on tokio broadcast channels in `Bridge`:
   - `iq` / `audio` / `spectrum` (bytes, already in SDRconnect framing, gathered to 20 ms by `chunk.rs`) for SDRconnect clients;
   - `iq_raw` (`Arc<RawIq>`: rate, centre from the last `dds`, float samples) for rtl_tcp clients — only filled while one is connected.
3. **Commands back to TCI** go through one `mpsc` channel (`Bridge.to_tci`) that `tci_link` writes out. Both client sides use it.
4. **SDRconnect client (`client` in main.rs + `sdrc::handle`)**: answers `get_property` from `State`, applies `set_property` (tuning only if `--lead client`), switches its streams on and off.
5. **rtl_tcp client (`rtltcp::client`)**, per connection:
   - writes the 12-byte header `RTL0` + tuner type 5 (R820T) + 29 gain steps;
   - reads 5-byte commands: `0x01` frequency, `0x02` sample rate; everything else (gain, AGC, bandwidth) is accepted and ignored — **TCI has no gain command**, so gain is set by hand in the receiver;
   - **offset tuning**: asks TCI for `(rate + 2·offset)·1.1` and tunes the radio's centre (`dds`) `--rtltcp-offset-khz` (default 1000) beside the client's frequency, VFO on the frequency; so the hardware's DC spike lies outside the decoder's channel (for DAB: outside the 1.536 MHz block; the resampler damps it only ~10 dB, and at 1.028 MHz it sits just past the output's Nyquist and folds back near the band edge, see §9);
   - **fallback**: if after `RATE_SWITCH_GRACE` (1.5 s) the stream is still too narrow for the offset (`holds`), it drops the offset and tunes on the frequency (logged as a warning; RTL-SDR at 2.4 MS/s does this);
   - **shift**: moves the client's frequency from the reported centre to zero (`shift_iq`, phase kept across blocks);
   - **retune silence**: after a frequency command, sends zeros until the server reports the new centre (`settled`: ±150 kHz or 1.5 s), so a decoder does not sync on the old channel;
   - **resample** to exactly the rate the client asked for (`resample.rs`), then **AGC to 8 bit** (`Agc::quantise`, RMS target −15 dBFS, one gain per block, down fast 0.3, up slow 0.03).

## 4. Invariants — do not "simplify" these away

- **Never decide on the first IQ block after a rate request.** Blocks at the old rate keep coming for a while; the 1.5 s grace exists because the fallback once fired on the first stale 250 kS/s block.
- **The centre comes from the server's `dds`, never from what we asked for.** SDR Oxide puts its centre 26.8–28.1 kHz beside the VFO and snaps the IQ rate to an integer decimation (2.000 instead of 2.048 MS/s). Everything downstream computes from the reported values.
- **`iq_raw` is fed only while an rtl_tcp client listens** (`receiver_count() > 0`): copies cost.
- **A lagging rtl_tcp client loses blocks and is told so in the log** (`RecvError::Lagged`), it is never allowed to stall the TCI link.
- **Silence, not old data, after a retune.** A DAB decoder that locks on the old ensemble needs seconds to recover.
- **Comments on lines of their own** (see §7), so `tools/commentonly.py` can prove an annotation pass changed no code.

## 5. Known behaviour of real clients and servers (measured 2026-11-14 … 16)

| Partner | What to know |
|---|---|
| SDR Oxide (TCI server) | One TCI server **per radio** since PR #896 (ports 50001, 50002, …); a switched-off radio still holds its port. Read the truth from `~/Library/Application Support/org.sdroxide.sdroxide/radio-<id>/tciserver.json` or measure with `tci_diag iq --tci host:port`. On macOS the server dropped clients at high IQ rates until PR #903 (sockets left non-blocking). |
| AbracaDABra 4.3 (DAB+) | rtl_tcp at 2.048 MS/s; plays with offset tuning from a RSP1A at 6 MS/s, RS uncorrectable 0. It steps gain 14–26 — ignored, set gain in SDR Oxide. |
| SDRangel 8.2 RemoteTCPInput | Protocol "SDRangel" also speaks rtl_tcp. Reads the header only after ≥64 bytes, so **no IQ → it hangs up after 2–5 s** ("Not connected"). Sees an RTL-SDR → max 3.2 MS/s, 8 bit. Sends its start frequency (435 MHz) on connect, which retunes the radio with `--lead client`. Two SDRangel devices on one bridge fight over the frequency. |
| msdr (SDRconnect client) | keeps 40 IQ frames; needed the 20 ms gathering (`chunk.rs`). |
| RTL-SDR in SDR Oxide | 2.4 MS/s: too narrow for 1 MHz offset at a 2 MS/s client → fallback, DC spike in band; use the client's DC correction. |

## 6. Build, test, run

```sh
cd "<vault>/30 Software Development/projects/SDR-Radio/TCI_transcode"     # source of truth
export CARGO_TARGET_DIR=/Volumes/Samsung9100Pro/build-cache/tci_transcode # never the internal disk
export PATH="$(brew --prefix rustup)/bin:$PATH"                              # rustc must be on PATH, not only cargo
nice -n 19 cargo test -j 2                                                # HDZ often listens live: build gently
cargo run --release -- --tci 127.0.0.1:50002 --lead client --rtltcp 127.0.0.1:1234
cargo run --release --bin tci_diag -- iq --tci 127.0.0.1:50002 --rate 2000000 --secs 3
```

- Tests include end-to-end runs over TCP against a fake TCI server (`main.rs` tests module): SDRconnect tone + tuning, rtl_tcp header + commands + tone after the shift.
- `RUST_LOG=tci_transcode=debug` shows every TCI line and every rtl_tcp command; that log answered every bug so far.
- Do not run `cargo fmt` on the whole project: it is not stock-formatted. Format only your own files with `rustfmt --config use_small_heuristics=Max <file>`.
- Publishing: edit in the vault, `bash tools/snapshot.sh` → `~/repos/TCI_transcode`, commit there (signature + `Co-Authored-By`), push only with HDZ's OK (over HTTPS with the gh token; SSH via Secretive can hang).

## 7. How the code is annotated: the Hosenträger method

HDZ's rule for every line written for him: three levels run side by side and say the same thing, so a reader — human or model — can dock on without reading the code, and a contradiction between levels shows a bug.

- **Code** — what runs.
- **LLM level** — each file's `//!` head carries a fixed grammar: **Ends** (what it is for) · **Ways** (how) · **Means** (dependencies) · **Provides** (API, CLI) · **Expects** (preconditions) · **Ensures** (postconditions) · **Fails** (failure modes) · **Tags** (closed set: #radio #tci #rtltcp #sdrconnect #dsp #net #test #build) · **See** (links); test files add **Proves**. Model: `src/bin/tci_diag.rs`, `tools/snapshot.sh`.
- **Human level** — `///` on every type, function, constant and non-obvious field: purpose, in/out, unit (Hz, S/s, samples, dBFS, ms), side effects, failure case. Inside bodies: whole-line comments at each non-obvious step, and `// Why:` (with the reason or HDZ's quote, German quotes stay German), `// Pitfall:`, `// Invariant:` wherever someone would otherwise undo a deliberate choice.
- Rules: comments on their own lines only; a pass that only annotates must pass `python3 tools/commentonly.py` (compares code lines against the last snapshot); a comment that lies is worse than none — read the code first.

## 8. Where to extend

- **A new client protocol** (e.g. SpyServer, SoapyRemote): a module like `rtltcp.rs` with its own `serve(listener, iq_raw receiver, to_tci sender, …)`, spawned in `main` behind its own `--flag`; take IQ from `Bridge.iq_raw`, send TCI commands through `to_tci`, keep its tuning rules in the module.
- **Gain**: needs a TCI command first (proposed as a small PR to SDR Oxide); then map rtl_tcp `0x04`/`0x0d` onto it.
- **One stream, several client rates** (msdr at 500 kS/s and DAB at 2.048 MS/s together): ask TCI for the highest, resample per client — the resampler already works per connection.
- **SDRangel's own header ("SDRA")** would lift the 3.2 MS/s / 8-bit limit for SDRangel clients.

## 9. Known issues (found 2026-11-16 by the annotation pass, not fixed yet)

1. **`--lead frontend` still widens the TCI rate** for the offset (`(r + 2·offset)·1.1`) although it never shifts; the client gets the stream centre, DC spike included. The offset probably belongs to `Lead::Client` only.
2. **The fallback does not arm the retune silence:** after dropping the offset it retunes `dds` but sends old-centre blocks until the server reports the new one; a retune in flight keeps waiting for `f + offset` for the full 1.5 s.
3. **The fallback is one-way per connection:** a later `0x02` rate change does not bring the offset back.
4. **DC spike only damped, not removed:** 32-tap kernel at 5–6 MS/s → 2.048 MS/s is −1.0/−1.4 dB at 768 kHz and only −11/−10 dB at 1.028 MHz, which aliases to about −1.020 MHz. Outside a DAB block, but visible in a client's spectrum. More taps or a larger offset would fix it.
5. **Rates are shared:** every rtl_tcp connect sets the one TCI `iq_samplerate` (last one wins), also for SDRconnect clients (see §8, one stream for several rates).
6. Retune silence quantises to 128, not 127.5 (+0.5 LSB, negligible); `tci_diag iq` reads rates slightly low when the first frame is late; `state.rs` derives the VFO from `if` with the centre known at that moment (a later `dds` alone does not move it); `examples/probe.rs` prints JSON quotes.

## 10. Where things are

- Vault (source): `30 Software Development/projects/SDR-Radio/TCI_transcode/`; German notes beside the code (`TCI_transcode - Executive Summary.md`, `Recherche - …`) stay out of the repo (snapshot excludes them).
- Public repo: https://github.com/HDZucht/TCI_transcode (MIT). Use cases A (DAB+ in AbracaDABra) and B (ADS-B in SDR Oxide and SDRangel at once) in the README.
- History and open loops: `../BREADCRUMBS-SDR.md`; ADS-B sources and measurements: `../ADS-B - Dossier Datenquellen und Empfang.md`.
