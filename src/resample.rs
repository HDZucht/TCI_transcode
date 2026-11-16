//! Changes the rate of a complex IQ stream by any ratio, e.g. the 2.000 MS/s a
//! TCI server snaps to into the 2.048 MS/s a DAB decoder insists on.
//!
//! A windowed-sinc interpolator: 32 taps, Kaiser window (β 8), the kernel
//! tabulated at 256 sub-sample phases and the nearest phase taken. The cut-off
//! sits at 46 % of the lower of the two rates, so going down it is also the
//! anti-alias filter. Each phase is normalised to unit DC gain.
//
// Why: HDZ 2026-11-16, TCI_transcode as an rtl_tcp server for AbracaDABra: a
//      TCI server's IQ rate is an integer decimation of its hardware rate
//      (SDR Oxide: 6 MS/s / 3 = 2.000, 10 MS/s / 5 = 2.000), while a DAB
//      receiver needs exactly 2.048 MS/s; "die Rate muss hoch, sonst ist die
//      Schnittstelle nutzlos".

const HALF: usize = 16;
const TAPS: usize = 2 * HALF;
const PHASES: usize = 256;
const BETA: f64 = 8.0;

pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    /// Input samples per output sample.
    step: f64,
    /// Position of the next output, in input samples from the start of `buf`.
    pos: f64,
    /// Interleaved I,Q history plus the current block.
    buf: Vec<f32>,
    /// `PHASES + 1` rows of `TAPS` coefficients.
    table: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Resampler {
        let step = in_rate as f64 / out_rate as f64;
        let cutoff = 0.46 * (out_rate as f64 / in_rate as f64).min(1.0); // of the input rate
        let mut table = Vec::with_capacity((PHASES + 1) * TAPS);
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row: Vec<f64> = (0..TAPS)
                .map(|k| {
                    // Tap k weighs input sample floor(pos) - HALF + 1 + k.
                    let x = (k as f64 - (HALF as f64 - 1.0)) - frac;
                    let sinc = if x.abs() < 1e-12 {
                        1.0
                    } else {
                        let a = std::f64::consts::PI * 2.0 * cutoff * x;
                        a.sin() / a
                    };
                    sinc * kaiser(x / HALF as f64, BETA)
                })
                .collect();
            let sum: f64 = row.iter().sum();
            table.extend(row.iter().map(|v| (v / sum) as f32));
        }
        Resampler {
            in_rate,
            out_rate,
            step,
            pos: (HALF - 1) as f64,
            buf: vec![0.0; 2 * (HALF - 1)],
            table,
        }
    }

    pub fn rates(&self) -> (u32, u32) {
        (self.in_rate, self.out_rate)
    }

    /// Feed interleaved I,Q at `in_rate`; append interleaved I,Q at `out_rate`.
    pub fn process(&mut self, iq: &[f32], out: &mut Vec<f32>) {
        if self.in_rate == self.out_rate {
            out.extend_from_slice(iq);
            return;
        }
        self.buf.extend_from_slice(iq);
        let n = self.buf.len() / 2;
        while (self.pos.floor() as usize) + HALF < n {
            let base = self.pos.floor() as usize + 1 - HALF;
            let frac = self.pos - self.pos.floor();
            let p = (frac * PHASES as f64).round() as usize;
            let row = &self.table[p * TAPS..(p + 1) * TAPS];
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (k, &c) in row.iter().enumerate() {
                let j = 2 * (base + k);
                re += c * self.buf[j];
                im += c * self.buf[j + 1];
            }
            out.push(re);
            out.push(im);
            self.pos += self.step;
        }
        // Keep what the next outputs still reach back to.
        let keep_from = (self.pos.floor() as usize + 1).saturating_sub(HALF);
        self.buf.drain(..2 * keep_from);
        self.pos -= keep_from as f64;
    }
}

/// Kaiser window at `t` in [-1, 1].
fn kaiser(t: f64, beta: f64) -> f64 {
    if t.abs() > 1.0 {
        return 0.0;
    }
    bessel_i0(beta * (1.0 - t * t).sqrt()) / bessel_i0(beta)
}

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    for k in 1..40 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f: f64, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let ph = std::f64::consts::TAU * f * i as f64 / rate as f64;
                [ph.cos() as f32, ph.sin() as f32]
            })
            .collect()
    }

    /// Frequency (Hz) and amplitude of a complex tone, from the mean phase step.
    fn measure(iq: &[f32], rate: u32) -> (f64, f64) {
        let c: Vec<(f64, f64)> = iq.chunks(2).map(|v| (v[0] as f64, v[1] as f64)).collect();
        let mut acc = (0.0, 0.0);
        for w in c.windows(2) {
            let (a, b) = (w[0], w[1]);
            acc.0 += b.0 * a.0 + b.1 * a.1;
            acc.1 += b.1 * a.0 - b.0 * a.1;
        }
        let f = acc.1.atan2(acc.0) * rate as f64 / std::f64::consts::TAU;
        let amp = (c.iter().map(|v| v.0 * v.0 + v.1 * v.1).sum::<f64>() / c.len() as f64).sqrt();
        (f, amp)
    }

    #[test]
    fn up_from_2000k_to_2048k_keeps_frequency_and_level() {
        // A DAB block edge: 700 kHz off centre.
        let mut r = Resampler::new(2_000_000, 2_048_000);
        let input = tone(700_000.0, 2_000_000, 200_000);
        let mut out = Vec::new();
        // In uneven pieces, as TCI frames arrive.
        for piece in input.chunks(2 * 977) {
            r.process(piece, &mut out);
        }
        let n_out = out.len() / 2;
        let expected = 200_000.0 * 2.048 / 2.0;
        assert!((n_out as f64 - expected).abs() < 40.0, "{n_out} outputs, expected ~{expected}");
        let (f, amp) = measure(&out[2 * 1000..], 2_048_000);
        assert!((f - 700_000.0).abs() < 50.0, "tone at {f} Hz");
        assert!((amp - 1.0).abs() < 0.01, "amplitude {amp}");
    }

    #[test]
    fn down_from_5000k_rejects_what_would_alias() {
        // 1.7 MHz lies outside 2.048 MS/s and must not fold back in.
        let mut r = Resampler::new(5_000_000, 2_048_000);
        let mut out = Vec::new();
        r.process(&tone(1_700_000.0, 5_000_000, 100_000), &mut out);
        let (_, amp) = measure(&out[2 * 1000..], 2_048_000);
        assert!(amp < 0.01, "alias at {:.1} dB", 20.0 * amp.log10());
        // And 600 kHz passes.
        let mut r = Resampler::new(5_000_000, 2_048_000);
        let mut out = Vec::new();
        r.process(&tone(600_000.0, 5_000_000, 100_000), &mut out);
        let (f, amp) = measure(&out[2 * 1000..], 2_048_000);
        assert!((f - 600_000.0).abs() < 50.0 && (amp - 1.0).abs() < 0.02, "{f} Hz, {amp}");
    }

    #[test]
    fn equal_rates_pass_through() {
        let mut r = Resampler::new(2_048_000, 2_048_000);
        let input = tone(100_000.0, 2_048_000, 1000);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }
}
