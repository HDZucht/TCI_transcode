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
const TUNER_R820T: u32 = 5;
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
const SETTLE_TOLERANCE_HZ: i64 = 150_000;

/// Whether a block whose stream is centred at `center_hz` belongs to the
/// retune that asked for `expected_hz` `since` ago: yes once the centre is
/// there, or once the wait is over.
pub fn settled(center_hz: i64, expected_hz: i64, since: std::time::Duration) -> bool {
    (center_hz - expected_hz).abs() <= SETTLE_TOLERANCE_HZ || since > SETTLE_LIMIT
}

/// RMS the AGC aims at, of full scale: −15 dBFS leaves OFDM's peaks room.
const TARGET_RMS: f32 = 0.178;

/// One block of interleaved float IQ as the TCI server sent it.
pub struct RawIq {
    pub rate: u32,
    /// The IQ stream's centre as the TCI server last reported it (`dds`).
    pub center_hz: i64,
    pub data: Vec<f32>,
}

/// A command from the client.
#[derive(Debug, PartialEq)]
pub enum Cmd {
    Frequency(u32),
    SampleRate(u32),
    Other(u8, u32),
}

pub fn parse_cmd(b: &[u8; 5]) -> Cmd {
    let v = u32::from_be_bytes([b[1], b[2], b[3], b[4]]);
    match b[0] {
        0x01 => Cmd::Frequency(v),
        0x02 => Cmd::SampleRate(v),
        c => Cmd::Other(c, v),
    }
}

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
    pub lead: Lead,
    pub offset_hz: i64,
}

/// The TCI rate that holds the client's band `offset_hz` off centre with
/// room: twice the offset plus the band, and a tenth more because the server
/// snaps to an integer share of its hardware rate.
pub fn tci_rate(client_rate: u32, offset_hz: i64) -> u32 {
    if offset_hz == 0 {
        return client_rate;
    }
    ((client_rate as f64 + 2.0 * offset_hz.abs() as f64) * 1.1) as u32
}

/// Whether a stream at `rate` holds the client's band at `offset_hz`: the
/// band's outer edge (80 % of the client's half rate) inside the stream's.
pub fn holds(rate: u32, client_rate: u32, offset_hz: i64) -> bool {
    offset_hz.abs() as f64 + 0.4 * client_rate as f64 + 50_000.0 <= rate as f64 / 2.0
}

/// The TCI commands a client's command turns into.
pub fn tci_commands(cmd: &Cmd, t: Tuning) -> Vec<String> {
    match *cmd {
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
    gain: Option<f32>,
}

impl Agc {
    pub fn new() -> Agc {
        Agc { gain: None }
    }

    pub fn quantise(&mut self, iq: &[f32], out: &mut Vec<u8>) {
        if iq.is_empty() {
            return;
        }
        let rms = (iq.iter().map(|v| v * v).sum::<f32>() / iq.len() as f32 * 2.0).sqrt();
        if rms > 1e-9 {
            let want = (TARGET_RMS / rms).clamp(1e-3, 1e6);
            let g = self.gain.get_or_insert(want);
            // About a second to settle at ~100 blocks a second; down fast, so a
            // strong signal clips for a block at most.
            *g += (want - *g) * if want < *g { 0.3 } else { 0.03 };
        }
        let g = self.gain.unwrap_or(1.0);
        out.extend(iq.iter().map(|&v| (v * g * 127.5 + 127.5).round().clamp(0.0, 255.0) as u8));
    }
}

impl Default for Agc {
    fn default() -> Self {
        Agc::new()
    }
}

/// Move a signal `offset_hz` above the centre down to zero: multiply by
/// e^(−j2π·offset·t), the phase carried across blocks.
pub fn shift_iq(iq: &[f32], offset_hz: f64, rate: u32, phase: &mut f64) -> Vec<f32> {
    let step = -std::f64::consts::TAU * offset_hz / rate as f64;
    let mut out = Vec::with_capacity(iq.len());
    for v in iq.chunks_exact(2) {
        let (c, s) = (phase.cos() as f32, phase.sin() as f32);
        out.push(v[0] * c - v[1] * s);
        out.push(v[0] * s + v[1] * c);
        *phase = (*phase + step) % std::f64::consts::TAU;
    }
    out
}

/// Accept rtl_tcp clients for ever.
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
                tokio::spawn(client(stream, iq.subscribe(), to_tci.clone(), tuning));
            }
            Err(e) => tracing::warn!("rtl_tcp accept failed: {e}"),
        }
    }
}

