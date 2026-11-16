//! tci_diag — small diagnostics against any TCI server (SDR Oxide, ExpertSDR, …).
//!
//! Ends:     answer "what does this TCI server say, deliver and accept?" in one command, without the bridge or a
//!           decoder in between: the status burst, the live conversation, the IQ stream's real rate and gaps, which
//!           ports serve TCI at all, and whether a SPOT label arrives (the return path a decoder will use).
//! Ways:     one WebSocket client per run (tokio-tungstenite); text commands parsed with the bridge's own tci.rs.
//! Means:    tokio, tokio-tungstenite, futures-util, clap; the bridge's tci.rs included by path (no library crate).
//! Provides: tci_diag status | watch | iq | ports | spot | info   (see --help of each)
//! Expects:  a TCI server at --tci (default 127.0.0.1:50001); for `iq`, that no other client needs the IQ
//!           rate unchanged (the subscription changes the server's rate for everyone).
//! Ensures:  read-only against the server except `iq` (sets the IQ rate, starts and stops IQ) and `spot` /
//!           `info` (adds a label, optionally deletes it); exit 0 on success.
//! Fails:    a server that never sends `ready;` within the timeout: exit 1 with what it did send.
//! Tags:     #tci #test #net
//! See:      src/tci.rs (parser), examples/tci_probe.rs (the older one-shot IQ probe), AGENTS.md §5 (what these
//!           measurements found).
//! Proves:   (tests) colour parsing, the SPOT line format, the status-burst summary.

// The bridge's own TCI parser, compiled in by path (the crate has no library
// target); dead_code allowed, since the diagnostics use only part of it.
#[allow(dead_code)]
#[path = "../tci.rs"]
mod tci;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// A client WebSocket to the TCI server.
type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

// The command line.
// Pitfall: plain `//` above clap structs, variants and fields that have no doc
//          yet: clap turns `///` there into --help text.
#[derive(Parser)]
#[command(version, about = "Diagnostics against a TCI server")]
struct Cli {
    /// TCI server, host:port
    #[arg(long, global = true, default_value = "127.0.0.1:50001")]
    tci: String,
    #[command(subcommand)]
    cmd: Cmd,
}

// The subcommands; their `///` lines are the --help text.
#[derive(Subcommand)]
enum Cmd {
    /// Print the status burst up to `ready;` and a summary (device, protocol, VFO, centre, mode, IQ rate).
    Status,
    /// Print every text line the server sends, and the binary frames per stream every 5 s, until Ctrl-C.
    Watch,
    /// Subscribe IQ and measure the delivered rate, frame sizes and the longest pause.
    Iq {
        // IQ rate to ask for, S/s.
        #[arg(long, default_value_t = 500_000)]
        rate: u32,
        // Seconds to measure, after a 2 s warm-up that is not counted.
        #[arg(long, default_value_t = 20)]
        secs: u64,
    },
    /// Try a range of ports on the host and say which serve TCI, with device and VFO.
    Ports {
        // First and last port to try, inclusive; the host comes from --tci.
        #[arg(long, default_value_t = 50001)]
        from: u16,
        #[arg(long, default_value_t = 50010)]
        to: u16,
    },
    /// Send one SPOT label, keep the connection for --hold seconds, then optionally delete it.
    Spot {
        // Label text in the waterfall (the SPOT "callsign" field); also the key for SPOT_DELETE.
        #[arg(long, default_value = "TCI-MSG")]
        call: String,
        // SPOT mode field, free text.
        #[arg(long, default_value = "INFO")]
        mode: String,
        /// Frequency in Hz; default: the server's current VFO.
        #[arg(long)]
        hz: Option<i64>,
        /// ARGB as 0xAARRGGBB or decimal.
        #[arg(long, default_value = "0xFF9E9E9E")]
        color: String,
        // Free text carried with the spot.
        #[arg(long, default_value = "tci_diag spot")]
        text: String,
        // Seconds to keep the connection open before closing.
        #[arg(long, default_value_t = 30)]
        hold: u64,
        /// Send SPOT_DELETE for the call before closing.
        #[arg(long)]
        delete: bool,
    },
    /// Shortcut: a grey INFO spot at the VFO, e.g. `tci_diag info "rtl433 decoder running"`.
    Info {
        // The message, shown as the spot's text.
        text: String,
        // Seconds to keep the label up; it is deleted at the end.
        #[arg(long, default_value_t = 30)]
        hold: u64,
    },
}

