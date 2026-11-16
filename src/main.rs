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
//!
//! Beside it, the same IQ as an rtl_tcp server (`--rtltcp`, port 1234 as the
//! reference rtl_tcp): resampled to the client's rate, unsigned 8 bit.
//!
//! Ends:     run one bridge process that keeps a TCI link alive and serves its receiver to any number of
//!           SDRconnect and rtl_tcp clients at once.
//! Ways:     one task `tci_link` (reads TCI, fans out on broadcast channels, writes commands from one mpsc
//!           queue), one task per SDRconnect client (`client`), and rtltcp.rs's tasks per rtl_tcp client; shared
//!           `State` behind a mutex.
//! Means:    tokio, tokio-tungstenite, futures-util, clap, tracing; modules chunk, resample, rtltcp, sdrc,
//!           spectrum, state, tci.
//! Provides: CLI `tci_transcode [--tci host:port] [--listen host:port] [--iq-rate S/s] [--lead frontend|client]
//!           [--rtltcp host:port|off] [--rtltcp-offset-khz kHz]`.
//! Expects:  a TCI server at `--tci` (it may come and go; the link retries); free ports for `--listen` and
//!           `--rtltcp`.
//! Ensures:  a lost TCI link is retried with back-off 0.5 s … 10 s and reported to clients as `started=false`;
//!           a slow client never stalls the TCI link or other clients; `iq_raw` costs nothing without an rtl_tcp
//!           client.
//! Fails:    `--listen` cannot bind → exit 1; `--rtltcp` cannot bind → warning, rtl_tcp off, bridge runs on;
//!           non-float32 TCI frames → skipped with one warning.
//! Tags:     #tci #sdrconnect #rtltcp #net
//! See:      AGENTS.md (architecture, invariants, measured partners), README.md, e2e tests at the end of this file.
//! Proves:   (tests module `e2e`) a tone, tuning and all three streams cross the bridge over real sockets; rtl_tcp
//!           header, commands and the shifted tone arrive end to end.

mod chunk;
mod resample;
mod rtltcp;
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

// Command line of the bridge.
// Pitfall: `//`, not `///`, above this struct: clap turns a doc comment
//          here into the --help text and would replace the Cargo description.
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
    /// Where rtl_tcp clients connect, host:port, or `off`. DAB programs tune
    /// themselves: run with `--lead client` for them.
    #[arg(long, default_value = "127.0.0.1:1234")]
    rtltcp: String,
    /// How far above an rtl_tcp client's frequency the hardware centre goes,
    /// so the receiver's DC spike stays out of the client's band; 0 tunes on
    /// the frequency itself.
    #[arg(long, default_value_t = 1000)]
    rtltcp_offset_khz: i64,
}

/// Everything the tasks share: the receiver's state, the fan-out channels to
/// clients, and the one queue of commands to the TCI server.
///
/// Broadcast channels drop the oldest item for a receiver that falls behind
/// (`RecvError::Lagged`); that is the back-pressure by design.
struct Bridge {
    /// The receiver as last reported by TCI, plus local properties.
    // Pitfall: a std Mutex, held only for short, non-async sections; never
    //          hold it across an `.await`.
    state: Mutex<State>,
    /// property_changed lines for every client.
    events: broadcast::Sender<String>,
    /// SDRconnect IQ frames (payload type 2, int16), 20 ms each.
    iq: broadcast::Sender<Bytes>,
    /// The TCI server's IQ as it came, for the rtl_tcp side.
    iq_raw: broadcast::Sender<Arc<rtltcp::RawIq>>,
    /// SDRconnect audio frames (payload type 1, PCM16 stereo), 20 ms each.
    audio: broadcast::Sender<Bytes>,
    /// SDRconnect spectrum frames (payload type 3, 512 × uint8), ~23 a second.
    spectrum: broadcast::Sender<Bytes>,
    /// Commands for the TCI server.
    to_tci: mpsc::UnboundedSender<String>,
}

