//! Records the IQ an SDRconnect endpoint sends (the bridge or a real
//! SDRconnect) as raw interleaved int16 (`cs16`) for N seconds, and prints the
//! centre and rate needed to read it back, e.g. with
//! `rtl_433 -r cs16:<file> -s <rate> -f <centre>`.
//!
//! cargo run --release --example dump_iq -- ws://127.0.0.1:5454 180 /tmp/x.cs16
//!
//! Ends:     capture what an SDRconnect endpoint really delivers, so a decoder can be run on it offline and the
//!           bridge compared with a real SDRconnect sample for sample.
//! Ways:     one WebSocket client: ask centre and rate, switch IQ on, append every IQ payload to a file.
//! Means:    tokio, tokio-tungstenite, futures-util, serde_json.
//! Provides: CLI `dump_iq [url] [seconds] [path]` (defaults ws://127.0.0.1:5454, 60, /tmp/iq.cs16); prints file,
//!           centre (Hz), rate (S/s) and seconds of signal.
//! Expects:  the endpoint answers `get_property` for `device_center_frequency` and `device_sample_rate`.
//! Ensures:  the file holds the int16 I,Q payloads in arrival order, without the 2-byte type.
//! Fails:    panics on connect or file errors; waits for ever if the two properties never come; centre and rate
//!           are read once at the start, so a retune during the recording is not noted.
//! Tags:     #sdrconnect #test
//! See:      examples/probe.rs, src/sdrc.rs (`PAYLOAD_IQ`).

use std::io::Write;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    // Positional arguments: url, seconds, output path.
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "ws://127.0.0.1:5454".into());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let path = args.next().unwrap_or_else(|| "/tmp/iq.cs16".into());
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");

    // Ask both properties first; the answers may come in any order.
    let get = |p: &'static str| json!({"event_type": "get_property", "property": p, "value": ""});
    for p in ["device_center_frequency", "device_sample_rate"] {
        ws.send(Message::text(get(p).to_string())).await.unwrap();
    }
    // Wait for both answers, skipping everything else (property events).
    // Pitfall: no timeout; an endpoint that never answers keeps this loop waiting.
    let (mut centre, mut rate) = (String::new(), String::new());
    while centre.is_empty() || rate.is_empty() {
        if let Some(Ok(Message::Text(t))) = ws.next().await {
            let v: Value = serde_json::from_str(t.as_str()).unwrap_or_default();
            if v["event_type"] == "get_property_response" {
                match v["property"].as_str() {
                    Some("device_center_frequency") => centre = v["value"].as_str().unwrap_or("").into(),
                    Some("device_sample_rate") => rate = v["value"].as_str().unwrap_or("").into(),
                    _ => {}
                }
            }
        }
    }
    // Only now switch IQ on, so the recording starts after centre and rate are known.
    let m = json!({"event_type": "iq_stream_enable", "property": "", "value": "true"});
    ws.send(Message::text(m.to_string())).await.unwrap();

    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create"));
    let mut bytes = 0usize;
    let end = Instant::now() + Duration::from_secs(secs);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end.into(), ws.next()).await {
        if let Message::Binary(b) = msg {
            // Payload type 2 (IQ, little-endian u16); the 2 type bytes are not written.
            if b.len() > 2 && b[..2] == [2, 0] {
                file.write_all(&b[2..]).unwrap();
                bytes += b.len() - 2;
            }
        }
    }
    file.flush().unwrap();
    println!("file {path}");
    println!("centre {centre}");
    println!("rate {rate}");
    // 4 bytes per complex sample (two int16); this tells whether frames were lost.
    println!("seconds of signal {:.1}", bytes as f64 / 4.0 / rate.parse::<f64>().unwrap_or(1.0));
}
