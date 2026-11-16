//! The TCI side: what a TCI server (SDR Oxide, ExpertSDR, Thetis …) says, and
//! the few commands the bridge sends it.
//!
//! TCI is Expert Electronics' open Transceiver Control Interface: one
//! WebSocket, text commands `name:arg,arg;` in both directions, and binary
//! stream frames with a 64-byte header of sixteen little-endian u32 followed
//! by little-endian float32 samples. Written from the published protocol, not
//! from any server's source.
//!
//! Ends:     read a TCI server's status lines and stream frames, and write the handful of commands the bridge
//!           needs, without holding any connection state.
//! Ways:     pure functions: split text frames into commands, reduce each to a `Status`, parse the 64-byte binary
//!           header, decode float32 payloads; `cmd::*` format outgoing lines.
//! Means:    std only.
//! Provides: `split_commands`, `parse_status` → `Status`, `parse_header` → `Header`, `payload_f32`, `Stream`,
//!           `FORMAT_FLOAT32`, `HEADER_LEN`, `cmd::{iq_samplerate, iq_start, audio_samplerate, audio_start, vfo, dds,
//!           modulation}`; tests also `build_frame`.
//! Expects:  text as the server sends it (any case, several commands per frame, `;`-terminated); binary frames
//!           with the header first.
//! Ensures:  receiver 0 only (other receivers → `Status::Other`); names lower-cased; a short or odd frame never
//!           panics (fewer samples, or `None`).
//! Fails:    unparseable numbers turn the line into `Status::Other`, silently.
//! Tags:     #tci #radio
//! See:      main.rs `tci_link` (reader), state.rs `State::apply` (consumer), bin/tci_diag.rs (same parser for
//!           diagnostics).

/// Size of the binary stream header: sixteen little-endian u32.
pub const HEADER_LEN: usize = 64;

/// What a binary frame carries (`type`, header word 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Interleaved I,Q float32, server → client.
    Iq,
    /// Receive audio, float32, usually stereo at 48 kHz, server → client.
    RxAudio,
    /// Transmit audio, client → server. The bridge never sends it.
    TxAudio,
    /// Transmit pacing, server → client.
    TxChrono,
    /// A type id this bridge does not know; kept so it can be logged.
    Other(u32),
}

impl Stream {
    /// Map header word 6 to a stream kind (0 IQ, 1 RX audio, 2 TX audio, 3 TX chrono).
    fn from_u32(v: u32) -> Stream {
        match v {
            0 => Stream::Iq,
            1 => Stream::RxAudio,
            2 => Stream::TxAudio,
            3 => Stream::TxChrono,
            other => Stream::Other(other),
        }
    }
}

/// The header fields the bridge reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Receiver index (word 0); the bridge serves receiver 0 only.
    pub receiver: u32,
    /// Sample rate of this frame (word 1): S/s for IQ, Hz for audio. The truth
    /// about the stream, even right after an `iq_samplerate` request.
    pub sample_rate: u32,
    /// Sample format id; 3 is float32, the only one TCI servers send in practice.
    pub format: u32,
    /// Number of float values in the payload (all channels together).
    pub length: u32,
    /// Stream kind (word 6).
    pub stream: Stream,
    /// Channel count; 2 for IQ and for stereo audio, 0 on servers that leave it unset.
    pub channels: u32,
}

/// The float32 format id.
pub const FORMAT_FLOAT32: u32 = 3;

/// Little-endian u32 number `i` of a frame header. Panics if the frame is
/// shorter than `4·(i+1)` bytes; callers check `HEADER_LEN` first.
fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
}

/// Parse the header at the start of a binary frame.
/// `None` when the frame is shorter than `HEADER_LEN` (64 bytes).
pub fn parse_header(frame: &[u8]) -> Option<Header> {
    if frame.len() < HEADER_LEN {
        return None;
    }
    // Words 3, 4 and 8–15 are not read: the bridge needs none of them.
    Some(Header {
        receiver: word(frame, 0),
        sample_rate: word(frame, 1),
        format: word(frame, 2),
        length: word(frame, 5),
        stream: Stream::from_u32(word(frame, 6)),
        channels: word(frame, 7),
    })
}

