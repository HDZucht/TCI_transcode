//! The spectrum stream SDRconnect clients expect and TCI does not carry,
//! computed from the IQ: 512 bins, lowest frequency first, about 23 frames a
//! second, uint8 levels across the display range. Also the channel's power
//! and S/N for `signal_power` and `signal_snr`.
//!
//! Ends:     give SDRconnect clients the spectrum frames and the signal power / S/N readings SDRconnect sends,
//!           which a TCI server does not provide.
//! Ways:     512-point FFT over non-overlapping Hann-windowed blocks of the IQ, power averaged over all blocks
//!           since the last display frame, shifted so the lowest frequency comes first, then scaled to uint8.
//! Means:    rustfft.
//! Provides: `Spectrum::new()`, `Spectrum::feed(iq, rate) -> Option<dBFS per bin>`, `to_bytes`, `channel_metrics`,
//!           `BINS`, `FRAMES_PER_SECOND`.
//! Expects:  interleaved float32 I,Q, full scale ±1, at the rate the TCI frame header carries.
//! Ensures:  a full-scale tone reads 0 dBFS in its bin; ~23 frames per second regardless of block size; a rate
//!           change discards what was gathered at the old rate.
//! Fails:    rate 0 yields no frame; an empty channel window yields (−140 dBFS, 0 dB).
//! Tags:     #dsp #sdrconnect
//! See:      main.rs `tci_link` / `publish_spectrum` (caller), sdrc.rs `PAYLOAD_SPECTRUM`, state.rs (`spectrum_base`,
//!           `spectrum_ref_level`, `signal_power`, `signal_snr`).

use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex32};

/// Bins per spectrum frame, which is also the FFT length (512).
pub const BINS: usize = 512;
/// Frame rate SDRconnect was measured to send (msdr notes, 2026-09-23).
pub const FRAMES_PER_SECOND: u32 = 23;

/// Running spectrum of one IQ stream: FFTs blocks as they arrive and averages
/// their power until a display frame is due.
pub struct Spectrum {
    /// Forward FFT of length `BINS`, planned once.
    fft: Arc<dyn Fft<f32>>,
    /// Hann window, `BINS` long.
    window: Vec<f32>,
    /// Power normalisation: a full-scale tone reads 0 dBFS.
    norm: f32,
    /// Complex samples not yet transformed (fewer than `BINS` after each `feed`).
    pending: Vec<Complex32>,
    /// FFT work buffer, `BINS` long.
    scratch: Vec<Complex32>,
    /// Power per bin summed over `frames` FFTs, in FFT order (0 Hz first).
    acc: Vec<f32>,
    /// FFTs summed into `acc` since the last display frame.
    frames: u32,
    /// Complex samples fed since the last display frame (minus what that frame used up).
    since_emit: usize,
    /// Rate of the samples in `pending` / `acc`, S/s; 0 before the first feed.
    rate: u32,
}