async fn client(
    stream: TcpStream,
    mut iq: broadcast::Receiver<Arc<RawIq>>,
    to_tci: tokio::sync::mpsc::UnboundedSender<String>,
    mut tuning: Tuning,
) {
    let lead = tuning.lead;
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    if wr.write_all(&header()).await.is_err() {
        return;
    }
    let mut want = DEFAULT_RATE;
    let _ = to_tci.send(tci::cmd::iq_samplerate(tci_rate(want, tuning.offset_hz)));
    // When the rate was last asked for: until the server has switched, blocks
    // still come at the old rate and say nothing about the new one.
    let mut rate_asked = std::time::Instant::now();
    let mut rs: Option<Resampler> = None;
    let mut agc = Agc::new();
    let (mut fl, mut bytes) = (Vec::new(), Vec::new());
    let mut cmd = [0u8; 5];
    let mut lead_warned = false;
    let mut lost = 0u64;
    // The frequency the client tuned to (only when it may tune), and the
    // phase of the oscillator that moves it to zero.
    let mut tuned: Option<i64> = None;
    let mut phase = 0f64;
    let mut shift_logged = i64::MIN;
    // A retune in flight: when it was asked for and the centre it should bring.
    let mut retune: Option<(std::time::Instant, i64)> = None;
    let mut muted = 0u64;
    loop {
        tokio::select! {
            r = rd.read_exact(&mut cmd) => {
                if r.is_err() {
                    break;
                }
                let c = parse_cmd(&cmd);
                tracing::debug!(?c, "rtl_tcp command");
                if let Cmd::SampleRate(r) = c && r > 0 {
                    want = r;
                    rate_asked = std::time::Instant::now();
                }
                if let Cmd::Frequency(hz) = c && lead == Lead::Client {
                    tuned = Some(hz as i64);
                    retune = Some((std::time::Instant::now(), hz as i64 + tuning.offset_hz));
                }
                if let Cmd::Frequency(hz) = c && lead == Lead::Frontend && !lead_warned {
                    tracing::warn!(hz, "rtl_tcp client tunes, but --lead frontend keeps the front end's frequency; start with --lead client to let it");
                    lead_warned = true;
                }
                for line in tci_commands(&c, tuning) {
                    let _ = to_tci.send(line);
                }
            }
            b = iq.recv() => match b {
                Ok(block) => {
                    // A stream too narrow for the offset: tune on the frequency.
                    if tuning.offset_hz != 0
                        && rate_asked.elapsed() > RATE_SWITCH_GRACE
                        && !holds(block.rate, want, tuning.offset_hz)
                    {
                        tracing::warn!(rate = block.rate, offset_hz = tuning.offset_hz, "rtl_tcp: TCI stream too narrow for offset tuning; tuning on the frequency (raise the radio's sample rate to keep its DC spike out)");
                        tuning.offset_hz = 0;
                        let _ = to_tci.send(tci::cmd::iq_samplerate(want));
                        if let Some(f) = tuned {
                            for line in tci_commands(&Cmd::Frequency(f as u32), tuning) {
                                let _ = to_tci.send(line);
                            }
                        }
                    }
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
                        _ => block.data.clone(),
                    };
                    let data = if silent { vec![0.0; data.len()] } else { data };
                    fl.clear();
                    rs.as_mut().unwrap().process(&data, &mut fl);
                    bytes.clear();
                    agc.quantise(&fl, &mut bytes);
                    if wr.write_all(&bytes).await.is_err() {
                        break;
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_rtl0_r820t() {
        let h = header();
        assert_eq!(&h[..4], b"RTL0");
        assert_eq!(u32::from_be_bytes(h[4..8].try_into().unwrap()), 5);
        assert_eq!(u32::from_be_bytes(h[8..12].try_into().unwrap()), 29);
    }

    #[test]
    fn commands_parse_big_endian() {
        assert_eq!(parse_cmd(&[0x01, 0x0d, 0x22, 0xa6, 0x00]), Cmd::Frequency(220_374_528));
        assert_eq!(parse_cmd(&[0x02, 0x00, 0x1f, 0x40, 0x00]), Cmd::SampleRate(2_048_000));
        assert_eq!(parse_cmd(&[0x04, 0, 0, 0x01, 0x2c]), Cmd::Other(4, 300));
    }

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
        for v in out.chunks(2) {
            assert!((v[0] - 1.0).abs() < 1e-3 && v[1].abs() < 1e-3, "{v:?}");
        }
    }

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
