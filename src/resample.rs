//! Changes the rate of a complex IQ stream by any ratio, e.g. the 2.000 MS/s a
//! TCI server snaps to into the 2.048 MS/s a DAB decoder insists on.
//!
//! A windowed-sinc interpolator: 32 taps, Kaiser window (β 8), the kernel
//! tabulated at 256 sub-sample phases and the nearest phase taken. The cut-off
//! sits at 46 % of the lower of the two rates, so going down it is also the
//! anti-alias filter. Each phase is normalised to unit DC gain.
//!
//! Ends:     hand an rtl_tcp client exactly the sample rate it asked for, whatever integer decimation the TCI
//!           server snapped to.
//! Ways:     polyphase windowed sinc, nearest of 257 tabulated phases, fractional read position carried across
//!           blocks; equal rates pass through untouched.
//! Means:    std only (own Kaiser window and Bessel I0).
//! Provides: `Resampler::new(in_rate, out_rate)`, `process(iq, out)`, `rates()`.
//! Expects:  interleaved I,Q float32, full scale ±1; one `Resampler` per stream and rate pair (build a new one
//!           when either rate changes, as rtltcp.rs does).
//! Ensures:  out/in sample count → out_rate/in_rate over a long run; unit gain at DC; passband flat to about
//!           ±0.7 MHz at 2.048 MS/s (tests: 700 kHz within 1 %, 600 kHz from 5 MS/s within 2 %).
//! Fails:    nothing returns an error. A delay of about 16 input samples is added; the first output samples
//!           after construction ramp in from the zero history.
//! Tags:     #dsp #rtltcp
//! See:      rtltcp.rs `client` (the only user), tests below.
//
// Why: HDZ 2026-11-16, TCI_transcode as an rtl_tcp server for AbracaDABra: a
//      TCI server's IQ rate is an integer decimation of its hardware rate
//      (SDR Oxide: 6 MS/s / 3 = 2.000, 10 MS/s / 5 = 2.000), while a DAB
//      receiver needs exactly 2.048 MS/s; "die Rate muss hoch, sonst ist die
//      Schnittstelle nutzlos".

/// Half the kernel length, in input samples (taps each side of the output position).
const HALF: usize = 16;
/// Kernel length in input samples (32).
const TAPS: usize = 2 * HALF;
/// Sub-sample phases tabulated; the nearest is taken (time error ≤ 1/512 sample).
const PHASES: usize = 256;
/// Kaiser window shape; 8 gives roughly 80 dB stop-band on the window itself.
const BETA: f64 = 8.0;

