//! The one receiver the bridge serves, seen from both sides: filled from TCI
//! status lines, read out as SDRconnect property values.
//!
//! Three kinds of property:
//! - **radio-bound** (centre, VFO, demodulator, sample rate) mirror the TCI
//!   server and change only when it says so;
//! - **local** ones (squelch, AGC, NR, LNA, antenna, display range, filter
//!   bandwidth …) have no TCI counterpart the bridge can drive. They are kept
//!   and echoed so a client's confirm loop succeeds, and they act on nothing;
//! - **measured or fixed** read-only ones (signal power and S/N from the
//!   bridge's own FFT, `api_version`, device names).
//!
//! Ends:     one shared picture of the receiver that every SDRconnect client reads from, kept true to what the
//!           TCI server reports.
//! Ways:     `apply` folds each TCI `Status` into fields and returns the property names that changed (for
//!           `property_changed` broadcasts); `value` renders any property as the API's string.
//! Means:    std, clap (`Lead` as a command-line value).
//! Provides: `State` (`new`, `apply`, `value`, `is_local`, `set_local`, `local_f32`, `bandwidth_hz`), `Lead`,
//!           `API_VERSION`, `RADIO_BOUND`, `tci_to_sdrc`, `sdrc_to_tci`.
//! Expects:  one `State` behind a mutex in main.rs `Bridge`; radio-bound fields written only by `apply` (the
//!           server's word), local ones only by clients via `set_local`.
//! Ensures:  `apply` reports a property only when its value really changed, so clients see no echo storms;
//!           unknown properties read as `None` (no answer), never as a made-up value.
//! Fails:    nothing returns an error; an unknown TCI mode reads as AM.
//! Tags:     #sdrconnect #tci #radio
//! See:      tci.rs `Status`, sdrc.rs `handle` / `set_property`, main.rs `tci_link`, `publish_spectrum`.

use std::collections::BTreeMap;

use crate::tci::Status;

/// Who may retune the radio.
///
/// Set once per bridge with `--lead`; the rtl_tcp side follows it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Lead {
    /// The operator tunes in the TCI front end; client tuning requests are
    /// answered with the unchanged value.
    Frontend,
    /// Clients may tune; their requests are forwarded as TCI commands.
    Client,
}

/// SDRconnect API version the bridge implements.
pub const API_VERSION: &str = "1.0.3";

/// Local properties and their values before any client sets them.
// Why: clients set these and then wait for the echo (confirm loop); a set on
//      an unknown property gets no echo at all. The values act on
//      nothing; `spectrum_ref_level` and `spectrum_base` are the exception,
//      read by main.rs `publish_spectrum` as the display range in dBFS.
const LOCAL_DEFAULTS: &[(&str, &str)] = &[
    ("lna_state", "0"),
    ("squelch_enable", "false"),
    ("squelch_threshold", "-100"),
    ("agc_enable", "false"),
    ("agc_threshold", "-30"),
    ("noise_reduction_enable", "false"),
    ("noise_reduction_strength", "0"),
    ("audio_volume_percent", "50"),
    ("audio_mute", "false"),
    ("audio_limiters", "false"),
    ("audio_filter", "false"),
    ("nfm_deemphasis_enable", "false"),
    ("wfm_stereo_enable", "false"),
    ("rds_enable", "false"),
    ("spectrum_ref_level", "-20"),
    ("spectrum_base", "-140"),
    ("active_antenna", "TCI"),
    ("am_lowcut_frequency", "0"),
    ("ssb_lowcut_frequency", "0"),
    ("nfm_lowcut_frequency", "0"),
];

/// Properties a client may set that steer the radio itself.
/// sdrc.rs turns a set on these into a TCI command instead of storing it.
pub const RADIO_BOUND: &[&str] = &[
    "device_center_frequency",
    "device_vfo_frequency",
    "demodulator",
    "device_sample_rate",
];

