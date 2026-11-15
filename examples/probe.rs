//! A small SDRconnect client for checking any SDRconnect API endpoint — the
//! bridge or a real SDRconnect: reads the properties msdr reads, then counts
//! spectrum, IQ and audio frames for a few seconds.
//!
//! cargo run --example probe -- ws://127.0.0.1:5454 [seconds]

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

const PROPS: &[&str] = &[
    "api_version",
    "started",
    "can_control",
    "active_device",
    "device_center_frequency",
    "device_vfo_frequency",
    "device_sample_rate",
    "demodulator",
    "filter_bandwidth",
    "signal_power",
    "signal_snr",
];

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "ws://127.0.0.1:5454".into());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(3);
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");

    for p in PROPS {
        let m = json!({"event_type": "get_property", "property": p, "value": ""});
        ws.send(Message::text(m.to_string())).await.unwrap();
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut answer = "(no answer)".to_string();
        while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline.into(), ws.next()).await {
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(t.as_str()).unwrap_or_default();
                if v["event_type"] == "get_property_response" && v["property"] == *p {
                    answer = v["value"].to_string();
                    break;
                }
            }
        }
        println!("{p:<24} {answer}");
    }

    for e in ["spectrum_enable", "iq_stream_enable", "audio_stream_enable"] {
        let m = json!({"event_type": e, "property": "", "value": "true"});
        ws.send(Message::text(m.to_string())).await.unwrap();
    }
    let (mut spec, mut iq_bytes, mut audio_bytes, mut events) = (0u32, 0usize, 0usize, 0u32);
    let mut last_bins: Vec<u8> = Vec::new();
    let (mut iq_sq, mut iq_n, mut iq_peak) = (0f64, 0u64, 0f64);
    let end = Instant::now() + Duration::from_secs(secs);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end.into(), ws.next()).await {
        match msg {
            Message::Binary(b) if b.len() >= 2 => match u16::from_le_bytes([b[0], b[1]]) {
                1 => audio_bytes += b.len() - 2,
                2 => {
                    iq_bytes += b.len() - 2;
                    for c in b[2..].chunks_exact(2) {
                        let v = i16::from_le_bytes([c[0], c[1]]) as f64;
                        iq_sq += v * v;
                        iq_n += 1;
                        iq_peak = iq_peak.max(v.abs());
                    }
                }
                3 => {
                    spec += 1;
                    last_bins = b[2..].to_vec();
                }
                _ => {}
            },
            Message::Text(_) => events += 1,
            _ => {}
        }
    }
    let s = secs as f64;
    println!("spectrum frames/s       {:.1}", spec as f64 / s);
    println!("IQ complex samples/s    {:.0}", iq_bytes as f64 / 4.0 / s);
    println!("audio stereo frames/s   {:.0}", audio_bytes as f64 / 4.0 / s);
    println!("text events             {events}");
    if iq_n > 0 {
        let rms = (iq_sq / iq_n as f64).sqrt();
        println!(
            "IQ int16 level          RMS {:.1} ({:.1} dBFS), peak {:.0} ({:.1} dBFS)",
            rms,
            20.0 * (rms / 32767.0).log10(),
            iq_peak,
            20.0 * (iq_peak.max(1.0) / 32767.0).log10()
        );
    }
    if !last_bins.is_empty() {
        let mut sorted = last_bins.clone();
        sorted.sort_unstable();
        let (peak_bin, peak) = last_bins.iter().enumerate().max_by_key(|(_, v)| **v).unwrap();
        println!(
            "last spectrum           {} bins, median {}, peak {} at bin {}",
            last_bins.len(),
            sorted[sorted.len() / 2],
            peak,
            peak_bin
        );
    }
}
