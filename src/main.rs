//! TCI_transcode: a TCI receiver (SDR Oxide, ExpertSDR, Thetis …) offered as the
//! SDRconnect WebSocket API 1.0.3, so programs written for that API — msdr and
//! the decoders it runs, J. van Katwijk's modules, TandemSDR — work with it.
//!
//! ```text
//! TCI server ──ws──► tci_link ──► State ──► property_changed ──► every client
//!                       │  IQ ─► int16 frames ─► clients with IQ on
//!                       │  IQ ─► Spectrum ─► uint8 frames, signal power/S/N
//!                       │  audio ─► PCM16 stereo ─► clients with audio on
//!                       ◄── TCI commands ◄── sdrc::handle ◄── client text
//! ```
//!
//! A slow client loses frames rather than holding up the others: binary
//! streams go through broadcast channels that drop on lag.

mod chunk;
mod sdrc;
mod spectrum;
mod state;
mod tci;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::{Bytes, Message};

use crate::state::{Lead, State};

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// TCI server, host:port.
    #[arg(long, default_value = "127.0.0.1:50001")]
    tci: String,
    /// Where SDRconnect clients connect, host:port (SDRconnect's own default is 5454).
    #[arg(long, default_value = "127.0.0.1:5454")]
    listen: String,
    /// IQ rate to ask the TCI server for until a client sets device_sample_rate.
    #[arg(long, default_value_t = 250_000)]
    iq_rate: u32,
    /// Who may retune: the operator in the front end, or the clients.
    #[arg(long, value_enum, default_value_t = Lead::Frontend)]
    lead: Lead,
}

struct Bridge {
    state: Mutex<State>,
    /// property_changed lines for every client.
    events: broadcast::Sender<String>,
    iq: broadcast::Sender<Bytes>,
    audio: broadcast::Sender<Bytes>,
    spectrum: broadcast::Sender<Bytes>,
    /// Commands for the TCI server.
    to_tci: mpsc::UnboundedSender<String>,
}