/// What the status burst said, reduced to the lines a diagnosis needs.
#[derive(Default, Debug)]
struct Summary {
    /// `protocol` line: server name and TCI version.
    protocol: Option<String>,
    /// `device` line: the radio's name.
    device: Option<String>,
    /// VFO A of receiver 0, Hz.
    vfo: Option<i64>,
    /// IQ centre (DDS) of receiver 0, Hz.
    dds: Option<i64>,
    /// Modulation of receiver 0, lower case.
    modulation: Option<String>,
    /// IQ rate the server reported, S/s.
    iq_rate: Option<u32>,
    /// Every command seen, re-joined as `name:args;`, in order.
    lines: Vec<String>,
}

impl Summary {
    /// Take in one text frame: keep every command as a line, and the fields
    /// above from the ones the tci.rs parser recognises (last one wins).
    fn take(&mut self, text: &str) {
        for (name, args) in tci::split_commands(text) {
            // Re-joined in a canonical form (lower-case name), not the raw text.
            self.lines.push(if args.is_empty() { format!("{name};") } else { format!("{name}:{};", args.join(",")) });
            match tci::parse_status(&name, &args) {
                tci::Status::Protocol(p) => self.protocol = Some(p),
                tci::Status::Device(d) => self.device = Some(d),
                tci::Status::Vfo { ch: 0, hz } => self.vfo = Some(hz),
                tci::Status::Dds(hz) => self.dds = Some(hz),
                tci::Status::Modulation(m) => self.modulation = Some(m),
                tci::Status::IqRate(r) => self.iq_rate = Some(r),
                _ => {}
            }
        }
    }
}

/// Open `ws://<addr>` and read the status burst up to `ready;`.
///
/// Out: the open socket and the burst's `Summary`. `timeout` covers the
/// connect and, separately, the whole burst. Err: a readable message with the
/// address, and on timeout the lines that did arrive.
async fn connect(addr: &str, timeout: Duration) -> Result<(Ws, Summary), String> {
    let (mut ws, _) = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(format!("ws://{addr}")))
        .await
        .map_err(|_| format!("{addr}: no answer within {timeout:?}"))?
        .map_err(|e| format!("{addr}: {e}"))?;
    let mut s = Summary::default();
    // One deadline for the whole burst, not per frame: a server that trickles
    // lines without ever sending `ready;` still ends in time.
    let end = Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(end.into(), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                s.take(t.as_str());
                // `ready;` anywhere in the frame (servers send it last in the burst).
                if t.as_str().to_lowercase().contains("ready;") {
                    return Ok((ws, s));
                }
            }
            // Binary frames or pings during the burst: ignored.
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(format!("{addr}: {e}")),
            Ok(None) => return Err(format!("{addr}: closed before ready; ({} lines)", s.lines.len())),
            Err(_) => return Err(format!("{addr}: no ready; within {timeout:?} ({} lines: {:?})", s.lines.len(), s.lines)),
        }
    }
}

/// Hz → "433.920000 MHz" (six decimals, so 1 Hz still shows).
fn mhz(hz: i64) -> String {
    format!("{:.6} MHz", hz as f64 / 1e6)
}

/// TCI colours are ARGB integers; accept 0xAARRGGBB, #AARRGGBB or decimal.
/// Err: a message naming the accepted forms.
fn parse_color(s: &str) -> Result<u32, String> {
    let t = s.trim();
    // A hex prefix decides the base; without one the text is decimal.
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).or_else(|| t.strip_prefix('#'));
    match hex {
        Some(h) => u32::from_str_radix(h, 16),
        None => t.parse(),
    }
    .map_err(|_| format!("colour {s:?}: needs 0xAARRGGBB or a decimal ARGB value"))
}

/// The TCI SPOT command: `spot:<call>,<mode>,<hz>,<argb decimal>,<text>;`.
// Pitfall: no escaping; a comma or semicolon in `call` or `text` breaks the line.
fn spot_line(call: &str, mode: &str, hz: i64, argb: u32, text: &str) -> String {
    format!("spot:{call},{mode},{hz},{argb},{text};")
}