impl Bridge {
    /// Broadcast a `property_changed` for each name in `props` with its current
    /// value; names the state does not know are skipped. Holds the state lock
    /// while sending (broadcast send does not block).
    fn announce(&self, props: &[&str]) {
        let st = self.state.lock().unwrap();
        for p in props {
            if let Some(line) = sdrc::property_changed(&st, p) {
                // Err = no client subscribed right now; nothing to do.
                let _ = self.events.send(line);
            }
        }
    }
}

/// Start logging, the TCI link, the rtl_tcp listener (unless `off`) and the
/// SDRconnect listener; then serve SDRconnect clients for ever.
#[tokio::main]
async fn main() {
    // Log level from RUST_LOG, default info for this crate only.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tci_transcode=info".into()),
        )
        .init();
    let args = Args::parse();
    let (bridge, from_bridge) = new_bridge(args.lead);
    tokio::spawn(tci_link(bridge.clone(), args.tci.clone(), args.iq_rate, from_bridge));

    // The SDRconnect port is the bridge's reason to exist: without it, exit.
    let listener = match TcpListener::bind(&args.listen).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("cannot listen on {}: {e}", args.listen);
            std::process::exit(1);
        }
    };
    // rtl_tcp is optional: a busy port turns it off with a warning, the rest runs.
    if args.rtltcp != "off" {
        match TcpListener::bind(&args.rtltcp).await {
            Ok(l) => {
                tracing::info!(listen = %args.rtltcp, "rtl_tcp up");
                let (iq, to_tci) = (bridge.iq_raw.clone(), bridge.to_tci.clone());
                let tuning = rtltcp::Tuning { lead: args.lead, offset_hz: args.rtltcp_offset_khz * 1000 };
                tokio::spawn(rtltcp::serve(l, iq, to_tci, tuning));
            }
            Err(e) => tracing::warn!("rtl_tcp off, cannot listen on {}: {e}", args.rtltcp),
        }
    }
    tracing::info!(listen = %args.listen, tci = %args.tci, lead = ?args.lead, "TCI_transcode up");
    serve(listener, bridge).await;
}

/// Build the shared `Bridge` and the receiving end of its TCI command queue
/// (handed to `tci_link`). Channel capacities set how much a slow client may
/// fall behind before it loses items.
fn new_bridge(lead: Lead) -> (Arc<Bridge>, mpsc::UnboundedReceiver<String>) {
    let (to_tci, from_bridge) = mpsc::unbounded_channel();
    // Capacities, in items: events 256 lines; iq and audio 64 frames of 20 ms
    // (≈1.3 s); iq_raw 512 TCI blocks (≈1.3–2.5 s at 200–400 blocks/s);
    // spectrum 16 frames (≈0.7 s at 23/s). The `.0` keeps the sender; receivers
    // are made per client with `subscribe()`.
    let bridge = Arc::new(Bridge {
        state: Mutex::new(State::new(lead)),
        events: broadcast::channel(256).0,
        iq: broadcast::channel(64).0,
        iq_raw: broadcast::channel(512).0,
        audio: broadcast::channel(64).0,
        spectrum: broadcast::channel(16).0,
        to_tci,
    });
    (bridge, from_bridge)
}