/// The receiver as the bridge knows it. Frequencies in Hz, rates in S/s, levels in dB.
#[derive(Debug, Clone)]
pub struct State {
    /// Who may retune (from `--lead`); shown to clients as `can_control`.
    pub lead: Lead,
    /// The TCI server has finished its status burst and the link is up.
    pub started: bool,
    /// Radio name from the server's `device` line; empty until it arrives.
    pub device: String,
    /// IQ stream centre (`dds` of receiver 0), Hz. 0 until the server reports it.
    pub center_hz: i64,
    /// VFO A of receiver 0, absolute, Hz.
    pub vfo_hz: i64,
    /// TCI modulation name, lower case.
    pub tci_mode: String,
    /// IQ rate the TCI server confirmed.
    pub iq_rate: u32,
    /// Receive filter edges relative to the VFO, when the server reports them.
    pub rx_filter: Option<(i64, i64)>,
    /// Bandwidth a client set; reported instead of the derived one.
    pub bandwidth_override: Option<u32>,
    /// Channel power from the bridge's own FFT, dBFS; refreshed once a second.
    pub signal_power_db: f32,
    /// Channel peak over the display median, dB; refreshed once a second.
    pub signal_snr_db: f32,
    /// Local property values as strings, keyed by SDRconnect name.
    local: BTreeMap<String, String>,
}

