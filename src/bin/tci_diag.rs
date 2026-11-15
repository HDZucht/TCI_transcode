//! tci_diag — small diagnostics against any TCI server (SDR Oxide, ExpertSDR, …).
//!
//! Ends:     answer "what does this TCI server say, deliver and accept?" in one command, without the bridge or a
//!           decoder in between: the status burst, the live conversation, the IQ stream's real rate and gaps, which
//!           ports serve TCI at all, and whether a SPOT label arrives (the return path a decoder will use).
//! Ways:     one WebSocket client per run (tokio-tungstenite); text commands parsed with the bridge's own tci.rs.
//! Provides: tci_diag status | watch | iq | ports | spot | info   (see --help of each)
//! Fails:    a server that never sends `ready;` within the timeout: exit 1 with what it did send.

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

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Parser)]
#[command(version, about = "Diagnostics against a TCI server")]
struct Cli {
    /// TCI server, host:port
    #[arg(long, global = true, default_value = "127.0.0.1:50001")]
    tci: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the status burst up to `ready;` and a summary (device, protocol, VFO, centre, mode, IQ rate).
    Status,
    /// Print every text line the server sends, and the binary frames per stream every 5 s, until Ctrl-C.
    Watch,
    /// Subscribe IQ and measure the delivered rate, frame sizes and the longest pause.
    Iq {
        #[arg(long, default_value_t = 500_000)]
        rate: u32,
        #[arg(long, default_value_t = 20)]
        secs: u64,
    },
    /// Try a range of ports on the host and say which serve TCI, with device and VFO.
    Ports {
        #[arg(long, default_value_t = 50001)]
        from: u16,
        #[arg(long, default_value_t = 50010)]
        to: u16,
    },
    /// Send one SPOT label, keep the connection for --hold seconds, then optionally delete it.
    Spot {
        #[arg(long, default_value = "TCI-DIAG")]
        call: String,
        #[arg(long, default_value = "INFO")]
        mode: String,
        /// Frequency in Hz; default: the server's current VFO.
        #[arg(long)]
        hz: Option<i64>,
        /// ARGB as 0xAARRGGBB or decimal.
        #[arg(long, default_value = "0xFF9E9E9E")]
        color: String,
        #[arg(long, default_value = "tci_diag spot")]
        text: String,
        #[arg(long, default_value_t = 30)]
        hold: u64,
        /// Send SPOT_DELETE for the call before closing.
        #[arg(long)]
        delete: bool,
    },
    /// Shortcut: a grey INFO spot at the VFO, e.g. `tci_diag info "rtl433 decoder running"`.
    Info {
        text: String,
        #[arg(long, default_value_t = 30)]
        hold: u64,
    },
}

/// What the status burst said, reduced to the lines a diagnosis needs.
#[derive(Default, Debug)]
struct Summary {
    protocol: Option<String>,
    device: Option<String>,
    vfo: Option<i64>,
    dds: Option<i64>,
    modulation: Option<String>,
    iq_rate: Option<u32>,
    lines: Vec<String>,
}