/// The float32 samples after the header. Trusts the bytes present rather than
/// `length`, so a short frame yields fewer samples instead of a panic.
///
/// Clears `out` first and reuses its allocation (the caller keeps one buffer
/// for all frames). Trailing bytes that do not make a whole float are dropped.
pub fn payload_f32(frame: &[u8], out: &mut Vec<f32>) {
    out.clear();
    if frame.len() <= HEADER_LEN {
        return;
    }
    out.extend(
        frame[HEADER_LEN..]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
    );
}

/// One status line from the server, reduced to what the bridge keeps.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// `protocol:<name>,<version>;` — arguments joined back with commas.
    Protocol(String),
    /// `device:<name>;` — the radio's name as the server reports it.
    Device(String),
    /// Hardware centre frequency of receiver 0.
    Dds(i64),
    /// Absolute VFO frequency of receiver 0, channel `ch`.
    Vfo { ch: u32, hz: i64 },
    /// VFO offset from the centre, receiver 0, channel `ch`.
    If { ch: u32, hz: i64 },
    /// Modulation of receiver 0, lower case (`am`, `nfm`, `usb` …).
    Modulation(String),
    /// Receive filter edges relative to the VFO, receiver 0.
    RxFilter { lo: i64, hi: i64 },
    /// IQ rate the server confirms, S/s (not per receiver in TCI).
    IqRate(u32),
    /// End of the status burst after connecting.
    Ready,
    /// The server started receiving (`start;`).
    Start,
    /// The server stopped (`stop;`); also synthesised by the bridge when the link drops.
    Stop,
    /// Anything else, or a line about another receiver.
    Other,
}

/// Split one text frame into its `;`-terminated commands, each as
/// (lower-case name, comma-separated arguments).
///
/// Whitespace around names and arguments is trimmed; empty pieces (e.g. after
/// the last `;`) are skipped; a command without `:` has no arguments.
/// Arguments keep their case (a device name is a name).
pub fn split_commands(text: &str) -> Vec<(String, Vec<String>)> {
    text.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once(':') {
            Some((name, args)) => (
                name.trim().to_lowercase(),
                args.split(',').map(|a| a.trim().to_string()).collect(),
            ),
            None => (p.to_lowercase(), Vec::new()),
        })
        .collect()
}

/// Parse an optional argument as a number; `None` if absent or not a number.
fn num<T: std::str::FromStr>(s: Option<&String>) -> Option<T> {
    s?.parse().ok()
}

/// Interpret one command from the server. Lines about receivers other than 0
/// come back as [`Status::Other`]: the bridge serves one receiver.
///
/// A known command with missing or non-numeric arguments also becomes `Other`.
pub fn parse_status(name: &str, args: &[String]) -> Status {
    // First argument "0" = receiver 0, for the per-receiver commands.
    let rx0 = || args.first().map(String::as_str) == Some("0");
    let parsed = match name {
        "protocol" => Some(Status::Protocol(args.join(","))),
        "device" => Some(Status::Device(args.join(","))),
        // dds:<rx>,<hz>;
        "dds" if rx0() => num(args.get(1)).map(Status::Dds),
        // vfo:<rx>,<channel>,<hz>;
        "vfo" if rx0() => match (num(args.get(1)), num(args.get(2))) {
            (Some(ch), Some(hz)) => Some(Status::Vfo { ch, hz }),
            _ => None,
        },
        // if:<rx>,<channel>,<offset hz>;
        "if" if rx0() => match (num(args.get(1)), num(args.get(2))) {
            (Some(ch), Some(hz)) => Some(Status::If { ch, hz }),
            _ => None,
        },
        "modulation" if rx0() => args.get(1).map(|m| Status::Modulation(m.to_lowercase())),
        // rx_filter_band:<rx>,<low hz>,<high hz>;
        "rx_filter_band" if rx0() => match (num(args.get(1)), num(args.get(2))) {
            (Some(lo), Some(hi)) => Some(Status::RxFilter { lo, hi }),
            _ => None,
        },
        // iq_samplerate:<rate>; carries no receiver index.
        "iq_samplerate" => num(args.first()).map(Status::IqRate),
        "ready" => Some(Status::Ready),
        "start" => Some(Status::Start),
        "stop" => Some(Status::Stop),
        _ => None,
    };
    parsed.unwrap_or(Status::Other)
}

