//! The spectrum stream SDRconnect clients expect and TCI does not carry,
//! computed from the IQ: 512 bins, lowest frequency first, about 23 frames a
//! second, uint8 levels across the display range. Also the channel's power
//! and S/N for `signal_power` and `signal_snr`.

use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex32};

pub const BINS: usize = 512;
/// Frame rate SDRconnect was measured to send (msdr notes, 2026-09-23).
pub const FRAMES_PER_SECOND: u32 = 23;

pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Power normalisation: a full-scale tone reads 0 dBFS.
    norm: f32,
    pending: Vec<Complex32>,
    scratch: Vec<Complex32>,
    acc: Vec<f32>,
    frames: u32,
    since_emit: usize,
    rate: u32,
}

impl Spectrum {
    pub fn new() -> Spectrum {
        let fft = FftPlanner::new().plan_fft_forward(BINS);
        let window: Vec<f32> = (0..BINS)
            .map(|i| {
                let x = std::f32::consts::PI * 2.0 * i as f32 / BINS as f32;
                0.5 - 0.5 * x.cos()
            })
            .collect();
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
    pub fn feed(&mut self, iq: &[f32], rate: u32) -> Option<Vec<f32>> {
        if rate == 0 {
            return None;
        }
        if rate != self.rate {
            self.rate = rate;
            self.pending.clear();
            self.acc.iter_mut().for_each(|a| *a = 0.0);
            self.frames = 0;
            self.since_emit = 0;
        }
        self.pending.extend(iq.chunks_exact(2).map(|c| Complex32::new(c[0], c[1])));
        self.since_emit += iq.len() / 2;
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
        let due = (self.rate / FRAMES_PER_SECOND).max(BINS as u32) as usize;
        if self.since_emit < due || self.frames == 0 {
            return None;
        }
        let k = self.norm / self.frames as f32;
        // FFT order puts 0 Hz first; the display wants the lowest frequency first.
        let db = (0..BINS)
            .map(|i| {
                let p = self.acc[(i + BINS / 2) % BINS] * k;
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
pub fn to_bytes(db: &[f32], base: f32, reference: f32) -> Vec<u8> {
    let span = (reference - base).max(1.0);
    db.iter().map(|d| ((d - base) / span * 255.0).clamp(0.0, 255.0).round() as u8).collect()
}

/// Power in the channel (dBFS, summed over its bins) and its S/N against the
/// median of the whole display (dB). `offset_hz` is the VFO's distance from
/// the centre.
pub fn channel_metrics(db: &[f32], rate: u32, offset_hz: f64, bandwidth_hz: u32) -> (f32, f32) {
    let n = db.len();
    if n == 0 || rate == 0 {
        return (-140.0, 0.0);
    }
    let bin_hz = rate as f64 / n as f64;
    let mid = n as f64 / 2.0 + offset_hz / bin_hz;
    let half = (bandwidth_hz as f64 / 2.0 / bin_hz).max(0.5);
    let lo = (mid - half).floor().max(0.0) as usize;
    let hi = ((mid + half).ceil() as usize).min(n);
    if lo >= hi {
        return (-140.0, 0.0);
    }
    let lin: f32 = db[lo..hi].iter().map(|d| 10f32.powf(d / 10.0)).sum();
    let power = 10.0 * lin.max(1e-20).log10();
    let mut sorted = db.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let floor = sorted[n / 2];
    let peak = db[lo..hi].iter().copied().fold(f32::MIN, f32::max);
    (power, peak - floor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, hz: f64, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let ph = 2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64;
                [amp * ph.cos() as f32, amp * ph.sin() as f32]
            })
            .collect()
    }

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

    #[test]
    fn bytes_span_the_display_range() {
        assert_eq!(to_bytes(&[-140.0, -80.0, -20.0, 0.0], -140.0, -20.0), vec![0, 128, 255, 255]);
    }
}
