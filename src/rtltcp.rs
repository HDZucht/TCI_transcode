//! The rtl_tcp protocol on the server side, so a program written for an
//! RTL-SDR over the network (AbracaDABra, SDR++, GQRX, welle.io, SDRangel,
//! rtl_433 -d rtl_tcp:…, and through SoapyRTLTCP every SoapySDR program)
//! receives the TCI server's IQ.
//!
//! The protocol, as the reference rtl_tcp speaks it:
//! - On connect the server sends 12 bytes: `RTL0`, the tuner type and the
//!   number of gain steps, both u32 big-endian. Here: R820T (5), 29 steps.
//! - Then nothing but IQ: unsigned 8-bit, interleaved I,Q, 127.5 is zero.
//! - The client sends 5-byte commands: one byte command, u32 big-endian value.
//!   0x01 frequency (Hz), 0x02 sample rate (S/s); 0x03–0x0e are gain, AGC,
//!   ppm, direct sampling, bias-T and the like, which a TCI receiver sets in
//!   its own front end: accepted and ignored.
//!
//! Offset tuning (`--rtltcp-offset-khz`, 1000 by default): the hardware centre
//! goes that far above the client's frequency, so the receiver's DC spike
//! lands outside the client's band (a DAB block is ±768 kHz); the TCI stream
//! is asked for wide enough to hold both, and the bridge cuts the client's
//! window out of it. When the TCI server cannot deliver that rate, the client
//! falls back to tuning on the frequency itself.
//!
//! A TCI server may put the IQ centre beside the frequency asked for (SDR
//! Oxide: 26.8 kHz above it, measured 2026-11-16 at 237.488 MHz); the IQ is
//! shifted so the client's frequency sits at zero, from the centre the server
//! reports.
//!
//! The IQ is resampled to exactly the rate the client asked for (a TCI server
//! snaps to an integer decimation of its hardware rate) and scaled by a slow
//! AGC to 8 bits, RMS at −15 dBFS, the gain held constant within a block.
//!
//! Ends:     let any rtl_tcp client (DAB, ADS-B, ISM decoders, SDR front ends) receive a TCI radio as if it were
//!           an RTL-SDR on the network, at exactly its own rate and frequency, without the hardware's DC spike.
//! Ways:     per connection: header, then a select loop over 5-byte commands and raw TCI IQ blocks; commands become
//!           TCI `iq_samplerate` / `vfo` / `dds`; each block is checked (rate wide enough, retune settled), shifted
//!           by an NCO from the reported centre to the client's frequency, resampled, AGC'd to u8 and written.
//! Means:    tokio (TCP, broadcast, mpsc); resample.rs; state.rs `Lead`; tci.rs `cmd`.
//! Provides: `serve(listener, iq, to_tci, tuning)`, `Tuning`, `RawIq`, `Cmd`, `parse_cmd`, `header`, `tci_rate`,
//!           `holds`, `settled`, `tci_commands`, `Agc`, `shift_iq`, `DEFAULT_RATE`.
//! Expects:  main.rs fills `Bridge.iq_raw` with every TCI IQ block (rate from the frame header, centre from the
//!           last `dds`) while a client listens, and writes `to_tci` lines to the TCI server.
//! Ensures:  the client gets its asked rate exactly; with `--lead client` its frequency sits at 0 Hz (when it lies
//!           inside the TCI stream); after a
//!           retune it gets zeros, never the old channel, for up to 1.5 s; a slow client loses blocks (logged) and
//!           never stalls the TCI link.
//! Fails:    a TCI stream too narrow for the offset after 1.5 s → offset dropped for this client, tuned on the
//!           frequency (warning; DC spike now in band). Gain commands do nothing (TCI has no gain command).
//!           A server that never reports the new centre → IQ resumes after 1.5 s at whatever centre it reports.
//! Tags:     #rtltcp #net #dsp #radio
//! See:      main.rs `tci_link` (feeds `iq_raw`) and e2e test `rtl_tcp_header_commands_and_tone`, resample.rs,
//!           AGENTS.md §3–§5 (invariants, measured client behaviour).
//
// Why: HDZ 2026-11-16: "SDRconnect und rtl_tcp wäre schon mehr als genug, da
//      die meisten second party libs das sprechen"; first client AbracaDABra
//      for DAB on SDR Oxide's receivers. Offset tuning: HDZ the same day,
//      AbracaDABra on a RSP1A: "beim SDR ist leider in der Mitte dieses
//      hässliche Artefakt", "du musst den Ausschnitt wirklich auf die Breite
//      zuschneiden".

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use crate::resample::Resampler;
use crate::state::Lead;
use crate::tci;