/// Send one SPOT label, hold the connection `hold` seconds printing any spot
/// lines the server sends back, then optionally SPOT_DELETE it and close.
///
/// The return path a decoder will use: if the label shows in the server's
/// waterfall, labels from the decoder will too. Err: no connection, or no
/// frequency (none given and no VFO in the burst).
async fn spot(addr: &str, call: &str, mode: &str, hz: Option<i64>, argb: u32, text: &str, hold: u64, delete: bool) -> Result<(), String> {
    let (mut ws, s) = connect(addr, Duration::from_secs(5)).await?;
    // Default frequency: the VFO the server just reported.
    let hz = hz.or(s.vfo).ok_or("no --hz given and the server reported no VFO")?;
    let line = spot_line(call, mode, hz, argb, text);
    ws.send(Message::text(line.clone())).await.map_err(|e| e.to_string())?;
    println!("sent  {line}  ({} on {})", mhz(hz), s.device.as_deref().unwrap_or("?"));
    println!("look for the label in the waterfall; holding the connection {hold} s");
    let end = Instant::now() + Duration::from_secs(hold);
    // Hold the socket open and echo only spot-related lines (the rest is VFO chatter).
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end.into(), ws.next()).await {
        if let Message::Text(t) = msg {
            if t.as_str().to_lowercase().contains("spot") {
                println!("recv  {}", t.as_str().trim());
            }
        }
    }
    if delete {
        let del = format!("spot_delete:{call};");
        ws.send(Message::text(del.clone())).await.map_err(|e| e.to_string())?;
        println!("sent  {del}");
        // Give the server a moment to act on the delete before the close arrives.
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = ws.close(None).await;
    Ok(())
}

/// Dispatch one subcommand; every error comes back as a printable string.
async fn run(cli: Cli) -> Result<(), String> {
    match cli.cmd {
        Cmd::Status => {
            let (_ws, s) = connect(&cli.tci, Duration::from_secs(5)).await?;
            for l in &s.lines {
                println!("  {l}");
            }
            println!("device      {}", s.device.as_deref().unwrap_or("?"));
            println!("protocol    {}", s.protocol.as_deref().unwrap_or("?"));
            println!("VFO         {}", s.vfo.map(mhz).unwrap_or("?".into()));
            println!("DDS centre  {}", s.dds.map(mhz).unwrap_or("?".into()));
            // The VFO–DDS distance: SDR Oxide showed 26.8–28.1 kHz here (2026-11-16).
            if let (Some(v), Some(d)) = (s.vfo, s.dds) {
                println!("VFO − DDS   {:+.1} kHz (hardware centre; the IQ stream has its own centre)", (v - d) as f64 / 1e3);
            }
            println!("mode        {}", s.modulation.as_deref().unwrap_or("?"));
            println!("IQ rate     {}", s.iq_rate.map(|r| r.to_string()).unwrap_or("?".into()));
        }
        Cmd::Watch => {
            let (mut ws, s) = connect(&cli.tci, Duration::from_secs(5)).await?;
            println!("connected to {} ({} status lines); Ctrl-C ends", s.device.as_deref().unwrap_or("?"), s.lines.len());
            let start = Instant::now();
            let mut tick = Instant::now();
            // Binary frames counted per stream kind, printed and reset every 5 s.
            let mut frames: BTreeMap<String, u64> = BTreeMap::new();
            while let Some(msg) = ws.next().await {
                match msg.map_err(|e| e.to_string())? {
                    Message::Text(t) => println!("{:8.3}  {}", start.elapsed().as_secs_f64(), t.as_str().trim()),
                    Message::Binary(b) => {
                        if let Some(h) = tci::parse_header(&b) {
                            *frames.entry(format!("{:?}", h.stream)).or_default() += 1;
                        }
                    }
                    _ => {}
                }
                // Checked after each message, so a silent server prints nothing.
                if tick.elapsed() >= Duration::from_secs(5) && !frames.is_empty() {
                    println!("{:8.3}  binary frames in 5 s by stream type: {frames:?}", start.elapsed().as_secs_f64());
                    frames.clear();
                    tick = Instant::now();
                }
            }
        }
        Cmd::Iq { rate, secs } => iq(&cli.tci, rate, secs).await?,
        Cmd::Ports { from, to } => {
            // Host part of --tci; its port is replaced by each port in the range.
            let host = cli.tci.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1").to_string();
            for port in from..=to {
                match connect(&format!("{host}:{port}"), Duration::from_millis(1500)).await {
                    Ok((_ws, s)) => println!(
                        "{port}  TCI  {}  VFO {}  centre {}",
                        s.device.as_deref().unwrap_or("?"),
                        s.vfo.map(mhz).unwrap_or("?".into()),
                        s.dds.map(mhz).unwrap_or("?".into())
                    ),
                    // Refused = nothing listens; anything else (timeout, not TCI) is printed in full.
                    Err(e) if e.contains("refused") => println!("{port}  -"),
                    Err(e) => println!("{port}  ?  {e}"),
                }
            }
        }
        Cmd::Spot { call, mode, hz, color, text, hold, delete } => {
            spot(&cli.tci, &call, &mode, hz, parse_color(&color)?, &text, hold, delete).await?
        }
        Cmd::Info { text, hold } => spot(&cli.tci, "TCI-MSG", "INFO", None, 0xFF9E_9E9E, &text, hold, true).await?,
    }
    Ok(())
}