/// Commands the bridge sends. Receiver 0, channel 0 (VFO A) throughout.
/// Each returns one complete `;`-terminated line, ready for a text frame.
pub mod cmd {
    /// Ask for an IQ rate in S/s. The server may snap it to an integer
    /// decimation of its hardware rate (SDR Oxide: 2.048 M → 2.000 M); the
    /// frame headers carry what it actually sends.
    pub fn iq_samplerate(hz: u32) -> String {
        format!("iq_samplerate:{hz};")
    }
    /// Start the IQ stream of receiver 0.
    pub fn iq_start() -> String {
        "iq_start:0;".into()
    }
    /// Ask for an audio rate in Hz (the bridge asks for 48 kHz).
    pub fn audio_samplerate(hz: u32) -> String {
        format!("audio_samplerate:{hz};")
    }
    /// Start the receive audio stream of receiver 0.
    pub fn audio_start() -> String {
        "audio_start:0;".into()
    }
    /// Set VFO A of receiver 0 to `hz` (absolute, Hz).
    pub fn vfo(hz: i64) -> String {
        format!("vfo:0,0,{hz};")
    }
    /// Set the hardware centre (DDS) of receiver 0 to `hz`. The server reports
    /// the centre it really took with a `dds` line; SDR Oxide put it 26.8–28.1
    /// kHz beside the value asked for (measured 2026-11-16).
    pub fn dds(hz: i64) -> String {
        format!("dds:0,{hz};")
    }
    /// Set the modulation of receiver 0 (`am`, `nfm`, `usb` …, lower case).
    pub fn modulation(m: &str) -> String {
        format!("modulation:0,{m};")
    }
}

/// Build a binary frame the way a TCI server does (tests only).
/// Receiver 0, float32, `samples.len()` as the length word.
#[cfg(test)]
pub fn build_frame(stream: u32, rate: u32, channels: u32, samples: &[f32]) -> Vec<u8> {
    let mut words = [0u32; 16];
    words[1] = rate;
    words[2] = FORMAT_FLOAT32;
    words[5] = samples.len() as u32;
    words[6] = stream;
    words[7] = channels;
    let mut out = Vec::with_capacity(HEADER_LEN + samples.len() * 4);
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Proves: a built frame parses back to its header and samples; a 63-byte frame
/// has no header; a status burst splits and reads in order; other receivers are ignored.
#[cfg(test)]
mod tests {
    use super::*;

    /// build_frame → parse_header / payload_f32 gives back every field and sample.
    #[test]
    fn header_and_payload_round_trip() {
        let f = build_frame(0, 250_000, 2, &[0.5, -0.25, 1.0, 0.0]);
        let h = parse_header(&f).unwrap();
        assert_eq!(h.stream, Stream::Iq);
        assert_eq!(h.sample_rate, 250_000);
        assert_eq!(h.format, FORMAT_FLOAT32);
        assert_eq!(h.length, 4);
        assert_eq!(h.channels, 2);
        let mut s = Vec::new();
        payload_f32(&f, &mut s);
        assert_eq!(s, vec![0.5, -0.25, 1.0, 0.0]);
    }

    /// One byte short of a header: `None`, no panic.
    #[test]
    fn short_frame_has_no_header() {
        assert!(parse_header(&[0u8; 63]).is_none());
    }

    /// An ExpertSDR-style burst, one text frame, read in order; the mode is lower-cased.
    #[test]
    fn status_burst_is_split_and_read() {
        let burst = "protocol:ExpertSDR3,1.9;device:SunSDR2DX;dds:0,433900000;\
                     vfo:0,0,433920000;if:0,0,20000;modulation:0,NFM;iq_samplerate:250000;ready;";
        let got: Vec<Status> =
            split_commands(burst).iter().map(|(n, a)| parse_status(n, a)).collect();
        assert_eq!(
            got,
            vec![
                Status::Protocol("ExpertSDR3,1.9".into()),
                Status::Device("SunSDR2DX".into()),
                Status::Dds(433_900_000),
                Status::Vfo { ch: 0, hz: 433_920_000 },
                Status::If { ch: 0, hz: 20_000 },
                Status::Modulation("nfm".into()),
                Status::IqRate(250_000),
                Status::Ready,
            ]
        );
    }

    /// Receiver 1 lines must not move receiver 0's state.
    #[test]
    fn other_receivers_are_ignored() {
        assert_eq!(parse_status("dds", &["1".into(), "7000000".into()]), Status::Other);
        assert_eq!(parse_status("modulation", &["1".into(), "usb".into()]), Status::Other);
    }
}