/// R820T in the reference rtl_tcp's tuner list.
// Why: the most common RTL-SDR tuner; clients treat it as a normal 8-bit
//      device up to 3.2 MS/s (SDRangel reads it that way).
const TUNER_R820T: u32 = 5;
/// Gain steps the R820T announces (29); clients build their gain slider from it.
/// The steps do nothing here: TCI has no gain command.
const GAIN_STEPS: u32 = 29;
/// What a client gets until it sets a rate: the DAB rate, and RTL-SDR's usual.
pub const DEFAULT_RATE: u32 = 2_048_000;
/// How long a TCI server gets to switch its IQ rate before a stream too
/// narrow for offset tuning counts as its answer. SDR Oxide switches within a
/// block or two; the first blocks after a request still carry the old rate.
// Why: 2026-11-16, the first block after AbracaDABra connected came at the
//      bridge's earlier 250 kS/s and switched offset tuning off within 0.4 ms.
const RATE_SWITCH_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);
/// How long after a retune the bridge waits for the TCI server to report the
/// new centre, sending silence meanwhile; and how far the reported centre may
/// lie from the one asked for (SDR Oxide adds ~28 kHz of its own).
// Why: HDZ 2026-11-16: right after a channel change the reported centre was
//      still the old one and the stream came out shifted by the channel step
//      (+684 to +844 kHz instead of −1,028 kHz); "Glätten nach Kanalwechsel
//      ist für das Do-it-yourself-Panorama auch noch nötig". Silence is what a
//      sweep can skip; a spectrum from the wrong frequency it cannot tell.
const SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_millis(1500);
/// Distance in Hz the reported centre may lie from the asked one and still
/// count as the new centre. 150 kHz: far above SDR Oxide's 26.8–28.1 kHz, far
/// below a DAB channel step (1.712 MHz from 9A to 9B).
const SETTLE_TOLERANCE_HZ: i64 = 150_000;

/// Whether a block whose stream is centred at `center_hz` belongs to the
/// retune that asked for `expected_hz` `since` ago: yes once the centre is
/// there, or once the wait is over.
///
/// In: Hz, Hz, time since the retune. Out: true = stop the silence.
pub fn settled(center_hz: i64, expected_hz: i64, since: std::time::Duration) -> bool {
    // Pitfall: the time limit is not optional. A server that never reports
    //          (or reports a centre further off than the tolerance) would
    //          otherwise keep the client silent for ever.
    (center_hz - expected_hz).abs() <= SETTLE_TOLERANCE_HZ || since > SETTLE_LIMIT
}

/// RMS the AGC aims at, of full scale: −15 dBFS leaves OFDM's peaks room.
// Why: 20·log10(0.178) = −15.0. OFDM (DAB) peaks run some 10–12 dB above
//      its RMS; −15 dBFS leaves them that room before 8 bits clip.
const TARGET_RMS: f32 = 0.178;

/// One block of interleaved float IQ as the TCI server sent it.
/// Shared by all rtl_tcp clients through an `Arc` (one copy per block, not per client).
pub struct RawIq {
    /// Sample rate from the TCI frame header, S/s: what the server really sends.
    pub rate: u32,
    /// The IQ stream's centre as the TCI server last reported it (`dds`).
    pub center_hz: i64,
    /// Interleaved I,Q float32, full scale ±1.
    pub data: Vec<f32>,
}

/// A command from the client.
#[derive(Debug, PartialEq)]
pub enum Cmd {
    /// 0x01: tune to this frequency, Hz.
    Frequency(u32),
    /// 0x02: deliver this sample rate, S/s.
    SampleRate(u32),
    /// Any other command byte and its value (gain, AGC, ppm …); ignored.
    Other(u8, u32),
}

