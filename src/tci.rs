//! The TCI side: what a TCI server (SDR Oxide, ExpertSDR, Thetis …) says, and
//! the few commands the bridge sends it.
//!
//! TCI is Expert Electronics' open Transceiver Control Interface: one
//! WebSocket, text commands `name:arg,arg;` in both directions, and binary
//! stream frames with a 64-byte header of sixteen little-endian u32 followed
//! by little-endian float32 samples. Written from the published protocol, not
//! from any server's source.

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
    Other(u32),
}

impl Stream {
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
    pub receiver: u32,
    pub sample_rate: u32,
    /// Sample format id; 3 is float32, the only one TCI servers send in practice.
    pub format: u32,
    /// Number of float values in the payload (all channels together).
    pub length: u32,
    pub stream: Stream,
    /// Channel count; 2 for IQ and for stereo audio, 0 on servers that leave it unset.
    pub channels: u32,
}

/// The float32 format id.
pub const FORMAT_FLOAT32: u32 = 3;

fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]])
}

/// Parse the header at the start of a binary frame.
pub fn parse_header(frame: &[u8]) -> Option<Header> {
    if frame.len() < HEADER_LEN {
        return None;
    }
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
    Protocol(String),
    Device(String),
    /// Hardware centre frequency of receiver 0.
    Dds(i64),
    /// Absolute VFO frequency of receiver 0, channel `ch`.
    Vfo { ch: u32, hz: i64 },
    /// VFO offset from the centre, receiver 0, channel `ch`.
    If { ch: u32, hz: i64 },
    Modulation(String),
    /// Receive filter edges relative to the VFO, receiver 0.
    RxFilter { lo: i64, hi: i64 },
    IqRate(u32),
    /// End of the status burst after connecting.
    Ready,
    Start,
    Stop,
    /// Anything else, or a line about another receiver.
    Other,
}

/// Split one text frame into its `;`-terminated commands, each as
/// (lower-case name, comma-separated arguments).
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

fn num<T: std::str::FromStr>(s: Option<&String>) -> Option<T> {
    s?.parse().ok()
}

/// Interpret one command from the server. Lines about receivers other than 0
/// come back as [`Status::Other`]: the bridge serves one receiver.
pub fn parse_status(name: &str, args: &[String]) -> Status {
    let rx0 = || args.first().map(String::as_str) == Some("0");
    let parsed = match name {
        "protocol" => Some(Status::Protocol(args.join(","))),
        "device" => Some(Status::Device(args.join(","))),
        "dds" if rx0() => num(args.get(1)).map(Status::Dds),
        "vfo" if rx0() => match (num(args.get(1)), num(args.get(2))) {
            (Some(ch), Some(hz)) => Some(Status::Vfo { ch, hz }),
            _ => None,
        },
        "if" if rx0() => match (num(args.get(1)), num(args.get(2))) {
            (Some(ch), Some(hz)) => Some(Status::If { ch, hz }),
            _ => None,
        },
        "modulation" if rx0() => args.get(1).map(|m| Status::Modulation(m.to_lowercase())),
        "rx_filter_band" if rx0() => match (num(args.get(1)), num(args.get(2))) {
            (Some(lo), Some(hi)) => Some(Status::RxFilter { lo, hi }),
            _ => None,
        },
        "iq_samplerate" => num(args.first()).map(Status::IqRate),
        "ready" => Some(Status::Ready),
        "start" => Some(Status::Start),
        "stop" => Some(Status::Stop),
        _ => None,
    };
    parsed.unwrap_or(Status::Other)
}

/// Commands the bridge sends. Receiver 0, channel 0 (VFO A) throughout.
pub mod cmd {
    pub fn iq_samplerate(hz: u32) -> String {
        format!("iq_samplerate:{hz};")
    }
    pub fn iq_start() -> String {
        "iq_start:0;".into()
    }
    pub fn audio_samplerate(hz: u32) -> String {
        format!("audio_samplerate:{hz};")
    }
    pub fn audio_start() -> String {
        "audio_start:0;".into()
    }
    pub fn vfo(hz: i64) -> String {
        format!("vfo:0,0,{hz};")
    }
    pub fn dds(hz: i64) -> String {
        format!("dds:0,{hz};")
    }
    pub fn modulation(m: &str) -> String {
        format!("modulation:0,{m};")
    }
}

/// Build a binary frame the way a TCI server does (tests only).
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn short_frame_has_no_header() {
        assert!(parse_header(&[0u8; 63]).is_none());
    }

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

    #[test]
    fn other_receivers_are_ignored() {
        assert_eq!(parse_status("dds", &["1".into(), "7000000".into()]), Status::Other);
        assert_eq!(parse_status("modulation", &["1".into(), "usb".into()]), Status::Other);
    }
}
