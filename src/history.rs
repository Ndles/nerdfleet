//! Graph history at three resolutions, kept the same way by agents (which
//! remember while nobody watches) and by the viewer (which carries it on
//! from what an agent sent when it connected).
//!
//! Each ring holds one value per step for each series: CPU, memory, then
//! each GPU's utilization, all 0 to 100. A longer step keeps the highest
//! value seen in it, so short spikes survive. The finest ring takes one
//! point per sample; the others are cut at fixed multiples of their step,
//! so two machines with the same samples agree, and a gap (the agent was
//! not running) shows as `NONE` rather than being bridged.

use crate::model::Sample;
use crate::proto::{bad, Wire};
use std::collections::VecDeque;
use std::io;

/// A point with no data: the agent was not running then.
pub const NONE: u8 = u8::MAX;

/// Step and length of each ring: 15 minutes by the second, 6 hours by ten
/// seconds, a day by the minute.
pub const RINGS: [(u32, usize); 3] = [(1000, 900), (10_000, 2160), (60_000, 1440)];

/// The most series a history can have: CPU, memory and this many GPUs.
const MAX_SERIES: usize = 2 + 64;

#[derive(Debug, Clone, PartialEq)]
pub struct Ring {
    pub step_ms: u32,
    pub cap: usize,
    /// The absolute number of the newest point: a sample count for the
    /// finest ring, `time / step` for the others. Graphs cut their cells
    /// at fixed multiples of it, so they scroll without jumping.
    pub end: u64,
    pub series: Vec<VecDeque<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct History {
    pub rings: Vec<Ring>,
}

impl Default for History {
    fn default() -> History {
        History {
            rings: RINGS
                .iter()
                .map(|&(step_ms, cap)| Ring { step_ms, cap, end: 0, series: Vec::new() })
                .collect(),
        }
    }
}

/// A sample's values for the history, in series order.
pub fn values(s: &Sample) -> Vec<u8> {
    let pct = |v: f64| v.clamp(0.0, 100.0).round() as u8;
    let mem = s.mem.total.saturating_sub(s.mem.available) as f64 * 100.0 / s.mem.total.max(1) as f64;
    let mut v = vec![pct(s.cpu.busy as f64 * 100.0), pct(mem)];
    v.extend(s.gpus.iter().map(|g| g.util.map_or(NONE, |u| u.min(100))));
    v
}

impl Ring {
    fn len(&self) -> usize {
        self.series.first().map_or(0, VecDeque::len)
    }

    /// Makes room for `n` series: new ones are empty back to the start.
    fn resize(&mut self, n: usize) {
        let len = self.len();
        self.series.resize_with(n, || std::iter::repeat_n(NONE, len).collect());
    }

    fn append(&mut self, values: &[u8]) {
        for (s, &v) in self.series.iter_mut().zip(values) {
            if s.len() == self.cap {
                s.pop_front();
            }
            s.push_back(v);
        }
    }

    /// The newest point takes the higher of what it has and `values`.
    fn merge(&mut self, values: &[u8]) {
        for (s, &v) in self.series.iter_mut().zip(values) {
            if let Some(last) = s.back_mut() {
                *last = match (*last, v) {
                    (NONE, v) | (v, NONE) => v,
                    (a, b) => a.max(b),
                };
            }
        }
    }

    fn push(&mut self, index: Option<u64>, values: &[u8]) {
        if self.series.len() != values.len() {
            self.resize(values.len());
        }
        let Some(index) = index else {
            // The finest ring: one point per sample.
            self.append(values);
            self.end += 1;
            return;
        };
        let empty = self.len() == 0;
        if !empty && index == self.end {
            self.merge(values);
            return;
        }
        if !empty && index > self.end {
            // Steps with no samples: the agent was not running.
            let gap = (index - self.end - 1).min(self.cap as u64) as usize;
            let none = vec![NONE; values.len()];
            for _ in 0..gap {
                self.append(&none);
            }
        } else if !empty && index < self.end {
            // The clock went back; carry on in the newest step.
            self.merge(values);
            return;
        }
        self.append(values);
        self.end = index;
    }
}

impl History {
    pub fn push(&mut self, time_ms: u64, values: &[u8]) {
        let values = &values[..values.len().min(MAX_SERIES)];
        for (i, ring) in self.rings.iter_mut().enumerate() {
            let index = (i > 0).then(|| time_ms / ring.step_ms.max(1) as u64);
            ring.push(index, values);
        }
    }

