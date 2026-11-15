//! Gathers the small blocks a TCI server sends into frames of a set duration.
//!
//! SDR Oxide sends IQ in blocks of a few hundred samples, 200–400 a second.
//! Passed on one for one, they fill an SDRconnect client's queue in a fraction
//! of a second: msdr keeps 40 IQ frames, which at that block size is about
//! 0.15 s, and dropped 958 frames in six minutes behind a brief stall (measured
//! 14.11.). Gathered to 20 ms, the same 40 frames hold 0.8 s.

/// How much signal one outgoing frame carries.
pub const FRAME_SECONDS: f64 = 0.020;

/// Collects interleaved float samples of one stream at one rate.
pub struct Chunker {
    /// Values per sample frame: 2 for IQ and for stereo audio.
    width: usize,
    rate: u32,
    buf: Vec<f32>,
}

impl Chunker {
    pub fn new(width: usize) -> Chunker {
        Chunker { width, rate: 0, buf: Vec::new() }
    }

    /// Add a block at `rate`; returns a full frame when one is ready. A change
    /// of rate hands out what was gathered at the old rate first, so no frame
    /// ever mixes two rates.
    pub fn push(&mut self, block: &[f32], rate: u32) -> Option<Vec<f32>> {
        let mut out = None;
        if rate != self.rate {
            if !self.buf.is_empty() {
                out = Some(std::mem::take(&mut self.buf));
            }
            self.rate = rate;
        }
        self.buf.extend_from_slice(block);
        if out.is_none() && self.buf.len() >= self.target() {
            out = Some(std::mem::take(&mut self.buf));
        }
        out
    }

    fn target(&self) -> usize {
        ((self.rate as f64 * FRAME_SECONDS) as usize).max(64) * self.width
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_blocks_leave_as_twenty_millisecond_frames() {
        let mut c = Chunker::new(2);
        let block = vec![0.0f32; 2 * 126];
        let mut frames = Vec::new();
        for _ in 0..200 {
            if let Some(f) = c.push(&block, 250_000) {
                frames.push(f.len() / 2);
            }
        }
        // 200 × 126 = 25,200 samples; 5,000 per frame at 250 k.
        assert_eq!(frames.len(), 5);
        assert!(frames.iter().all(|&n| (5_000..5_126).contains(&n)), "{frames:?}");
    }

    #[test]
    fn a_rate_change_flushes_first() {
        let mut c = Chunker::new(2);
        assert!(c.push(&[1.0; 20], 250_000).is_none());
        let f = c.push(&[2.0; 20], 500_000).unwrap();
        assert_eq!(f, vec![1.0; 20]);
    }
}