/// Decode one 5-byte client command: byte 0 = command, bytes 1–4 = u32 big-endian value.
pub fn parse_cmd(b: &[u8; 5]) -> Cmd {
    let v = u32::from_be_bytes([b[1], b[2], b[3], b[4]]);
    match b[0] {
        0x01 => Cmd::Frequency(v),
        0x02 => Cmd::SampleRate(v),
        c => Cmd::Other(c, v),
    }
}

/// The 12-byte greeting: `RTL0`, tuner type (R820T = 5), gain steps (29), u32 big-endian.
// Pitfall: SDRangel's RemoteTCPInput reads this header only once at least 64
//          bytes have arrived, so the header alone, with no IQ behind it,
//          makes it hang up after 2–5 s (measured 2026-11-16).
pub fn header() -> [u8; 12] {
    let mut h = [0u8; 12];
    h[..4].copy_from_slice(b"RTL0");
    h[4..8].copy_from_slice(&TUNER_R820T.to_be_bytes());
    h[8..12].copy_from_slice(&GAIN_STEPS.to_be_bytes());
    h
}

/// Who may retune, and how far beside the client's frequency the hardware
/// centre goes.
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// `--lead`: with `Frontend` the client's frequency commands are not forwarded.
    pub lead: Lead,
    /// Hardware centre minus client frequency, Hz (`--rtltcp-offset-khz` × 1000);
    /// 0 = tune on the frequency. Per client, and set to 0 by the fallback.
    pub offset_hz: i64,
}

/// The TCI rate that holds the client's band `offset_hz` off centre with
/// room: twice the offset plus the band, and a tenth more because the server
/// snaps to an integer share of its hardware rate.
///
/// Example: 2.048 MS/s at 1 MHz offset → 4.4528 MS/s asked; SDR Oxide gives 5 or 6 MS/s.
pub fn tci_rate(client_rate: u32, offset_hz: i64) -> u32 {
    if offset_hz == 0 {
        return client_rate;
    }
    // Twice the offset: the stream is centred on the hardware, so it must
    // reach `offset` on one side plus the client's half band, and is symmetric.
    ((client_rate as f64 + 2.0 * offset_hz.abs() as f64) * 1.1) as u32
}

/// Whether a stream at `rate` holds the client's band at `offset_hz`: the
/// band's outer edge (80 % of the client's half rate) inside the stream's.
///
/// In Hz: |offset| + 0.4 × client rate + 50 kHz margin ≤ stream rate / 2.
pub fn holds(rate: u32, client_rate: u32, offset_hz: i64) -> bool {
    offset_hz.abs() as f64 + 0.4 * client_rate as f64 + 50_000.0 <= rate as f64 / 2.0
}

/// The TCI commands a client's command turns into.
///
/// Rate → `iq_samplerate` (widened for the offset), whatever the lead.
/// Frequency → `vfo` + `dds` at frequency + offset, only with `Lead::Client`.
/// Everything else → nothing.
pub fn tci_commands(cmd: &Cmd, t: Tuning) -> Vec<String> {
    match *cmd {
        // Rate 0 is not a request; skip it rather than ask TCI for nothing.
        Cmd::SampleRate(r) if r > 0 => vec![tci::cmd::iq_samplerate(tci_rate(r, t.offset_hz))],
        // The IQ stream's centre is the TCI server's DDS; the VFO goes along
        // so the operator sees where the hardware sits.
        Cmd::Frequency(hz) if t.lead == Lead::Client => {
            let c = hz as i64 + t.offset_hz;
            vec![tci::cmd::vfo(c), tci::cmd::dds(c)]
        }
        _ => Vec::new(),
    }
}

/// Slow AGC to 8 bits: one gain per block, following the block RMS.
pub struct Agc {
    /// Current linear gain; `None` until the first block with signal sets it outright.
    gain: Option<f32>,
}

impl Agc {
    /// An AGC with no gain yet; the first non-silent block sets it in one step.
    pub fn new() -> Agc {
        Agc { gain: None }
    }