impl Spectrum {
    /// Plan the FFT and build the Hann window and its normalisation.
    pub fn new() -> Spectrum {
        let fft = FftPlanner::new().plan_fft_forward(BINS);
        // Periodic Hann window: 0.5 − 0.5·cos(2πi/N).
        let window: Vec<f32> = (0..BINS)
            .map(|i| {
                let x = std::f32::consts::PI * 2.0 * i as f32 / BINS as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
        // Why: a tone of amplitude 1 in one bin gives |X|² = (Σw)², so 1/(Σw)²
        //      makes it read 0 dBFS (the test below holds it to ±0.5 dB).
        let sum: f32 = window.iter().sum();
        Spectrum {
            fft,
            norm: 1.0 / (sum * sum),
            window,
            pending: Vec::with_capacity(BINS * 2),
            scratch: vec![Complex32::default(); BINS],
            acc: vec![0.0; BINS],
            frames: 0,
            since_emit: 0,
            rate: 0,
        }
    }

    /// Feed interleaved float32 I,Q at `rate`. Returns the averaged power per
    /// bin in dBFS, lowest frequency first, whenever a display frame is due.
    ///
    /// A frame is due every `rate / 23` complex samples (at least `BINS`), so the
    /// frame rate follows the signal's time, not the arrival of blocks.
    pub fn feed(&mut self, iq: &[f32], rate: u32) -> Option<Vec<f32>> {
        if rate == 0 {
            return None;
        }
        // Rate change: drop everything gathered at the old rate, since bins of
        // two rates mean different frequencies and must not be averaged.
        if rate != self.rate {
            self.rate = rate;
            self.pending.clear();
            self.acc.iter_mut().for_each(|a| *a = 0.0);
            self.frames = 0;
            self.since_emit = 0;
        }
        self.pending.extend(iq.chunks_exact(2).map(|c| Complex32::new(c[0], c[1])));
        self.since_emit += iq.len() / 2;
        // Transform every full block of BINS samples, without overlap; the rest
        // waits in `pending` for the next feed.
        while self.pending.len() >= BINS {
            for (i, (s, w)) in self.pending[..BINS].iter().zip(&self.window).enumerate() {
                self.scratch[i] = s * w;
            }
            self.pending.drain(..BINS);
            self.fft.process(&mut self.scratch);
            for (a, x) in self.acc.iter_mut().zip(&self.scratch) {
                *a += x.norm_sqr();
            }
            self.frames += 1;
        }
        // Samples per display frame at 23 frames/s, never fewer than one FFT.
        let due = (self.rate / FRAMES_PER_SECOND).max(BINS as u32) as usize;
        if self.since_emit < due || self.frames == 0 {
            return None;
        }
        // Mean power per FFT, normalised to dBFS.
        let k = self.norm / self.frames as f32;
        // FFT order puts 0 Hz first; the display wants the lowest frequency first.
        let db = (0..BINS)
            .map(|i| {
                let p = self.acc[(i + BINS / 2) % BINS] * k;
                // Floor at −200 dBFS so an all-zero bin gives a number, not −inf.
                10.0 * p.max(1e-20).log10()
            })
            .collect();
        self.acc.iter_mut().for_each(|a| *a = 0.0);
        self.frames = 0;
        // Carry the remainder, so the frame rate holds whatever the block size.
        self.since_emit -= due;
        Some(db)
    }
}

/// dBFS per bin → uint8 across [base, ref].
///
/// `base` maps to 0, `reference` to 255, linear in dB; values outside clip.
/// A span under 1 dB is treated as 1 dB, so base ≥ ref cannot divide by zero.
pub fn to_bytes(db: &[f32], base: f32, reference: f32) -> Vec<u8> {
    let span = (reference - base).max(1.0);
    db.iter().map(|d| ((d - base) / span * 255.0).clamp(0.0, 255.0).round() as u8).collect()
}

/// Power in the channel (dBFS, summed over its bins) and its S/N against the
/// median of the whole display (dB). `offset_hz` is the VFO's distance from
/// the centre.
///
/// In: `db` a display frame (lowest frequency first), `rate` S/s, `offset_hz`
/// Hz, `bandwidth_hz` Hz. Out: (power dBFS, S/N dB). The "S/N" is the
/// channel's highest bin over the display's median bin, not a power ratio.
/// A channel outside the display returns (−140, 0).
pub fn channel_metrics(db: &[f32], rate: u32, offset_hz: f64, bandwidth_hz: u32) -> (f32, f32) {
    let n = db.len();
    if n == 0 || rate == 0 {
        return (-140.0, 0.0);
    }
    // Bin width, and the VFO's bin (the display centre is bin n/2).
    let bin_hz = rate as f64 / n as f64;
    let mid = n as f64 / 2.0 + offset_hz / bin_hz;
    // At least half a bin each side, so a narrow channel still covers one bin.
    let half = (bandwidth_hz as f64 / 2.0 / bin_hz).max(0.5);
    let lo = (mid - half).floor().max(0.0) as usize;
    // Pitfall: a channel far below the display gives a negative float here;
    //          `as usize` saturates it to 0, and the lo ≥ hi check below catches it.
    let hi = ((mid + half).ceil() as usize).min(n);
    if lo >= hi {
        return (-140.0, 0.0);
    }
    // Sum in linear power, then back to dB.
    let lin: f32 = db[lo..hi].iter().map(|d| 10f32.powf(d / 10.0)).sum();
    let power = 10.0 * lin.max(1e-20).log10();
    // Noise floor = median bin of the whole display: robust against a few strong carriers.
    let mut sorted = db.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let floor = sorted[n / 2];
    let peak = db[lo..hi].iter().copied().fold(f32::MIN, f32::max);
    (power, peak - floor)
}

/// Proves: a full-scale tone lands in its bin at 0 dBFS with high S/N; frames
/// come at ~23 per second; the byte scale spans the display range.
#[cfg(test)]
mod tests {
    use super::*;

    /// `n` complex samples of a tone at `hz`, amplitude `amp`, sampled at `rate` S/s.
    fn tone(rate: u32, hz: f64, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let ph = 2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64;
                [amp * ph.cos() as f32, amp * ph.sin() as f32]
            })
            .collect()
    }

    /// One second at 256 kS/s, tone at +32 kHz: bin 320, 0 dBFS, S/N above 60 dB.
    #[test]
    fn a_tone_lands_in_its_bin_at_its_level() {
        let rate = 256_000;
        let hz = 32_000.0; // bin 256 + 32000/500 = 320
        let mut s = Spectrum::new();
        let db = s.feed(&tone(rate, hz, rate as usize, 1.0), rate).unwrap();
        let (peak_bin, peak) =
            db.iter().enumerate().fold((0, f32::MIN), |b, (i, &v)| if v > b.1 { (i, v) } else { b });
        assert_eq!(peak_bin, 320);
        assert!(peak.abs() < 0.5, "full-scale tone read {peak} dBFS");
        let (_, snr) = channel_metrics(&db, rate, hz, 2_000);
        assert!(snr > 60.0, "S/N {snr}");
    }

    /// 100 blocks of 10 ms at 230 kS/s give 22–24 frames.
    #[test]
    fn frames_come_at_the_display_rate() {
        let rate = 230_000;
        let mut s = Spectrum::new();
        let frames = (0..100)
            .filter(|_| s.feed(&tone(rate, 1_000.0, 2_300, 0.1), rate).is_some())
            .count();
        // 100 blocks of 10 ms = 1 s.
        assert!((22..=24).contains(&frames), "{frames} frames");
    }

    /// Base → 0, midpoint → 128, ref → 255, above ref clips to 255.
    #[test]
    fn bytes_span_the_display_range() {
        assert_eq!(to_bytes(&[-140.0, -80.0, -20.0, 0.0], -140.0, -20.0), vec![0, 128, 255, 255]);
    }
}