    /// The finest ring's series `i`, newest last, for small sparklines.
    pub fn recent(&self, i: usize) -> Option<&VecDeque<u8>> {
        self.rings.first()?.series.get(i)
    }

    /// Whether this history is one this program could have made: the rings
    /// it knows, no ring longer than its length, series of equal length.
    /// Histories come from other machines (and from files); this is what
    /// lets the rest of the code index them without checking.
    pub fn is_sane(&self) -> bool {
        self.rings.len() == RINGS.len()
            && self.rings.iter().zip(RINGS).all(|(r, (step, cap))| {
                r.step_ms == step
                    && r.cap == cap
                    && r.series.len() <= MAX_SERIES
                    && r.series.iter().all(|s| s.len() <= cap && s.len() == r.len())
            })
    }
}

// ===== wire =================================================================

impl Wire for Points {
    fn put(&self, out: &mut Vec<u8>) {
        (self.0.len() as u32).put(out);
        out.extend(self.0.iter());
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        let n = u32::get(input)? as usize;
        if n > 4096 || n > input.len() {
            return Err(bad("series too long"));
        }
        let (head, rest) = input.split_at(n);
        *input = rest;
        Ok(Points(head.iter().copied().collect()))
    }
}

/// A series on the wire: its length, then a byte per point.
struct Points(VecDeque<u8>);

impl Wire for Ring {
    fn put(&self, out: &mut Vec<u8>) {
        self.step_ms.put(out);
        (self.cap as u32).put(out);
        self.end.put(out);
        (self.series.len() as u32).put(out);
        for s in &self.series {
            Points(s.clone()).put(out);
        }
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        let step_ms = u32::get(input)?;
        let cap = u32::get(input)? as usize;
        let end = u64::get(input)?;
        let n = u32::get(input)? as usize;
        if n > MAX_SERIES {
            return Err(bad("too many series"));
        }
        let series = (0..n).map(|_| Points::get(input).map(|s| s.0)).collect::<io::Result<_>>()?;
        Ok(Ring { step_ms, cap, end, series })
    }
}

impl Wire for History {
    fn put(&self, out: &mut Vec<u8>) {
        self.rings.put(out);
    }
    fn get(input: &mut &[u8]) -> io::Result<Self> {
        let h = History { rings: Wire::get(input)? };
        if !h.is_sane() {
            return Err(bad("history does not fit its rings"));
        }
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{read_frame, write_frame};

    #[test]
    fn coarse_rings_keep_peaks_and_mark_gaps() {
        let mut h = History::default();
        for t in 0..25 {
            h.push(t * 1000, &[if t == 3 { 90 } else { 10 }, 50]);
        }
        // A minute and a half with the agent off, then one more sample.
        h.push(115_000, &[20, 50]);
        let ten = &h.rings[1];
        assert_eq!(ten.series[0].iter().copied().collect::<Vec<_>>(), [90, 10, 10, NONE, NONE, NONE, NONE, NONE, NONE, NONE, NONE, 20]);
        assert_eq!(ten.end, 11);
        assert_eq!(h.rings[0].series[0].len(), 26, "the finest ring takes every sample");
        assert_eq!(h.rings[0].end, 26);
    }

    #[test]
    fn rings_keep_their_length() {
        let mut h = History::default();
        for t in 0..2000u64 {
            h.push(t * 1000, &[1, 2]);
        }
        assert_eq!(h.rings[0].series[0].len(), 900);
        assert!(h.is_sane());
    }

    #[test]
    fn a_gpu_appearing_later_starts_empty() {
        let mut h = History::default();
        h.push(0, &[1, 2]);
        h.push(1000, &[1, 2, 77]);
        assert_eq!(h.rings[0].series[2].iter().copied().collect::<Vec<_>>(), [NONE, 77]);
        assert!(h.is_sane());
    }

    #[test]
    fn history_survives_a_roundtrip_and_bad_ones_are_refused() {
        let mut h = History::default();
        for t in 0..100u64 {
            h.push(t * 1000, &[(t % 100) as u8, 3, 4]);
        }
        let mut buf = Vec::new();
        write_frame(&mut buf, &h).unwrap();
        assert_eq!(read_frame::<_, History>(&mut buf.as_slice()).unwrap().unwrap(), h);
        let mut bad = h.clone();
        bad.rings[0].series[0].push_back(1);
        let mut buf = Vec::new();
        write_frame(&mut buf, &bad).unwrap();
        assert!(read_frame::<_, History>(&mut buf.as_slice()).is_err());
    }
}