impl Bridge {
    fn announce(&self, props: &[&str]) {
        let st = self.state.lock().unwrap();
        for p in props {
            if let Some(line) = sdrc::property_changed(&st, p) {
                let _ = self.events.send(line);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tci_transcode=info".into()),
        )
        .init();
    let args = Args::parse();
    let (bridge, from_bridge) = new_bridge(args.lead);
    tokio::spawn(tci_link(bridge.clone(), args.tci.clone(), args.iq_rate, from_bridge));

    let listener = match TcpListener::bind(&args.listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cannot listen on {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    tracing::info!(listen = %args.listen, tci = %args.tci, lead = ?args.lead, "TCI_transcode up");
    serve(listener, bridge).await;
}

fn new_bridge(lead: Lead) -> (Arc<Bridge>, mpsc::UnboundedReceiver<String>) {
    let (to_tci, from_bridge) = mpsc::unbounded_channel();
    let bridge = Arc::new(Bridge {
        state: Mutex::new(State::new(lead)),
        events: broadcast::channel(256).0,
        iq: broadcast::channel(64).0,
        audio: broadcast::channel(64).0,
        spectrum: broadcast::channel(16).0,
        to_tci,
    });
    (bridge, from_bridge)
}

/// Accept SDRconnect clients for ever.
async fn serve(listener: TcpListener, bridge: Arc<Bridge>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tracing::info!(%peer, "client connected");
                tokio::spawn(client(bridge.clone(), stream));
            }
            Err(e) => tracing::warn!("accept failed: {e}"),
        }
    }
}

/// Keep a connection to the TCI server, reconnecting with backoff.
async fn tci_link(
    bridge: Arc<Bridge>,
    addr: String,
    iq_rate: u32,
    mut commands: mpsc::UnboundedReceiver<String>,
) {
    let url = format!("ws://{addr}");
    let mut backoff = Duration::from_millis(500);
    let mut spec = spectrum::Spectrum::new();
    let mut samples = Vec::new();
    // The IQ rate to subscribe at: the command line's, until a client sets one.
    let mut want_rate = iq_rate;
    let mut iq_chunks = chunk::Chunker::new(2);
    let mut audio_chunks = chunk::Chunker::new(2);
    let mut counts = [0u32; 2];
    let mut format_warned = false;
    loop {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => {
                tracing::info!(%url, "TCI connected");
                backoff = Duration::from_millis(500);
                let (mut tx, mut rx) = ws.split();
                let mut last_metrics = Instant::now();
                loop {
                    tokio::select! {
                        msg = rx.next() => match msg {
                            Some(Ok(Message::Text(t))) => {
                                tracing::debug!(line = %t.as_str(), "from TCI");
                                for (name, args) in tci::split_commands(t.as_str()) {
                                    let status = tci::parse_status(&name, &args);
                                    if status == tci::Status::Ready {
                                        // Subscribe once the server's status burst is over, at
                                        // the rate wanted (the server reports its own before).
                                        let rate = want_rate;
                                        for c in [
                                            tci::cmd::iq_samplerate(rate),
                                            tci::cmd::audio_samplerate(48_000),
                                            tci::cmd::iq_start(),
                                            tci::cmd::audio_start(),
                                        ] {
                                            tracing::debug!(command = %c, "to TCI");
                                            let _ = tx.send(Message::text(c)).await;
                                        }
                                    }
                                    let changed = bridge.state.lock().unwrap().apply(&status);
                                    bridge.announce(&changed);
                                }
                            }
                            Some(Ok(Message::Binary(b))) => {
                                let Some(h) = tci::parse_header(&b) else { continue };
                                if h.receiver != 0 {
                                    continue;
                                }
                                if h.format != tci::FORMAT_FLOAT32 {
                                    if !format_warned {
                                        tracing::warn!(format = h.format, "TCI frame not float32, skipped");
                                        format_warned = true;
                                    }
                                    continue;
                                }
                                if let tci::Stream::Iq | tci::Stream::RxAudio = h.stream {
                                    let i = if h.stream == tci::Stream::Iq { 0 } else { 1 };
                                    counts[i] += 1;
                                    if counts[0] + counts[1] == 1 || (counts[i] % 500 == 0) {
                                        tracing::debug!(iq_frames = counts[0], audio_frames = counts[1], rate = h.sample_rate, "TCI streams");
                                    }
                                }
                                tci::payload_f32(&b, &mut samples);
                                match h.stream {
                                    tci::Stream::Iq => {
                                        if let Some(frame) = iq_chunks.push(&samples, h.sample_rate) {
                                            let _ = bridge.iq.send(Bytes::from(sdrc::iq_frame(&frame)));
                                        }
                                        if let Some(db) = spec.feed(&samples, h.sample_rate) {
                                            publish_spectrum(&bridge, &db, h.sample_rate, &mut last_metrics);
                                        }
                                    }
                                    tci::Stream::RxAudio => {
                                        let ch = if h.channels == 0 { 2 } else { h.channels };
                                        let stereo: Vec<f32> = if ch == 1 {
                                            samples.iter().flat_map(|&s| [s, s]).collect()
                                        } else {
                                            samples.clone()
                                        };
                                        if let Some(frame) = audio_chunks.push(&stereo, h.sample_rate) {
                                            let _ = bridge.audio.send(Bytes::from(sdrc::audio_frame(&frame, 2)));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Some(Ok(_)) => {}
                            Some(Err(e)) => { tracing::warn!("TCI read: {e}"); break; }
                            None => break,
                        },
                        Some(c) = commands.recv() => {
                            tracing::debug!(command = %c, "to TCI");
                            if let Some(r) = c.strip_prefix("iq_samplerate:").and_then(|r| r.trim_end_matches(';').parse().ok()) {
                                want_rate = r;
                            }
                            if tx.send(Message::text(c)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                tracing::warn!("TCI connection lost");
                let changed = bridge.state.lock().unwrap().apply(&tci::Status::Stop);
                bridge.announce(&changed);
            }
            Err(e) => tracing::warn!(%url, "TCI connect failed: {e}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// Send a spectrum frame, and once a second refresh signal power and S/N.
fn publish_spectrum(bridge: &Bridge, db: &[f32], rate: u32, last_metrics: &mut Instant) {
    let (bytes, metrics_due) = {
        let mut st = bridge.state.lock().unwrap();
        let base = st.local_f32("spectrum_base").unwrap_or(-140.0);
        let reference = st.local_f32("spectrum_ref_level").unwrap_or(-20.0);
        let due = last_metrics.elapsed() >= Duration::from_secs(1);
        if due {
            let offset = (st.vfo_hz - st.center_hz) as f64;
            let (p, snr) = spectrum::channel_metrics(db, rate, offset, st.bandwidth_hz());
            st.signal_power_db = p;
            st.signal_snr_db = snr;
        }
        (spectrum::to_bytes(db, base, reference), due)
    };
    let _ = bridge.spectrum.send(Bytes::from(sdrc::frame(sdrc::PAYLOAD_SPECTRUM, &bytes)));
    if metrics_due {
        *last_metrics = Instant::now();
        bridge.announce(&["signal_power", "signal_snr"]);
    }
}

/// One SDRconnect client connection. msdr opens several (control, IQ, audio),
/// each with its own stream switches.
async fn client(bridge: Arc<Bridge>, stream: TcpStream) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!("WebSocket handshake failed: {e}");
            return;
        }
    };
    let (mut tx, mut rx) = ws.split();
    let mut events = bridge.events.subscribe();
    let mut iq = bridge.iq.subscribe();
    let mut audio = bridge.audio.subscribe();
    let mut spec = bridge.spectrum.subscribe();
    let mut streams = sdrc::Streams::default();

    // A lagging receiver lost frames; that is the intended back-pressure.
    macro_rules! forward {
        ($r:expr, $on:expr) => {
            match $r {
                Ok(b) => {
                    if $on && tx.send(Message::binary(b)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        };
    }

    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    let out = {
                        let mut st = bridge.state.lock().unwrap();
                        sdrc::handle(&mut st, &mut streams, t.as_str())
                    };
                    for line in out.reply {
                        if tx.send(Message::text(line)).await.is_err() {
                            return;
                        }
                    }
                    for line in out.broadcast {
                        let _ = bridge.events.send(line);
                    }
                    for c in out.tci {
                        let _ = bridge.to_tci.send(c);
                    }
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => { tracing::debug!("client read: {e}"); break; }
            },
            ev = events.recv() => match ev {
                Ok(line) => {
                    if tx.send(Message::text(line)).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!(n, "client missed property events"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            r = iq.recv() => forward!(r, streams.iq),
            r = audio.recv() => forward!(r, streams.audio),
            r = spec.recv() => forward!(r, streams.spectrum),
        }
    }
    tracing::info!("client gone");
}

/// End to end without hardware: a fake TCI server with a tone 20 kHz above
/// the centre, the bridge, and an SDRconnect client.
#[cfg(test)]
mod e2e {
    use super::*;
    use serde_json::Value;
    use tokio::time::timeout;

    const CENTER: i64 = 433_900_000;
    const TONE_OFFSET: f64 = 20_000.0;

    /// Accepts one TCI client, sends the status burst, echoes tuning, and
    /// streams the tone once IQ is started. Every command received is kept.
    async fn fake_tci(listener: TcpListener, seen: Arc<Mutex<Vec<String>>>) {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        let (mut tx, mut rx) = ws.split();
        tx.send(Message::text(format!(
            "protocol:Fake,1.9;device:FakeRig;dds:0,{CENTER};vfo:0,0,{};modulation:0,nfm;ready;",
            CENTER + TONE_OFFSET as i64
        )))
        .await
        .unwrap();
        let mut rate = 0u32;
        let mut streaming = false;
        let mut phase = 0f64;
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        loop {
            tokio::select! {
                msg = rx.next() => {
                    let Some(Ok(Message::Text(t))) = msg else { return };
                    for (name, args) in tci::split_commands(t.as_str()) {
                        seen.lock().unwrap().push(format!("{name}:{}", args.join(",")));
                        match name.as_str() {
                            "iq_samplerate" => {
                                rate = args[0].parse().unwrap();
                                tx.send(Message::text(format!("iq_samplerate:{rate};"))).await.unwrap();
                            }
                            "iq_start" => streaming = true,
                            "vfo" => {
                                tx.send(Message::text(format!("vfo:0,0,{};", args[2]))).await.unwrap();
                            }
                            _ => {}
                        }
                    }
                }
                _ = tick.tick(), if streaming && rate > 0 => {
                    let n = rate as usize / 200;
                    let step = 2.0 * std::f64::consts::PI * TONE_OFFSET / rate as f64;
                    let iq: Vec<f32> = (0..n)
                        .flat_map(|_| {
                            phase += step;
                            [0.5 * phase.cos() as f32, 0.5 * phase.sin() as f32]
                        })
                        .collect();
                    let frame = tci::build_frame(0, rate, 2, &iq);
                    if tx.send(Message::binary(frame)).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

    async fn send(ws: &mut Ws, event: &str, prop: &str, value: &str) {
        let m = serde_json::json!({"event_type": event, "property": prop, "value": value});
        ws.send(Message::text(m.to_string())).await.unwrap();
    }

    /// Next text frame matching `event` and `prop`, skipping everything else.
    async fn expect(ws: &mut Ws, event: &str, prop: &str) -> String {
        loop {
            let msg = ws.next().await.unwrap().unwrap();
            if let Message::Text(t) = msg {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["event_type"] == event && v["property"] == prop {
                    return v["value"].as_str().unwrap().to_string();
                }
            }
        }
    }

    async fn get(ws: &mut Ws, prop: &str) -> String {
        send(ws, "get_property", prop, "").await;
        expect(ws, "get_property_response", prop).await
    }

    async fn next_binary(ws: &mut Ws, payload_type: u16) -> Vec<u8> {
        loop {
            if let Message::Binary(b) = ws.next().await.unwrap().unwrap() {
                if b[..2] == payload_type.to_le_bytes() {
                    return b.to_vec();
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tone_tuning_and_streams_cross_the_bridge() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tci_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tci_addr = tci_listener.local_addr().unwrap().to_string();
        tokio::spawn(fake_tci(tci_listener, seen.clone()));

        let (bridge, from_bridge) = new_bridge(Lead::Client);
        tokio::spawn(tci_link(bridge.clone(), tci_addr, 250_000, from_bridge));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(serve(listener, bridge));

        timeout(Duration::from_secs(10), async {
            let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
            while get(&mut ws, "device_sample_rate").await != "250000" {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(get(&mut ws, "started").await, "true");
            assert_eq!(get(&mut ws, "device_center_frequency").await, CENTER.to_string());
            assert_eq!(get(&mut ws, "demodulator").await, "NFM");
            assert_eq!(get(&mut ws, "api_version").await, "1.0.3");
            assert_eq!(get(&mut ws, "active_device").await, "TCI_transcode (FakeRig)");

            // Spectrum: 512 bins, the tone 20 kHz above the centre. The tone
            // (-6 dBFS) sits above the default top of -20 dBFS and would clip
            // its neighbours to 255 as well, so the display range goes to 0.
            send(&mut ws, "set_property", "spectrum_ref_level", "0").await;
            assert_eq!(expect(&mut ws, "property_changed", "spectrum_ref_level").await, "0");
            send(&mut ws, "spectrum_enable", "", "true").await;
            let spec = next_binary(&mut ws, sdrc::PAYLOAD_SPECTRUM).await;
            let bins = &spec[2..];
            assert_eq!(bins.len(), spectrum::BINS);
            let peak = bins.iter().enumerate().max_by_key(|(_, v)| **v).unwrap().0;
            let expected = 256.0 + TONE_OFFSET / (250_000.0 / 512.0);
            assert!((peak as f64 - expected).abs() <= 1.0, "peak bin {peak}, expected {expected:.1}");

            // IQ: int16 frames.
            send(&mut ws, "iq_stream_enable", "", "true").await;
            let iq = next_binary(&mut ws, sdrc::PAYLOAD_IQ).await;
            assert!(iq.len() > 2 && (iq.len() - 2) % 4 == 0);

            // Tuning with the client leading: forwarded, echoed, announced.
            send(&mut ws, "set_property", "device_vfo_frequency", "433950000").await;
            assert_eq!(expect(&mut ws, "property_changed", "device_vfo_frequency").await, "433950000");
        })
        .await
        .expect("end-to-end run timed out");

        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"iq_samplerate:250000".to_string()), "{seen:?}");
        assert!(seen.contains(&"vfo:0,0,433950000".to_string()), "{seen:?}");
    }
}