/// Subscribe IQ at `rate` (S/s) and measure for `secs` seconds after a 2 s
/// warm-up: delivered samples per second (and % of requested), frames per
/// second, the most common frame sizes, the longest pause between frames.
///
/// Side effect: changes the server's IQ rate, and sends `iq_stop` at the end.
async fn iq(addr: &str, rate: u32, secs: u64) -> Result<(), String> {
    let (mut ws, _) = connect(addr, Duration::from_secs(5)).await?;
    for c in [tci::cmd::iq_samplerate(rate), tci::cmd::iq_start()] {
        ws.send(Message::text(c)).await.map_err(|e| e.to_string())?;
    }
    // The first 2 s are warm-up (rate change, DDC rebuild) and are not counted.
    let (mut first, mut last) = (None::<Instant>, Instant::now());
    let (mut samples, mut frames, mut max_gap) = (0u64, 0u64, Duration::ZERO);
    let mut sizes = BTreeMap::<usize, u64>::new();
    let end = Instant::now() + Duration::from_secs(secs + 2);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(end.into(), ws.next()).await {
        let Message::Binary(b) = msg else { continue };
        let Some(h) = tci::parse_header(&b) else { continue };
        if h.stream != tci::Stream::Iq {
            continue;
        }
        let now = Instant::now();
        // Warm-up: the clock starts at the first IQ frame, not at subscribe.
        if now.duration_since(*first.get_or_insert(now)) < Duration::from_secs(2) {
            last = now;
            continue;
        }
        // Complex samples in the frame: 8 bytes each (two float32) after the 64-byte header.
        let n = (b.len() - 64) / 8;
        samples += n as u64;
        frames += 1;
        *sizes.entry(n).or_default() += 1;
        max_gap = max_gap.max(now.duration_since(last));
        last = now;
    }
    let _ = ws.send(Message::text("iq_stop:0;")).await;
    // Pitfall: divides by `secs`, but the window ends at a deadline set before
    //          the first frame came; a late first frame shortens it, so the
    //          rate reads a little low then.
    let per_s = samples as f64 / secs as f64;
    println!("requested     {rate} samples/s");
    println!("delivered     {per_s:.0} samples/s  ({:.1} %)", 100.0 * per_s / rate as f64);
    println!("frames/s      {:.1}", frames as f64 / secs as f64);
    println!("frame sizes   {:?}", sizes.iter().take(6).collect::<Vec<_>>());
    println!("longest pause {:.1} ms", max_gap.as_secs_f64() * 1e3);
    Ok(())
}

/// Parse the command line and run; on error print `tci_diag: <reason>` and exit 1.
#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("tci_diag: {e}");
        std::process::exit(1);
    }
}

/// Proves: colour forms parse (and a name is refused); the SPOT line format;
/// the summary picks the fields out of a burst.
#[cfg(test)]
mod tests {
    use super::*;

    /// Hex with 0x or #, decimal, and a name that must be refused.
    #[test]
    fn colours() {
        assert_eq!(parse_color("0xFF9E9E9E").unwrap(), 0xFF9E_9E9E);
        assert_eq!(parse_color("#ff000000").unwrap(), 0xFF00_0000);
        assert_eq!(parse_color("4278190080").unwrap(), 0xFF00_0000);
        assert!(parse_color("grey").is_err());
    }

    /// The SPOT line, with the colour as a decimal ARGB.
    #[test]
    fn spot_text() {
        assert_eq!(
            spot_line("TCI-DIAG", "INFO", 433_920_000, 0xFF9E_9E9E, "rtl433 running"),
            "spot:TCI-DIAG,INFO,433920000,4288585374,rtl433 running;"
        );
    }

    /// A burst in one frame, with SDR Oxide's 28 kHz between VFO and DDS.
    #[test]
    fn summary_reads_the_burst() {
        let mut s = Summary::default();
        s.take("protocol:ExpertSDR3,2.0;device:SDR Oxide;dds:0,433892000;vfo:0,0,433920000;iq_samplerate:500000;ready;");
        assert_eq!(s.vfo, Some(433_920_000));
        assert_eq!(s.dds, Some(433_892_000));
        assert_eq!(s.iq_rate, Some(500_000));
        assert_eq!(s.device.as_deref(), Some("SDR Oxide"));
    }
}