impl State {
    /// A receiver not yet heard from: not started, frequencies 0, mode AM,
    /// local properties at their defaults.
    pub fn new(lead: Lead) -> State {
        State {
            lead,
            started: false,
            device: String::new(),
            center_hz: 0,
            vfo_hz: 0,
            tci_mode: "am".into(),
            iq_rate: 0,
            rx_filter: None,
            bandwidth_override: None,
            signal_power_db: -140.0,
            signal_snr_db: 0.0,
            local: LOCAL_DEFAULTS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    /// Whether a client may set `prop` and have it kept: the local table, plus
    /// `filter_bandwidth`, which is stored as an override rather than in the table.
    pub fn is_local(&self, prop: &str) -> bool {
        self.local.contains_key(prop) || prop == "filter_bandwidth"
    }

    /// Store a local property. `filter_bandwidth` becomes the override.
    ///
    /// Pitfall: an unknown name is dropped silently (only known keys are
    /// updated); a `filter_bandwidth` that does not parse clears the override.
    pub fn set_local(&mut self, prop: &str, value: String) {
        if prop == "filter_bandwidth" {
            self.bandwidth_override = value.parse().ok();
        } else if let Some(v) = self.local.get_mut(prop) {
            *v = value;
        }
    }

    /// A local property as a number (e.g. `spectrum_base` in dBFS); `None` if
    /// unknown or not a number.
    pub fn local_f32(&self, prop: &str) -> Option<f32> {
        self.local.get(prop)?.parse().ok()
    }

    /// The receive bandwidth in Hz: a client's override, else the server's
    /// filter, else a typical width for the mode.
    pub fn bandwidth_hz(&self) -> u32 {
        if let Some(bw) = self.bandwidth_override {
            return bw;
        }
        // The server's filter edges are relative to the VFO; width = |hi − lo|.
        // A zero-width filter falls through to the mode's default.
        if let Some((lo, hi)) = self.rx_filter {
            let w = (hi - lo).unsigned_abs() as u32;
            if w > 0 {
                return w;
            }
        }
        default_bandwidth(&self.tci_mode)
    }

    /// The SDRconnect value of a property, as the string the API carries.
    /// `None` for properties the bridge does not know.
    pub fn value(&self, prop: &str) -> Option<String> {
        // "TCI_transcode (<radio>)", so a client's device list shows both the
        // bridge and the radio behind it.
        let device_name = || {
            if self.device.is_empty() {
                "TCI_transcode".to_string()
            } else {
                format!("TCI_transcode ({})", self.device)
            }
        };
        let v = match prop {
            "device_center_frequency" => self.center_hz.to_string(),
            "device_vfo_frequency" => self.vfo_hz.to_string(),
            "device_sample_rate" => self.iq_rate.to_string(),
            "demodulator" => tci_to_sdrc(&self.tci_mode).to_string(),
            "filter_bandwidth" => self.bandwidth_hz().to_string(),
            // The widest channel a client can ask for is the whole IQ stream.
            "demod_max_bandwidth" => self.iq_rate.to_string(),
            // No LNA control over TCI: a range of one step.
            "lna_state_min" | "lna_state_max" => "0".into(),
            "started" => self.started.to_string(),
            "overload" => "false".into(),
            "can_control" => (self.lead == Lead::Client).to_string(),
            "signal_power" => format!("{:.1}", self.signal_power_db),
            "signal_snr" => format!("{:.1}", self.signal_snr_db),
            "valid_antennas" => "TCI".into(),
            "valid_devices" | "active_device" => device_name(),
            "api_version" => API_VERSION.into(),
            // No WFM stereo or RDS decoding in the bridge: fixed empty values,
            // so clients that poll them get an answer.
            "wfm_stereo" => "false".into(),
            "rds_ps" | "rds_radiotext" => String::new(),
            "rds_pi" | "rds_pty" => "0".into(),
            // Everything else: a local property, or unknown (None → no reply).
            other => return self.local.get(other).cloned(),
        };
        Some(v)
    }

    /// Take in one TCI status line; returns the SDRconnect properties whose
    /// value it changed.
    ///
    /// Side effect: updates the fields. The caller broadcasts the returned names.
    // Invariant: each arm has a guard "only if different", so a server that
    //            repeats a line causes no broadcast to the clients.
    pub fn apply(&mut self, status: &Status) -> Vec<&'static str> {
        let mut changed = Vec::new();
        match status {
            Status::Device(d) if *d != self.device => {
                self.device = d.clone();
                changed.extend(["valid_devices", "active_device"]);
            }
            Status::Dds(hz) if *hz != self.center_hz => {
                self.center_hz = *hz;
                changed.push("device_center_frequency");
            }
            // VFO A only; channel 1 (VFO B) is not served.
            Status::Vfo { ch: 0, hz } if *hz != self.vfo_hz => {
                self.vfo_hz = *hz;
                changed.push("device_vfo_frequency");
            }
            // An IF offset gives the VFO too: centre + offset.
            // Pitfall: it uses the centre known at that moment; a later `dds`
            //          line alone does not move the VFO (no `if` line, no update).
            Status::If { ch: 0, hz } if self.center_hz + hz != self.vfo_hz => {
                self.vfo_hz = self.center_hz + hz;
                changed.push("device_vfo_frequency");
            }
            Status::Modulation(m) if *m != self.tci_mode => {
                self.tci_mode = m.clone();
                changed.push("demodulator");
                // The bandwidth follows the mode only while it is derived from it.
                if self.bandwidth_override.is_none() && self.rx_filter.is_none() {
                    changed.push("filter_bandwidth");
                }
            }
            Status::RxFilter { lo, hi } if self.rx_filter != Some((*lo, *hi)) => {
                self.rx_filter = Some((*lo, *hi));
                if self.bandwidth_override.is_none() {
                    changed.push("filter_bandwidth");
                }
            }
            Status::IqRate(r) if *r != self.iq_rate => {
                self.iq_rate = *r;
                changed.extend(["device_sample_rate", "demod_max_bandwidth"]);
            }
            Status::Ready | Status::Start if !self.started => {
                self.started = true;
                changed.push("started");
            }
            Status::Stop if self.started => {
                self.started = false;
                changed.push("started");
            }
            // Repeats, other channels, protocol lines: nothing to report.
            _ => {}
        }
        changed
    }
}

/// TCI modulation → SDRconnect demodulator (AM/USB/LSB/CW/SAM/NFM/WFM).
/// Digital sidebands map to their sideband; anything unknown reads as AM.
pub fn tci_to_sdrc(m: &str) -> &'static str {
    match m {
        "lsb" | "digl" => "LSB",
        "usb" | "digu" => "USB",
        "cw" => "CW",
        "sam" => "SAM",
        "nfm" | "fm" => "NFM",
        "wfm" => "WFM",
        _ => "AM",
    }
}