/// Accept SDRconnect clients for ever.
/// One task per connection; an accept error is logged and the loop goes on.
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
///
/// In: the bridge, the server's host:port, the starting IQ rate (S/s), the
/// queue of commands from clients. Never returns. Per connection: subscribe
/// after `ready;`, fold status lines into `State` and announce changes, turn
/// binary frames into the SDRconnect streams and `iq_raw`, and write queued
/// commands to the server.
async fn tci_link(
    bridge: Arc<Bridge>,
    addr: String,
    iq_rate: u32,
    mut commands: mpsc::UnboundedReceiver<String>,
) {
    let url = format!("ws://{addr}");
    let mut backoff = Duration::from_millis(500);
    // Per-stream processors live across reconnects; each resets itself when the rate changes.
    let mut spec = spectrum::Spectrum::new();
    // Reused float buffer for each frame's payload.
    let mut samples = Vec::new();
    // The IQ rate to subscribe at: the command line's, until a client sets one.
    let mut want_rate = iq_rate;
    let mut iq_chunks = chunk::Chunker::new(2);
    let mut audio_chunks = chunk::Chunker::new(2);
    // Frames seen per stream [IQ, audio], for the debug log only.
    let mut counts = [0u32; 2];
    let mut format_warned = false;
    loop {
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => {
                tracing::info!(%url, "TCI connected");
                // A good connection resets the back-off for the next loss.
                backoff = Duration::from_millis(500);
                let (mut tx, mut rx) = ws.split();
                let mut last_metrics = Instant::now();
                loop {
                    tokio::select! {
                        // Server → bridge.
                        msg = rx.next() => match msg {
                            Some(Ok(Message::Text(t))) => {
                                tracing::debug!(line = %t.as_str(), "from TCI");
                                // One text frame may hold many `;`-terminated commands.
                                for (name, args) in tci::split_commands(t.as_str()) {
                                    let status = tci::parse_status(&name, &args);
                                    if status == tci::Status::Ready {
                                        // Subscribe once the server's status burst is over, at
                                        // the rate wanted (the server reports its own before).
                                        // Invariant: want_rate survives a reconnect, so a client's
                                        //            rate is restored when the server comes back.
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
                                    // Fold into State (short lock), then announce outside it.
                                    let changed = bridge.state.lock().unwrap().apply(&status);
                                    bridge.announce(&changed);
                                }
                            }
                            Some(Ok(Message::Binary(b))) => {
                                // Shorter than a header: not a stream frame, skip.
                                let Some(h) = tci::parse_header(&b) else { continue };
                                // One receiver served: receiver 0.
                                if h.receiver != 0 {
                                    continue;
                                }
                                // Only float32 is decoded; warn once, not per frame.
                                if h.format != tci::FORMAT_FLOAT32 {
                                    if !format_warned {
                                        tracing::warn!(format = h.format, "TCI frame not float32, skipped");
                                        format_warned = true;
                                    }
                                    continue;
                                }
                                // Debug log: the first frame, then every 500th per stream.
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
                                        // rtl_tcp side: the raw block with the rate from
                                        // the header and the centre from the last `dds`.
                                        // Invariant: copy only while an rtl_tcp client
                                        //            listens; the copy costs per block.
                                        if bridge.iq_raw.receiver_count() > 0 {
                                            let center_hz = bridge.state.lock().unwrap().center_hz;
                                            let raw = rtltcp::RawIq { rate: h.sample_rate, center_hz, data: samples.clone() };
                                            let _ = bridge.iq_raw.send(Arc::new(raw));
                                        }
                                        // SDRconnect IQ: gathered to 20 ms frames (see chunk.rs).
                                        if let Some(frame) = iq_chunks.push(&samples, h.sample_rate) {
                                            let _ = bridge.iq.send(Bytes::from(sdrc::iq_frame(&frame)));
                                        }
                                        // Spectrum and channel metrics from the same IQ.
                                        if let Some(db) = spec.feed(&samples, h.sample_rate) {
                                            publish_spectrum(&bridge, &db, h.sample_rate, &mut last_metrics);
                                        }
                                    }
                                    tci::Stream::RxAudio => {
                                        // Channels 0 = unset by the server: assume stereo.
                                        let ch = if h.channels == 0 { 2 } else { h.channels };
                                        // Mono becomes stereo here, so the chunker and
                                        // audio_frame always see two channels.
                                        let stereo: Vec<f32> = if ch == 1 {
                                            samples.iter().flat_map(|&s| [s, s]).collect()
                                        } else {
                                            samples.clone()
                                        };
                                        if let Some(frame) = audio_chunks.push(&stereo, h.sample_rate) {
                                            let _ = bridge.audio.send(Bytes::from(sdrc::audio_frame(&frame, 2)));
                                        }
                                    }
                                    // TX audio, TX chrono, unknown: not served.
                                    _ => {}
                                }
                            }
                            // Ping, pong, close: nothing to do here (after a close the next read is None).
                            Some(Ok(_)) => {}
                            Some(Err(e)) => { tracing::warn!("TCI read: {e}"); break; }
                            None => break,
                        },
                        // Bridge → server: commands from SDRconnect and rtl_tcp clients.
                        Some(c) = commands.recv() => {
                            tracing::debug!(command = %c, "to TCI");
                            // Remember a requested IQ rate, so the next `ready;`
                            // (after a reconnect) subscribes at it.
                            if let Some(r) = c.strip_prefix("iq_samplerate:").and_then(|r| r.trim_end_matches(';').parse().ok()) {
                                want_rate = r;
                            }
                            if tx.send(Message::text(c)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                // Tell clients the radio is gone (`started` = false) until it is back.
                tracing::warn!("TCI connection lost");
                let changed = bridge.state.lock().unwrap().apply(&tci::Status::Stop);
                bridge.announce(&changed);
            }
            Err(e) => tracing::warn!(%url, "TCI connect failed: {e}"),
        }
        // Back-off doubles per failure, 0.5 s up to 10 s.
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// Send a spectrum frame, and once a second refresh signal power and S/N.
///
/// In: dBFS per bin (lowest frequency first), the IQ rate in S/s, the time of
/// the last metrics update (updated here). Side effects: broadcasts one
/// spectrum frame; once a second writes `signal_power_db` / `signal_snr_db`
/// into State and announces both.
fn publish_spectrum(bridge: &Bridge, db: &[f32], rate: u32, last_metrics: &mut Instant) {
    // All State work in one short lock; sending happens after it is released.
    let (bytes, metrics_due) = {
        let mut st = bridge.state.lock().unwrap();
        // Display range from the clients' local properties, dBFS.
        let base = st.local_f32("spectrum_base").unwrap_or(-140.0);
        let reference = st.local_f32("spectrum_ref_level").unwrap_or(-20.0);
        let due = last_metrics.elapsed() >= Duration::from_secs(1);
        if due {
            // The channel sits at VFO − centre in the spectrum.
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
///
/// Ends when the client closes, a send fails, or a channel closes. Every
/// connection receives all property events; binary streams only as switched on.
async fn client(bridge: Arc<Bridge>, stream: TcpStream) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!("WebSocket handshake failed: {e}");
            return;
        }
    };
    let (mut tx, mut rx) = ws.split();
    // Subscribe to everything at once; a stream that is switched off is
    // received and dropped (see `forward!`), so switching on needs no resubscribe.
    let mut events = bridge.events.subscribe();
    let mut iq = bridge.iq.subscribe();
    let mut audio = bridge.audio.subscribe();
    let mut spec = bridge.spectrum.subscribe();
    let mut streams = sdrc::Streams::default();

    // A lagging receiver lost frames; that is the intended back-pressure.
    // `forward!(result, switched_on)`: send a binary frame if switched on;
    // `break` the loop when the client is gone or the channel closed.
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
            // Client → bridge: JSON text.
            msg = rx.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    // Handle under the lock, send after releasing it.
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
                // Binary or ping from a client: nothing to do.
                Some(Ok(_)) => {}
                Some(Err(e)) => { tracing::debug!("client read: {e}"); break; }
            },
            // Bridge → client: property events, never dropped silently.
            ev = events.recv() => match ev {
                Ok(line) => {
                    if tx.send(Message::text(line)).await.is_err() {
                        break;
                    }
                }
                // Missed events are worth a warning: the client's view of the
                // properties may now be stale.
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

    /// The fake server's DDS centre, Hz.
    const CENTER: i64 = 433_900_000;
    /// The fake tone's distance above the centre, Hz.
    const TONE_OFFSET: f64 = 20_000.0;

    /// Accepts one TCI client, sends the status burst, echoes tuning, and
    /// streams the tone once IQ is started. Every command received is kept.
    ///
    /// The tone: amplitude 0.5 (−6 dBFS), in 5 ms blocks at the
    /// rate last asked for. The centre stays at CENTER whatever `dds` says.
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
                            // Takes any rate as asked and confirms it.
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
                // Every 5 ms, rate/200 samples of the tone.
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

    /// A client-side WebSocket.
    type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;

    /// Send one SDRconnect JSON message.
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

    /// get_property and wait for its response value.
    async fn get(ws: &mut Ws, prop: &str) -> String {
        send(ws, "get_property", prop, "").await;
        expect(ws, "get_property_response", prop).await
    }

    /// Next binary frame of `payload_type`, whole (type bytes included).
    async fn next_binary(ws: &mut Ws, payload_type: u16) -> Vec<u8> {
        loop {
            if let Message::Binary(b) = ws.next().await.unwrap().unwrap() {
                if b[..2] == payload_type.to_le_bytes() {
                    return b.to_vec();
                }
            }
        }
    }

    /// SDRconnect end to end: properties from the burst, the tone in the right
    /// spectrum bin, int16 IQ frames, and tuning forwarded with the client leading.
    #[tokio::test(flavor = "multi_thread")]
    async fn tone_tuning_and_streams_cross_the_bridge() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        // Port 0: the OS picks a free port, so tests never collide with a running bridge.
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
            // Poll until the subscription went through and the server confirmed the rate.
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

    /// rtl_tcp end to end: header, rate and frequency commands reach the TCI
    /// server, and the 8-bit stream carries the fake server's tone.
    #[tokio::test(flavor = "multi_thread")]
    async fn rtl_tcp_header_commands_and_tone() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen = Arc::new(Mutex::new(Vec::new()));
        let tci_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tci_addr = tci_listener.local_addr().unwrap().to_string();
        tokio::spawn(fake_tci(tci_listener, seen.clone()));
        let (bridge, from_bridge) = new_bridge(Lead::Client);
        tokio::spawn(tci_link(bridge.clone(), tci_addr, 250_000, from_bridge));
        let rl = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let raddr = rl.local_addr().unwrap();
        let (iq, to_tci) = (bridge.iq_raw.clone(), bridge.to_tci.clone());
        // Offset 0: tune on the frequency. The fake server keeps its centre at
        // CENTER whatever `dds` asks, so the shift is what gets tested.
        let tuning = rtltcp::Tuning { lead: Lead::Client, offset_hz: 0 };
        tokio::spawn(rtltcp::serve(rl, iq, to_tci, tuning));

        let bytes = timeout(Duration::from_secs(10), async {
            let mut s = TcpStream::connect(raddr).await.unwrap();
            let mut head = [0u8; 12];
            s.read_exact(&mut head).await.unwrap();
            assert_eq!(&head[..4], b"RTL0");
            // Two commands in one write: rate 2.048 MS/s, then 433.95 MHz.
            let mut cmd = vec![0x02];
            cmd.extend_from_slice(&2_048_000u32.to_be_bytes());
            cmd.push(0x01);
            cmd.extend_from_slice(&433_950_000u32.to_be_bytes());
            s.write_all(&cmd).await.unwrap();
            // Past the AGC's settling, then a stretch to measure.
            let mut buf = vec![0u8; 2_000_000];
            s.read_exact(&mut buf).await.unwrap();
            buf
        })
        .await
        .expect("rtl_tcp run timed out");

        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"iq_samplerate:2048000".to_string()), "{seen:?}");
        assert!(seen.contains(&"dds:0,433950000".to_string()), "{seen:?}");
        // The tone's frequency from the mean phase step of the last 0.25 MB.
        let tail = &bytes[bytes.len() - 500_000..];
        let c: Vec<(f64, f64)> =
            tail.chunks(2).map(|v| (v[0] as f64 - 127.5, v[1] as f64 - 127.5)).collect();
        let (mut re, mut im) = (0.0, 0.0);
        for w in c.windows(2) {
            re += w[1].0 * w[0].0 + w[1].1 * w[0].1;
            im += w[1].1 * w[0].0 - w[1].0 * w[0].1;
        }
        let f = im.atan2(re) * 2_048_000.0 / std::f64::consts::TAU;
        // The fake server keeps its centre at CENTER, so the bridge shifts the
        // stream to the client's 433.95 MHz: the tone 20 kHz above the centre
        // shows 30 kHz below the client's frequency.
        let want = TONE_OFFSET - (433_950_000 - CENTER) as f64;
        assert!((f - want).abs() < 200.0, "tone at {f:.0} Hz, want {want:.0}");
        // Level: the pair RMS near the AGC's target of 0.178 (−15 dBFS).
        let power = c.iter().map(|v| v.0 * v.0 + v.1 * v.1).sum::<f64>() / c.len() as f64;
        let rms = power.sqrt() / 127.5;
        assert!(rms > 0.1 && rms < 0.3, "level {rms}");
    }
}
