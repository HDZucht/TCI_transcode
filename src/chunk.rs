//! Gathers the small blocks a TCI server sends into frames of a set duration.
//!
//! SDR Oxide sends IQ in blocks of a few hundred samples, 200–400 a second.
//! Passed on one for one, they fill an SDRconnect client's queue in a fraction
//! of a second: msdr keeps 40 IQ frames, which at that block size is about
//! 0.15 s, and dropped 958 frames in six minutes behind a brief stall (measured
//! 14.11.). Gathered to 20 ms, the same 40 frames hold 0.8 s.
//!
//! Ends:     turn many small TCI blocks into fewer 20 ms frames, so a client with a fixed-length frame queue
//!           (msdr: 40 frames) holds 0.8 s of signal instead of 0.15 s.
//! Ways:     one growing buffer per stream; hand it out whole once it holds 20 ms at the current rate.
//! Means:    std only.
//! Provides: `Chunker::new(width)`, `Chunker::push(block, rate) -> Option<frame>`, `FRAME_SECONDS`.
//! Expects:  interleaved float samples, `width` values per sample frame (2 for IQ and stereo audio); `rate` in
//!           sample frames per second (S/s for IQ, Hz for audio).
//! Ensures:  a frame never mixes two rates; nothing is dropped or reordered; frames hold at least 20 ms (or 64
//!           sample frames at very low rates), plus at most one input block.
//! Fails:    nothing to fail; a stream that stops leaves its last partial frame in the buffer until the next push.
//! Tags:     #sdrconnect #dsp
//! See:      main.rs `tci_link` (two chunkers: IQ and audio), sdrc.rs `iq_frame` / `audio_frame`.

/// How much signal one outgoing frame carries, in seconds (20 ms).
// Why: msdr keeps 40 frames; 40 × 20 ms = 0.8 s of buffer against a brief
//      stall. Longer frames would add latency without a measured need.
pub const FRAME_SECONDS: f64 = 0.020;

/// Collects interleaved float samples of one stream at one rate.
///
/// One instance per stream (IQ, audio); `push` returns a full frame or `None`.
pub struct Chunker {
    /// Values per sample frame: 2 for IQ and for stereo audio.
    width: usize,
    /// Rate of the samples now in `buf`, in sample frames per second; 0 before the first push.
    rate: u32,
    /// Interleaved values gathered so far at `rate`.
    buf: Vec<f32>,
}

impl Chunker {
    /// A chunker for a stream with `width` interleaved values per sample frame
    /// (2 for IQ, 2 for stereo audio). Starts empty, rate unknown (0).
    pub fn new(width: usize) -> Chunker {
        Chunker { width, rate: 0, buf: Vec::new() }
    }

    /// Add a block at `rate`; returns a full frame when one is ready. A change
    /// of rate hands out what was gathered at the old rate first, so no frame
    /// ever mixes two rates.
    ///
    /// In: `block` interleaved values, `rate` in sample frames per second.
    /// Out: `Some(frame)` with the gathered values (ownership moves to the caller),
    /// else `None`. At most one frame per call.
    pub fn push(&mut self, block: &[f32], rate: u32) -> Option<Vec<f32>> {
        let mut out = None;
        // Rate change: flush the old-rate remainder before the new block joins.
        // Invariant: a frame carries one rate; the receiver reads the rate from
        //            elsewhere (state), so a mixed frame would be mis-timed.
        if rate != self.rate {
            if !self.buf.is_empty() {
                out = Some(std::mem::take(&mut self.buf));
            }
            self.rate = rate;
        }
        self.buf.extend_from_slice(block);
        // Only one frame per call: if the flush above already produced one, the
        // new block waits for the next push even when it alone fills a frame.
        if out.is_none() && self.buf.len() >= self.target() {
            out = Some(std::mem::take(&mut self.buf));
        }
        out
    }

    /// Frame size in values (not sample frames): 20 ms at the current rate,
    /// at least 64 sample frames, times `width`.
    fn target(&self) -> usize {
        // Pitfall: at rate 0 (or a very low rate) 20 ms rounds to almost nothing;
        //          the floor of 64 keeps frames from shrinking to single samples.
        ((self.rate as f64 * FRAME_SECONDS) as usize).max(64) * self.width
    }
}

/// Proves: small blocks leave as 20 ms frames; a rate change flushes before mixing.
#[cfg(test)]
mod tests {
    use super::*;

    /// 200 blocks of 126 IQ samples at 250 kS/s become five frames of just over 5,000 samples.
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

    /// A partial frame at 250 k is handed out as soon as a block at 500 k arrives.
    #[test]
    fn a_rate_change_flushes_first() {
        let mut c = Chunker::new(2);
        assert!(c.push(&[1.0; 20], 250_000).is_none());
        let f = c.push(&[2.0; 20], 500_000).unwrap();
        assert_eq!(f, vec![1.0; 20]);
    }
}