/// Rate changer for one complex IQ stream; state carries across `process` calls.
pub struct Resampler {
    /// Input rate, S/s.
    in_rate: u32,
    /// Output rate, S/s.
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
    /// Build the kernel table for `in_rate` → `out_rate` (both S/s, > 0).
    /// Cost: 257 × 32 kernel values with a Bessel series each; done once per rate pair.
    pub fn new(in_rate: u32, out_rate: u32) -> Resampler {
        let step = in_rate as f64 / out_rate as f64;
        // Cut-off as a fraction of the input rate (cycles per input sample):
        // 0.46 of the input going up, 0.46 of the output going down. In Hz that
        // is 46 % of the lower rate, below the output's Nyquist at 50 %.
        let cutoff = 0.46 * (out_rate as f64 / in_rate as f64).min(1.0); // of the input rate
        let mut table = Vec::with_capacity((PHASES + 1) * TAPS);
        // One row per phase 0/256 … 256/256; the last row (frac 1.0) exists so
        // rounding frac up to a full sample never indexes past the table.
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let row: Vec<f64> = (0..TAPS)
                .map(|k| {
                    // Tap k weighs input sample floor(pos) - HALF + 1 + k.
                    let x = (k as f64 - (HALF as f64 - 1.0)) - frac;
                    // sinc(2·cutoff·x); the limit 1 at x = 0 is taken explicitly.
                    let sinc = if x.abs() < 1e-12 {
                        1.0
                    } else {
                        let a = std::f64::consts::PI * 2.0 * cutoff * x;
                        a.sin() / a
                    };
                    // x/HALF lies in [−1, 1] for every tap and phase, inside the window.
                    sinc * kaiser(x / HALF as f64, BETA)
                })
                .collect();
            // Why: normalise each row to sum 1 (unit DC gain). Without it the
            //      gain wobbles with the phase and a steady carrier picks up
            //      amplitude ripple at the beat of the two rates.
            let sum: f64 = row.iter().sum();
            table.extend(row.iter().map(|v| (v / sum) as f32));
        }
        // Start with HALF − 1 zero samples of history (indices 0 … 14) and the
        // first output on index 15, the first real input sample: the kernel's
        // left half then reads zeros instead of indexing before the buffer.
        Resampler {
            in_rate,
            out_rate,
            step,
            pos: (HALF - 1) as f64,
            buf: vec![0.0; 2 * (HALF - 1)],
            table,
        }
    }

    /// The (input, output) rates in S/s this resampler was built for; the
    /// caller compares them to decide whether a new one is needed.
    pub fn rates(&self) -> (u32, u32) {
        (self.in_rate, self.out_rate)
    }

    /// Feed interleaved I,Q at `in_rate`; append interleaved I,Q at `out_rate`.
    ///
    /// Appends to `out` (never clears it). Blocks may have any length; the
    /// fractional position and the last ~16 input samples carry to the next call.
    pub fn process(&mut self, iq: &[f32], out: &mut Vec<f32>) {
        // Equal rates: a copy, bit-exact and without the filter's delay.
        if self.in_rate == self.out_rate {
            out.extend_from_slice(iq);
            return;
        }
        self.buf.extend_from_slice(iq);
        // n = complex samples in the buffer (history + new block).
        let n = self.buf.len() / 2;
        // An output needs input up to floor(pos) + HALF; stop when that is not here yet.
        while (self.pos.floor() as usize) + HALF < n {
            // First input sample under the kernel.
            let base = self.pos.floor() as usize + 1 - HALF;
            // Sub-sample position → nearest tabulated phase (0 … 256).
            let frac = self.pos - self.pos.floor();
            let p = (frac * PHASES as f64).round() as usize;
            let row = &self.table[p * TAPS..(p + 1) * TAPS];
            // Real kernel, so I and Q are filtered alike with the same taps.
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
        // Invariant: after the drain, pos stays relative to buf's new start, so
        //            the fractional position runs on without a jump across blocks.
        let keep_from = (self.pos.floor() as usize + 1).saturating_sub(HALF);
        self.buf.drain(..2 * keep_from);
        self.pos -= keep_from as f64;
    }
}

/// Kaiser window at `t` in [-1, 1].
/// Returns 1 at the centre, falling to I0(0)/I0(β) at the edges; 0 outside.
fn kaiser(t: f64, beta: f64) -> f64 {
    if t.abs() > 1.0 {
        return 0.0;
    }
    bessel_i0(beta * (1.0 - t * t).sqrt()) / bessel_i0(beta)
}

/// Modified Bessel function of the first kind, order 0, by its power series.
/// 40 terms are far more than needed for the arguments here (|x| ≤ β = 8).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    // Term k = ((x/2)^k / k!)², built from the previous term.
    for k in 1..40 {
        term *= (x / (2.0 * k as f64)).powi(2);
        sum += term;
    }
    sum
}

/// Proves: 2.000 → 2.048 MS/s keeps frequency and level; 5 → 2.048 MS/s
/// rejects a tone that would alias and passes one in band; equal rates copy.
#[cfg(test)]
mod tests {
    use super::*;

    /// A complex tone of `n` samples at `f` Hz, amplitude 1, sampled at `rate` S/s.
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
        // Sum of z[n]·conj(z[n−1]); its angle is the mean phase step per sample.
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

    /// The case that motivated the module: SDR Oxide's 2.000 MS/s into DAB's 2.048 MS/s.
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
        // Skip the first 1000 outputs: the ramp-in from the zero history.
        let (f, amp) = measure(&out[2 * 1000..], 2_048_000);
        assert!((f - 700_000.0).abs() < 50.0, "tone at {f} Hz");
        assert!((amp - 1.0).abs() < 0.01, "amplitude {amp}");
    }

    /// The offset-tuning case: a 5 MS/s TCI stream down to 2.048 MS/s.
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

    /// Equal rates must be a bit-exact copy (no filter, no delay).
    #[test]
    fn equal_rates_pass_through() {
        let mut r = Resampler::new(2_048_000, 2_048_000);
        let input = tone(100_000.0, 2_048_000, 1000);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        assert_eq!(out, input);
    }
}