    /// Scale one block of interleaved float I,Q and append it to `out` as
    /// unsigned 8 bit (127.5 = zero, rounded, clipped to 0…255).
    ///
    /// The gain is updated once from this block's RMS, then applied to the
    /// whole block. Silence (RMS ≤ 1e-9) leaves the gain unchanged.
    pub fn quantise(&mut self, iq: &[f32], out: &mut Vec<u8>) {
        if iq.is_empty() {
            return;
        }
        // RMS of the complex sample: mean of I² and Q² over all values, × 2 = mean |z|².
        let rms = (iq.iter().map(|v| v * v).sum::<f32>() / iq.len() as f32 * 2.0).sqrt();
        // Why: the silence after a retune must not pump the gain to its limit;
        //      the first block of the new channel would then clip hard.
        if rms > 1e-9 {
            // Gain that would hit the target exactly, bounded to 1e-3 … 1e6.
            let want = (TARGET_RMS / rms).clamp(1e-3, 1e6);
            // First block: take it outright, no settling from nothing.
            let g = self.gain.get_or_insert(want);
            // About a second to settle at ~100 blocks a second; down fast, so a
            // strong signal clips for a block at most.
            // Why: up slow (0.03), so a short dip (fading, DAB's null symbol)
            //      does not lift the noise; down fast (0.3), because a clipped
            //      block is lost, while too little gain only costs resolution.
            *g += (want - *g) * if want < *g { 0.3 } else { 0.03 };
        }
        let g = self.gain.unwrap_or(1.0);
        // Pitfall: exact zero rounds to 128, not 127.5; a constant +0.5 LSB on
        //          silence, harmless for a decoder.
        out.extend(iq.iter().map(|&v| (v * g * 127.5 + 127.5).round().clamp(0.0, 255.0) as u8));
    }
}

impl Default for Agc {
    /// Same as `Agc::new()`.
    fn default() -> Self {
        Agc::new()
    }
}

/// Move a signal `offset_hz` above the centre down to zero: multiply by
/// e^(−j2π·offset·t), the phase carried across blocks.
///
/// In: interleaved I,Q, offset Hz (negative = below centre), rate S/s, the
/// caller's oscillator phase (radians, updated). Out: a new shifted block.
// Invariant: `phase` lives with the client, not the block; restarting it per
//            block would put a phase jump at every block edge. The test splits
//            at an odd point to prove the phase runs on.
pub fn shift_iq(iq: &[f32], offset_hz: f64, rate: u32, phase: &mut f64) -> Vec<f32> {
    let step = -std::f64::consts::TAU * offset_hz / rate as f64;
    let mut out = Vec::with_capacity(iq.len());
    for v in iq.chunks_exact(2) {
        // Complex multiply (I + jQ)·(cos φ + j sin φ).
        let (c, s) = (phase.cos() as f32, phase.sin() as f32);
        out.push(v[0] * c - v[1] * s);
        out.push(v[0] * s + v[1] * c);
        // Wrap to keep f64 precision over hours of streaming.
        *phase = (*phase + step) % std::f64::consts::TAU;
    }
    out
}

/// Accept rtl_tcp clients for ever.
///
/// Each client gets its own task, its own receiver on the raw IQ broadcast and
/// its own copy of `tuning` (the fallback changes one client only).
pub async fn serve(
    listener: TcpListener,
    iq: broadcast::Sender<Arc<RawIq>>,
    to_tci: tokio::sync::mpsc::UnboundedSender<String>,
    tuning: Tuning,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tracing::info!(%peer, "rtl_tcp client connected");
                // Subscribing here also turns on main.rs's iq_raw feed
                // (it fills the channel only while receiver_count() > 0).
                tokio::spawn(client(stream, iq.subscribe(), to_tci.clone(), tuning));
            }
            // An accept error (e.g. out of file descriptors) is logged; the loop goes on.
            Err(e) => tracing::warn!("rtl_tcp accept failed: {e}"),
        }
    }
}

