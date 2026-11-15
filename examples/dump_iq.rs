//! Records the IQ an SDRconnect endpoint sends (the bridge or a real
//! SDRconnect) as raw interleaved int16 (`cs16`) for N seconds, and prints the
//! centre and rate needed to read it back, e.g. with
//! `rtl_433 -r cs16:<file> -s <rate> -f <centre>`.
//!
//! cargo run --release --example dump_iq -- ws://127.0.0.1:5454 180 /tmp/x.cs16

use std::io::Write;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| "ws://127.0.0.1:5454".into());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let path = args.next().unwrap_or_else(|| "/tmp/iq.cs16".into());
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");

    let get = |p: &'static str| json!({"event_type": "get_property", "property": p, "value": ""});
    for p in ["device_center_frequency", "device_sample_rate"] {
        ws.send(Message::text(get(p).to_string())).await.unwrap();
    }
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
    let m = json!({"event_type": "iq_stream_enable", "property": "", "value": "true"});
    ws.send(Message::text(m.to_string())).await.unwrap();

    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).expect("create"));
    let mut bytes = 0usize;
    let end = Instant::now() + Duration::from_secs(secs);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end.into(), ws.next()).await {
        if let Message::Binary(b) = msg {
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
    println!("seconds of signal {:.1}", bytes as f64 / 4.0 / rate.parse::<f64>().unwrap_or(1.0));
}
