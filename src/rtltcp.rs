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
//! The IQ is resampled to exactly the rate the client asked for (a TCI server
//! snaps to an integer decimation of its hardware rate) and scaled by a slow
//! AGC to 8 bits, RMS at −15 dBFS, the gain held constant within a block.
//
// Why: HDZ 2026-11-16: "SDRconnect und rtl_tcp wäre schon mehr als genug, da
//      die meisten second party libs das sprechen"; first client AbracaDABra
//      for DAB on SDR Oxide's receivers.

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
/// RMS the AGC aims at, of full scale: −15 dBFS leaves OFDM's peaks room.
const TARGET_RMS: f32 = 0.178;

/// One block of interleaved float IQ as the TCI server sent it.
pub struct RawIq {
    pub rate: u32,
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

/// The TCI commands a client's command turns into, given who may retune.
pub fn tci_commands(cmd: &Cmd, lead: Lead) -> Vec<String> {
    match *cmd {
        Cmd::SampleRate(r) if r > 0 => vec![tci::cmd::iq_samplerate(r)],
        // The IQ stream's centre is the TCI server's DDS; the VFO goes along
        // so the operator hears where the client looks.
        Cmd::Frequency(hz) if lead == Lead::Client => {
            vec![tci::cmd::vfo(hz as i64), tci::cmd::dds(hz as i64)]
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

/// Accept rtl_tcp clients for ever.
pub async fn serve(
    listener: TcpListener,
    iq: broadcast::Sender<Arc<RawIq>>,
    to_tci: tokio::sync::mpsc::UnboundedSender<String>,
    lead: Lead,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tracing::info!(%peer, "rtl_tcp client connected");
                tokio::spawn(client(stream, iq.subscribe(), to_tci.clone(), lead));
            }
            Err(e) => tracing::warn!("rtl_tcp accept failed: {e}"),
        }
    }
}

async fn client(
    stream: TcpStream,
    mut iq: broadcast::Receiver<Arc<RawIq>>,
    to_tci: tokio::sync::mpsc::UnboundedSender<String>,
    lead: Lead,
) {
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    if wr.write_all(&header()).await.is_err() {
        return;
    }
    let mut want = DEFAULT_RATE;
    let _ = to_tci.send(tci::cmd::iq_samplerate(want));
    let mut rs: Option<Resampler> = None;
    let mut agc = Agc::new();
    let (mut fl, mut bytes) = (Vec::new(), Vec::new());
    let mut cmd = [0u8; 5];
    let mut lead_warned = false;
    let mut lost = 0u64;
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
                }
                if let Cmd::Frequency(hz) = c && lead == Lead::Frontend && !lead_warned {
                    tracing::warn!(hz, "rtl_tcp client tunes, but --lead frontend keeps the front end's frequency; start with --lead client to let it");
                    lead_warned = true;
                }
                for line in tci_commands(&c, lead) {
                    let _ = to_tci.send(line);
                }
            }
            b = iq.recv() => match b {
                Ok(block) => {
                    if rs.as_ref().map(|r| r.rates()) != Some((block.rate, want)) {
                        tracing::info!(from = block.rate, to = want, "rtl_tcp IQ rate");
                        rs = Some(Resampler::new(block.rate, want));
                    }
                    fl.clear();
                    rs.as_mut().unwrap().process(&block.data, &mut fl);
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
        assert_eq!(tci_commands(&f, Lead::Client), vec!["vfo:0,0,220352000;", "dds:0,220352000;"]);
        assert!(tci_commands(&f, Lead::Frontend).is_empty());
        assert_eq!(
            tci_commands(&Cmd::SampleRate(2_048_000), Lead::Frontend),
            vec!["iq_samplerate:2048000;"]
        );
        assert!(tci_commands(&Cmd::Other(3, 1), Lead::Client).is_empty());
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