/// One rtl_tcp client from greeting to hang-up.
///
/// Sends the header and a first rate request, then loops: client commands →
/// TCI commands; TCI IQ blocks → (fallback check, retune silence, shift,
/// resample, AGC) → u8 bytes to the client. Ends when the client closes, a
/// write fails or the IQ broadcast closes.
async fn client(
    stream: TcpStream,
    mut iq: broadcast::Receiver<Arc<RawIq>>,
    to_tci: tokio::sync::mpsc::UnboundedSender<String>,
    mut tuning: Tuning,
) {
    let lead = tuning.lead;
    // Disable Nagle, so each write leaves at once.
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    if wr.write_all(&header()).await.is_err() {
        return;
    }
    // Rate the client wants, S/s; DEFAULT_RATE until it says otherwise.
    let mut want = DEFAULT_RATE;
    // Ask for the default rate at once: a client that sends 0x02 late, or
    // never, still gets a stream at 2.048 MS/s.
    // Pitfall: the TCI stream is one per radio; this changes the rate for every
    //          other client of the bridge too.
    let _ = to_tci.send(tci::cmd::iq_samplerate(tci_rate(want, tuning.offset_hz)));
    // When the rate was last asked for: until the server has switched, blocks
    // still come at the old rate and say nothing about the new one.
    let mut rate_asked = std::time::Instant::now();
    // Built on the first block, rebuilt whenever (block rate, wanted rate) changes.
    let mut rs: Option<Resampler> = None;
    let mut agc = Agc::new();
    // Reused buffers: resampled floats and the bytes for the socket.
    let (mut fl, mut bytes) = (Vec::new(), Vec::new());
    let mut cmd = [0u8; 5];
    let mut lead_warned = false;
    let mut lost = 0u64;
    // The frequency the client tuned to (only when it may tune), and the
    // phase of the oscillator that moves it to zero.
    let mut tuned: Option<i64> = None;
    let mut phase = 0f64;
    // Last logged shift, Hz, so a changed shift is logged once, not per block.
    let mut shift_logged = i64::MIN;
    // A retune in flight: when it was asked for and the centre it should bring.
    let mut retune: Option<(std::time::Instant, i64)> = None;
    // Blocks silenced for the current retune (for the debug log).
    let mut muted = 0u64;
    loop {
        tokio::select! {
            // Client → bridge: one 5-byte command. A short read or EOF ends the client.
            r = rd.read_exact(&mut cmd) => {
                if r.is_err() {
                    break;
                }
                let c = parse_cmd(&cmd);
                tracing::debug!(?c, "rtl_tcp command");
                // A new rate: restart the grace, so the fallback does not judge
                // the stream on blocks at the old rate.
                if let Cmd::SampleRate(r) = c && r > 0 {
                    want = r;
                    rate_asked = std::time::Instant::now();
                }
                // A new frequency (client leads): remember it for the shift, and
                // arm the silence until the server reports the new centre.
                if let Cmd::Frequency(hz) = c && lead == Lead::Client {
                    tuned = Some(hz as i64);
                    retune = Some((std::time::Instant::now(), hz as i64 + tuning.offset_hz));
                }
                // Frontend leads: the client's frequency is ignored; say so once,
                // since the client then receives another channel than it asked for.
                if let Cmd::Frequency(hz) = c && lead == Lead::Frontend && !lead_warned {
                    tracing::warn!(hz, "rtl_tcp client tunes, but --lead frontend keeps the front end's frequency; start with --lead client to let it");
                    lead_warned = true;
                }
                for line in tci_commands(&c, tuning) {
                    let _ = to_tci.send(line);
                }
            }
            // TCI → client: one raw IQ block.
            b = iq.recv() => match b {
                Ok(block) => {
                    // A stream too narrow for the offset: tune on the frequency.
                    // Invariant: only after RATE_SWITCH_GRACE since the last rate
                    //            request; earlier blocks may still carry the old rate.
                    if tuning.offset_hz != 0
                        && rate_asked.elapsed() > RATE_SWITCH_GRACE
                        && !holds(block.rate, want, tuning.offset_hz)
                    {
                        tracing::warn!(rate = block.rate, offset_hz = tuning.offset_hz, "rtl_tcp: TCI stream too narrow for offset tuning; tuning on the frequency (raise the radio's sample rate to keep its DC spike out)");
                        // One-way for this client: the offset stays 0 until it reconnects.
                        tuning.offset_hz = 0;
                        let _ = to_tci.send(tci::cmd::iq_samplerate(want));
                        // Retune on the frequency itself, if the client has set one.
                        if let Some(f) = tuned {
                            for line in tci_commands(&Cmd::Frequency(f as u32), tuning) {
                                let _ = to_tci.send(line);
                            }
                        }
                    }
                    // (Re)build the resampler when either rate changed.
                    if rs.as_ref().map(|r| r.rates()) != Some((block.rate, want)) {
                        tracing::info!(from = block.rate, to = want, "rtl_tcp IQ rate");
                        rs = Some(Resampler::new(block.rate, want));
                    }
                    // Until the server reports the new centre, send silence.
                    if let Some((t, expected)) = retune {
                        if settled(block.center_hz, expected, t.elapsed()) {
                            if muted > 0 {
                                tracing::debug!(blocks = muted, ms = t.elapsed().as_millis() as u64, "rtl_tcp retune settled");
                            }
                            retune = None;
                            muted = 0;
                        } else {
                            muted += 1;
                        }
                    }
                    let silent = retune.is_some();
                    // The NCO shift: client frequency minus reported centre, Hz.
                    // Only when the client has tuned, the server has reported a
                    // centre (≠ 0), and the frequency lies inside the stream.
                    let shift = tuned
                        .map(|f| f - block.center_hz)
                        .filter(|d| block.center_hz != 0 && d.abs() < block.rate as i64 / 2);
                    let data = match shift {
                        Some(d) if d != 0 => {
                            if d != shift_logged {
                                tracing::info!(offset_hz = d, "rtl_tcp IQ shifted to the client's frequency");
                                shift_logged = d;
                            }
                            shift_iq(&block.data, d as f64, block.rate, &mut phase)
                        }
                        // No shift known (frontend leads, no centre yet, or the
                        // frequency outside the stream): pass the block as it is.
                        _ => block.data.clone(),
                    };
                    // Why: zeros of the same length, not a skipped block, so the
                    //      client's sample clock keeps running at its rate.
                    let data = if silent { vec![0.0; data.len()] } else { data };
                    fl.clear();
                    // `rs` was set just above, so the unwrap cannot fail.
                    rs.as_mut().unwrap().process(&data, &mut fl);
                    bytes.clear();
                    agc.quantise(&fl, &mut bytes);
                    // A failed write = client gone.
                    if wr.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
                // Behind by `n` blocks (512-block broadcast queue in main.rs):
                // they are gone; the client continues with the oldest block still queued.
                // Invariant: never block the TCI link for one slow client.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    lost += n;
                    tracing::warn!(blocks = n, total = lost, "rtl_tcp client behind; IQ blocks lost");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    tracing::info!("rtl_tcp client gone");
}

/// Proves: the header is RTL0/R820T/29; commands parse big-endian; tuning follows
/// the lead; offset tuning moves the centre and widens the stream (and `holds`
/// matches what SDR Oxide snaps to); a tone at the tuned frequency lands at 0 Hz
/// across a block split; a retune waits for the new centre, but not for ever;
/// the AGC centres at 127.5 and levels to TARGET_RMS.
#[cfg(test)]
mod tests {
    use super::*;

    /// The 12-byte greeting byte for byte.
    #[test]
    fn header_is_rtl0_r820t() {
        let h = header();
        assert_eq!(&h[..4], b"RTL0");
        assert_eq!(u32::from_be_bytes(h[4..8].try_into().unwrap()), 5);
        assert_eq!(u32::from_be_bytes(h[8..12].try_into().unwrap()), 29);
    }

    /// 0x01 / 0x02 / other command bytes, values big-endian.
    #[test]
    fn commands_parse_big_endian() {
        assert_eq!(parse_cmd(&[0x01, 0x0d, 0x22, 0xa6, 0x00]), Cmd::Frequency(220_374_528));
        assert_eq!(parse_cmd(&[0x02, 0x00, 0x1f, 0x40, 0x00]), Cmd::SampleRate(2_048_000));
        assert_eq!(parse_cmd(&[0x04, 0, 0, 0x01, 0x2c]), Cmd::Other(4, 300));
    }

    /// Without offset: client lead tunes vfo + dds; frontend lead does not; rates always pass; gain never.
    #[test]
    fn tuning_follows_the_lead() {
        let f = Cmd::Frequency(220_352_000);
        let plain = |lead| Tuning { lead, offset_hz: 0 };
        assert_eq!(
            tci_commands(&f, plain(Lead::Client)),
            vec!["vfo:0,0,220352000;", "dds:0,220352000;"]
        );
        assert!(tci_commands(&f, plain(Lead::Frontend)).is_empty());
        assert_eq!(
            tci_commands(&Cmd::SampleRate(2_048_000), plain(Lead::Frontend)),
            vec!["iq_samplerate:2048000;"]
        );
        assert!(tci_commands(&Cmd::Other(3, 1), plain(Lead::Client)).is_empty());
    }

    /// 1 MHz offset: centre 1 MHz up, rate widened; `holds` against real SDR Oxide rates.
    #[test]
    fn offset_tuning_moves_the_centre_and_widens_the_stream() {
        let t = Tuning { lead: Lead::Client, offset_hz: 1_000_000 };
        let r = 2_048_000;
        assert_eq!(
            tci_commands(&Cmd::Frequency(237_488_000), t),
            vec!["vfo:0,0,238488000;", "dds:0,238488000;"]
        );
        assert_eq!(
            tci_commands(&Cmd::SampleRate(r), t),
            vec![format!("iq_samplerate:{};", tci_rate(r, 1_000_000))]
        );
        // What SDR Oxide snaps to: 10 MS/s / 2 and 6 MS/s / 1 hold the band
        // with its own 26.8 kHz on top; 2 MS/s (the RSP1A's start) does not.
        assert!(holds(5_000_000, r, 1_026_800));
        assert!(holds(6_000_000, r, 1_026_800));
        assert!(!holds(2_000_000, r, 1_000_000));
        assert!(holds(2_000_000, r, 26_800));
    }

    /// The NCO shift, split at an odd sample: no phase jump between the blocks.
    #[test]
    fn a_tone_at_the_tuned_frequency_lands_at_zero() {
        // Centre 26.8 kHz above the client's frequency: the client's signal
        // sits at −26.8 kHz in the stream and must come out at 0 Hz.
        let rate = 2_000_000u32;
        let iq: Vec<f32> = (0..20_000)
            .flat_map(|i| {
                let ph = -std::f64::consts::TAU * 26_800.0 * i as f64 / rate as f64;
                [ph.cos() as f32, ph.sin() as f32]
            })
            .collect();
        let mut phase = 0.0;
        let (a, b) = iq.split_at(2 * 7_001);
        let mut out = shift_iq(a, -26_800.0, rate, &mut phase);
        out.extend(shift_iq(b, -26_800.0, rate, &mut phase));
        // A constant 1 + 0j: the tone stopped turning.
        for v in out.chunks(2) {
            assert!((v[0] - 1.0).abs() < 1e-3 && v[1].abs() < 1e-3, "{v:?}");
        }
    }

    /// Within tolerance → settled; old channel → wait; past 1.5 s → settled anyway.
    #[test]
    fn a_retune_waits_for_the_new_centre() {
        use std::time::Duration;
        let ms = Duration::from_millis;
        // Asked for 238.488 MHz (9A + 1 MHz offset); SDR Oxide reports 28 kHz above.
        assert!(settled(238_516_125, 238_488_000, ms(10)));
        // Still the previous channel (1.712 MHz lower): wait.
        assert!(!settled(236_804_125, 238_488_000, ms(10)));
        // But not for ever.
        assert!(settled(236_804_125, 238_488_000, ms(1600)));
    }

    /// A weak tone (−40 dBFS) after 200 blocks: mean 127.5, per-component RMS = TARGET_RMS/√2.
    #[test]
    fn agc_centres_and_levels_eight_bits() {
        let mut agc = Agc::new();
        let iq: Vec<f32> = (0..20_000).map(|i| 0.01 * ((i as f32) * 0.37).sin()).collect();
        let mut out = Vec::new();
        for _ in 0..200 {
            out.clear();
            agc.quantise(&iq, &mut out);
        }
        let mean = out.iter().map(|&b| b as f64).sum::<f64>() / out.len() as f64;
        let rms = (out.iter().map(|&b| (b as f64 - 127.5).powi(2)).sum::<f64>() / out.len() as f64)
            .sqrt()
            / 127.5;
        assert!((mean - 127.5).abs() < 1.0, "mean {mean}");
        // The pair's RMS is TARGET_RMS, so each component's is 1/√2 of it.
        let want = TARGET_RMS as f64 / std::f64::consts::SQRT_2;
        assert!((rms - want).abs() < 0.01, "rms {rms}, want {want}");
    }
}
