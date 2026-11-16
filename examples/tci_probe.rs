//! Measures a TCI server's IQ stream directly, without the bridge: subscribes
//! at a rate, then reports samples per second, the rate the frame headers
//! carry, frame sizes and the longest pause between frames.
//!
//! cargo run --release --example tci_probe -- 127.0.0.1:50001 500000 20
//!
//! Ends:     find out whether a TCI server delivers the IQ rate it was asked for, before blaming the bridge.
//! Ways:     one WebSocket client: wait for `ready;`, subscribe, skip 2 s, then count IQ frames and samples, the
//!           rates the headers carry, frame sizes and the longest gap.
//! Means:    tokio, tokio-tungstenite, futures-util; header words read by hand (no tci.rs).
//! Provides: CLI `tci_probe [host:port] [rate S/s] [seconds]` (defaults 127.0.0.1:50001, 500000, 20).
//! Expects:  a TCI server; nobody else relying on its IQ rate during the run (the subscription changes it).
//! Ensures:  prints requested rate, header rates seen, samples/s and % of requested, frames/s, frame sizes,
//!           longest pause (ms).
//! Fails:    panics on connect, on a closed socket before `ready;`, or on send errors. Leaves IQ running (no
//!           `iq_stop`).
//! Tags:     #tci #test
//! See:      src/bin/tci_diag.rs (`tci_diag iq` does the same with a summary and `iq_stop`), src/tci.rs.

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    // Positional arguments: host:port, rate (S/s), seconds.
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

    // `first` = arrival of the first IQ frame; the 2 s warm-up counts from there.
    let mut first: Option<Instant> = None;
    let mut last = Instant::now();
    let (mut samples, mut frames, mut max_gap) = (0u64, 0u64, Duration::ZERO);
    let mut header_rates = std::collections::BTreeMap::<u32, u64>::new();
    let mut sizes = std::collections::BTreeMap::<u32, u64>::new();
    let end = Instant::now() + Duration::from_secs(secs + 2);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end.into(), ws.next()).await {
        let Message::Binary(b) = msg else { continue };
        // Shorter than the 64-byte header: not a stream frame.
        if b.len() < 64 {
            continue;
        }
        // Header word i, little-endian u32 (word 1 = rate, word 6 = stream type).
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
        // Complex samples: 8 bytes each (two float32) after the header.
        let n = (b.len() as u64 - 64) / 8;
        samples += n;
        frames += 1;
        *header_rates.entry(w(1)).or_default() += 1;
        *sizes.entry(n as u32).or_default() += 1;
        max_gap = max_gap.max(now.duration_since(last));
        last = now;
    }
    // Pitfall: the window is taken as `secs`, though it ends at a deadline set
    //          before the first frame arrived; a slow start reads a little low.
    let measured = secs as f64;
    println!("requested rate        {rate}");
    println!("header sample_rate    {header_rates:?}");
    println!("IQ samples/s          {:.0}  ({:.1} % of requested)", samples as f64 / measured, 100.0 * samples as f64 / measured / rate as f64);
    println!("frames/s              {:.1}", frames as f64 / measured);
    println!("samples per frame     {:?}", sizes.iter().take(6).collect::<Vec<_>>());
    println!("longest pause         {:.1} ms", max_gap.as_secs_f64() * 1000.0);
}