/// SDRconnect demodulator → TCI modulation, or `None` for an unknown name.
/// Case-insensitive, since clients differ.
pub fn sdrc_to_tci(m: &str) -> Option<&'static str> {
    Some(match m.to_ascii_uppercase().as_str() {
        "AM" => "am",
        "USB" => "usb",
        "LSB" => "lsb",
        "CW" => "cw",
        "SAM" => "sam",
        "NFM" => "nfm",
        "WFM" => "wfm",
        _ => return None,
    })
}

/// A typical receive bandwidth in Hz for a TCI mode, used when neither a
/// client nor the server has given one (feeds `filter_bandwidth` and the
/// channel window of `signal_power`).
fn default_bandwidth(tci_mode: &str) -> u32 {
    match tci_mode {
        "lsb" | "usb" | "digl" | "digu" => 2_700,
        "cw" => 500,
        "nfm" | "fm" => 12_500,
        "wfm" => 200_000,
        _ => 10_000,
    }
}

/// Proves: status lines change properties once; modes map both ways; bandwidth
/// prefers override, then filter, then mode; the lead shows in `can_control`.
#[cfg(test)]
mod tests {
    use super::*;

    /// Repeats and other channels report nothing; an IF that matches the VFO is no change.
    #[test]
    fn status_lines_change_properties_once() {
        let mut s = State::new(Lead::Frontend);
        assert_eq!(s.apply(&Status::Dds(433_900_000)), vec!["device_center_frequency"]);
        assert!(s.apply(&Status::Dds(433_900_000)).is_empty());
        assert_eq!(
            s.apply(&Status::Vfo { ch: 0, hz: 433_920_000 }),
            vec!["device_vfo_frequency"]
        );
        assert!(s.apply(&Status::If { ch: 0, hz: 20_000 }).is_empty());
        assert!(s.apply(&Status::Vfo { ch: 1, hz: 1 }).is_empty());
        assert_eq!(s.value("device_vfo_frequency").unwrap(), "433920000");
    }

    /// Every SDRconnect mode survives the round trip; DIGU reads as USB; unknown names are refused.
    #[test]
    fn modes_map_both_ways() {
        for m in ["AM", "USB", "LSB", "CW", "SAM", "NFM", "WFM"] {
            assert_eq!(tci_to_sdrc(sdrc_to_tci(m).unwrap()), m);
        }
        assert_eq!(tci_to_sdrc("digu"), "USB");
        assert_eq!(sdrc_to_tci("PACKET"), None);
    }

    /// NFM default 12.5 kHz → server filter 12 kHz → client override 8 kHz.
    #[test]
    fn bandwidth_prefers_override_then_filter_then_mode() {
        let mut s = State::new(Lead::Frontend);
        s.apply(&Status::Modulation("nfm".into()));
        assert_eq!(s.bandwidth_hz(), 12_500);
        s.apply(&Status::RxFilter { lo: -6_000, hi: 6_000 });
        assert_eq!(s.bandwidth_hz(), 12_000);
        s.set_local("filter_bandwidth", "8000".into());
        assert_eq!(s.bandwidth_hz(), 8_000);
    }

    /// Clients see whether they may tune.
    #[test]
    fn lead_shows_in_can_control() {
        assert_eq!(State::new(Lead::Frontend).value("can_control").unwrap(), "false");
        assert_eq!(State::new(Lead::Client).value("can_control").unwrap(), "true");
    }
}