impl Summary {
    fn take(&mut self, text: &str) {
        for (name, args) in tci::split_commands(text) {
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

async fn connect(addr: &str, timeout: Duration) -> Result<(Ws, Summary), String> {
    let (mut ws, _) = tokio::time::timeout(timeout, tokio_tungstenite::connect_async(format!("ws://{addr}")))
        .await
        .map_err(|_| format!("{addr}: no answer within {timeout:?}"))?
        .map_err(|e| format!("{addr}: {e}"))?;
    let mut s = Summary::default();
    let end = Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(end.into(), ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                s.take(t.as_str());
                if t.as_str().to_lowercase().contains("ready;") {
                    return Ok((ws, s));
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(format!("{addr}: {e}")),
            Ok(None) => return Err(format!("{addr}: closed before ready; ({} lines)", s.lines.len())),
            Err(_) => return Err(format!("{addr}: no ready; within {timeout:?} ({} lines: {:?})", s.lines.len(), s.lines)),
        }
    }
}

fn mhz(hz: i64) -> String {
    format!("{:.6} MHz", hz as f64 / 1e6)
}

/// TCI colours are ARGB integers; accept 0xAARRGGBB, #AARRGGBB or decimal.
fn parse_color(s: &str) -> Result<u32, String> {
    let t = s.trim();
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).or_else(|| t.strip_prefix('#'));
    match hex {
        Some(h) => u32::from_str_radix(h, 16),
        None => t.parse(),
    }
    .map_err(|_| format!("colour {s:?}: needs 0xAARRGGBB or a decimal ARGB value"))
}

fn spot_line(call: &str, mode: &str, hz: i64, argb: u32, text: &str) -> String {
    format!("spot:{call},{mode},{hz},{argb},{text};")
}

async fn spot(addr: &str, call: &str, mode: &str, hz: Option<i64>, argb: u32, text: &str, hold: u64, delete: bool) -> Result<(), String> {
    let (mut ws, s) = connect(addr, Duration::from_secs(5)).await?;
    let hz = hz.or(s.vfo).ok_or("no --hz given and the server reported no VFO")?;
    let line = spot_line(call, mode, hz, argb, text);
    ws.send(Message::text(line.clone())).await.map_err(|e| e.to_string())?;
    println!("sent  {line}  ({} on {})", mhz(hz), s.device.as_deref().unwrap_or("?"));
    println!("look for the label in the waterfall; holding the connection {hold} s");
    let end = Instant::now() + Duration::from_secs(hold);
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
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let _ = ws.close(None).await;
    Ok(())
}

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
                if tick.elapsed() >= Duration::from_secs(5) && !frames.is_empty() {
                    println!("{:8.3}  binary frames in 5 s by stream type: {frames:?}", start.elapsed().as_secs_f64());
                    frames.clear();
                    tick = Instant::now();
                }
            }
        }
        Cmd::Iq { rate, secs } => iq(&cli.tci, rate, secs).await?,
        Cmd::Ports { from, to } => {
            let host = cli.tci.rsplit_once(':').map(|(h, _)| h).unwrap_or("127.0.0.1").to_string();
            for port in from..=to {
                match connect(&format!("{host}:{port}"), Duration::from_millis(1500)).await {
                    Ok((_ws, s)) => println!(
                        "{port}  TCI  {}  VFO {}  centre {}",
                        s.device.as_deref().unwrap_or("?"),
                        s.vfo.map(mhz).unwrap_or("?".into()),
                        s.dds.map(mhz).unwrap_or("?".into())
                    ),
                    Err(e) if e.contains("refused") => println!("{port}  -"),
                    Err(e) => println!("{port}  ?  {e}"),
                }
            }
        }
        Cmd::Spot { call, mode, hz, color, text, hold, delete } => {
            spot(&cli.tci, &call, &mode, hz, parse_color(&color)?, &text, hold, delete).await?
        }
        Cmd::Info { text, hold } => spot(&cli.tci, "TCI-DIAG", "INFO", None, 0xFF9E_9E9E, &text, hold, true).await?,
    }
    Ok(())
}

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
        if now.duration_since(*first.get_or_insert(now)) < Duration::from_secs(2) {
            last = now;
            continue;
        }
        let n = (b.len() - 64) / 8;
        samples += n as u64;
        frames += 1;
        *sizes.entry(n).or_default() += 1;
        max_gap = max_gap.max(now.duration_since(last));
        last = now;
    }
    let _ = ws.send(Message::text("iq_stop:0;")).await;
    let per_s = samples as f64 / secs as f64;
    println!("requested     {rate} samples/s");
    println!("delivered     {per_s:.0} samples/s  ({:.1} %)", 100.0 * per_s / rate as f64);
    println!("frames/s      {:.1}", frames as f64 / secs as f64);
    println!("frame sizes   {:?}", sizes.iter().take(6).collect::<Vec<_>>());
    println!("longest pause {:.1} ms", max_gap.as_secs_f64() * 1e3);
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("tci_diag: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours() {
        assert_eq!(parse_color("0xFF9E9E9E").unwrap(), 0xFF9E_9E9E);
        assert_eq!(parse_color("#ff000000").unwrap(), 0xFF00_0000);
        assert_eq!(parse_color("4278190080").unwrap(), 0xFF00_0000);
        assert!(parse_color("grey").is_err());
    }

    #[test]
    fn spot_text() {
        assert_eq!(
            spot_line("TCI-DIAG", "INFO", 433_920_000, 0xFF9E_9E9E, "rtl433 running"),
            "spot:TCI-DIAG,INFO,433920000,4288585374,rtl433 running;"
        );
    }

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
