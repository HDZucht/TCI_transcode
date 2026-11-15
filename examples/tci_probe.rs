//! Measures a TCI server's IQ stream directly, without the bridge: subscribes
//! at a rate, then reports samples per second, the rate the frame headers
//! carry, frame sizes and the longest pause between frames.
//!
//! cargo run --release --example tci_probe -- 127.0.0.1:50001 500000 20

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:50001".into());
    let rate: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(500_000);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.expect("connect");

    // Wait for the status burst to end, then subscribe.
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) if t.as_str().contains("ready;") => break,
            Some(Ok(_)) => {}
            _ => panic!("no ready;"),
        }
    }
    for c in [format!("iq_samplerate:{rate};"), "iq_start:0;".into()] {
        ws.send(Message::text(c)).await.unwrap();
    }

    let mut first: Option<Instant> = None;
    let mut last = Instant::now();
    let (mut samples, mut frames, mut max_gap) = (0u64, 0u64, Duration::ZERO);
    let mut header_rates = std::collections::BTreeMap::<u32, u64>::new();
    let mut sizes = std::collections::BTreeMap::<u32, u64>::new();
    let end = Instant::now() + Duration::from_secs(secs + 2);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end.into(), ws.next()).await {
        let Message::Binary(b) = msg else { continue };
        if b.len() < 64 {
            continue;
        }
        let w = |i: usize| u32::from_le_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        if w(6) != 0 {
            continue; // not IQ
        }
        let now = Instant::now();
        // Skip the first two seconds: the server retunes its DDC on subscribe.
        let start = *first.get_or_insert(now);
        if now.duration_since(start) < Duration::from_secs(2) {
            last = now;
            continue;
        }
        let n = (b.len() as u64 - 64) / 8;
        samples += n;
        frames += 1;
        *header_rates.entry(w(1)).or_default() += 1;
        *sizes.entry(n as u32).or_default() += 1;
        max_gap = max_gap.max(now.duration_since(last));
        last = now;
    }
    let measured = secs as f64;
    println!("requested rate        {rate}");
    println!("header sample_rate    {header_rates:?}");
    println!("IQ samples/s          {:.0}  ({:.1} % of requested)", samples as f64 / measured, 100.0 * samples as f64 / measured / rate as f64);
    println!("frames/s              {:.1}", frames as f64 / measured);
    println!("samples per frame     {:?}", sizes.iter().take(6).collect::<Vec<_>>());
    println!("longest pause         {:.1} ms", max_gap.as_secs_f64() * 1000.0);
}
