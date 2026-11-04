//! Training runs, from the run cards on this host (docs/runcards.md).
//!
//! A card says where a run's metrics file is, how far the run goes and which
//! process it is. Each update reads the cards, reads what the metrics files
//! gained since last time (a bounded amount: a resident agent that starts
//! beside a long run catches up over a few updates), and works out each
//! run's step, losses, pace and ETA. It also decides the alerts about runs;
//! `Engine` gives them ids and keeps the log.
//!
//! Everything read here belongs to the agent's user: cards and metrics files
//! owned by anyone else are skipped, as are anything but regular files.

use super::procs::ProcInfo;
use crate::model::{AlertKind, QueueInfo, RunInfo, RunStatus, Runs, Workload, SERIES};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Where cards live, under the home directory.
const DIR: &str = ".local/state/runcards";
/// The largest card read.
const CARD_MAX: u64 = 64 * 1024;
/// Bytes read from one metrics file per update.
const READ_CAP: usize = 4 * 1024 * 1024;
/// Cards of runs that ended longer ago than this are left alone.
const FORGET_MS: u64 = 24 * 3600 * 1000;
/// A run is stalled after this long without a step...
const STALL_MIN_S: u32 = 10 * 60;
/// ...and this many times its usual step time.
const STALL_STEPS: f32 = 10.0;
/// The pace is measured over this much of the recent past...
const PACE_WINDOW_MS: u64 = 10 * 60 * 1000;
/// ...once it covers at least this much.
const PACE_MIN_MS: u64 = 60 * 1000;
/// A non-finite loss is news again this many steps after the last one.
const NONFINITE_REARM: u32 = 100;

#[derive(Deserialize, Default)]
#[serde(default)]
struct Card {
    version: u32,
    kind: String,
    name: Option<String>,
    run_dir: Option<String>,
    metrics: Option<String>,
    step_key: Option<String>,
    loss_key: Option<String>,
    eval_key: Option<String>,
    nonfinite_key: Option<String>,
    start_step: Option<u64>,
    max_steps: Option<u64>,
    pid: Option<u32>,
    pid_start: Option<u64>,
    started_ms: Option<u64>,
    updated_ms: Option<u64>,
    status: Option<String>,
    ended_ms: Option<u64>,
    current: Option<String>,
    items: Option<Vec<Item>>,
}

#[derive(Deserialize)]
struct Item {
    #[allow(dead_code)]
    name: String,
    state: String,
}

/// A series kept to at most `SERIES` points: each point is the mean of
/// `stride` consecutive values, and when the points run out, neighbours
/// merge and the stride doubles. A run of any length fits, evenly.
#[derive(Default, Clone, Debug)]
struct Thinned {
    points: Vec<f32>,
    stride: u64,
    sum: f64,
    n: u64,
}

impl Thinned {
    fn push(&mut self, v: f32) {
        if self.stride == 0 {
            self.stride = 1;
        }
        self.sum += v as f64;
        self.n += 1;
        if self.n < self.stride {
            return;
        }
        self.points.push((self.sum / self.n as f64) as f32);
        self.sum = 0.0;
        self.n = 0;
        if self.points.len() >= SERIES {
            self.points = self.points.chunks(2).map(|c| c.iter().sum::<f32>() / c.len() as f32).collect();
            self.stride *= 2;
        }
    }

    /// The points, and the one filling, if any.
    fn values(&self) -> Vec<f32> {
        let mut v = self.points.clone();
        if self.n > 0 {
            v.push((self.sum / self.n as f64) as f32);
        }
        v
    }
}

/// What one metrics file has said so far.
#[derive(Default)]
struct Metrics {
    offset: u64,
    step: Option<u64>,
    loss: Option<f32>,
    eval: Option<f32>,
    eval_step: u64,
    tokens_per_s: Option<f32>,
    /// nerdtrain's `start` event: tokens per optimizer step, for a pace
    /// before there is a measured one.
    tokens_per_step: Option<f64>,
    nonfinite: u32,
    /// The step of the last non-finite loss.
    nonfinite_step: Option<u64>,
    bad_lines: u32,
    series: Thinned,
    evals: VecDeque<f32>,
}

struct Keys<'a> {
    step: &'a str,
    loss: &'a str,
    eval: &'a str,
    nonfinite: &'a str,
}

impl Metrics {
    /// Takes one complete line.
    fn line(&mut self, line: &[u8], keys: &Keys) {
        let text = String::from_utf8_lossy(line);
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&lenient(text)) else {
            self.bad_lines += 1;
            return;
        };
        if obj.get("event").and_then(Value::as_str) == Some("start") {
            self.tokens_per_step = obj.get("tokens_per_step").and_then(Value::as_f64).filter(|v| *v > 0.0);
        }
        let step = obj.get(keys.step).and_then(Value::as_f64).filter(|s| s.is_finite() && *s >= 0.0).map(|s| s as u64);
        if let Some(v) = obj.get(keys.eval).and_then(Value::as_f64).filter(|v| v.is_finite()) {
            self.eval = Some(v as f32);
            self.eval_step = step.or(self.step).unwrap_or(0);
            if self.evals.len() == SERIES {
                self.evals.pop_front();
            }
            self.evals.push_back(v as f32);
        }
        let Some(step) = step else { return };
        // A line with an eval and no training loss is an eval line, not a step.
        let Some(loss) = obj.get(keys.loss) else {
            self.step = Some(self.step.map_or(step, |s| s.max(step)));
            return;
        };
        self.step = Some(step);
        let flagged = match obj.get(keys.nonfinite) {
            Some(Value::Bool(b)) => *b,
            Some(v) => v.as_f64().is_some_and(|v| v != 0.0),
            None => false,
        };
        match loss.as_f64().filter(|v| v.is_finite() && !flagged) {
            Some(v) => {
                self.loss = Some(v as f32);
                self.series.push(v as f32);
            }
            None => {
                self.loss = None;
                self.nonfinite += 1;
                self.nonfinite_step = Some(step);
            }
        }
        if let Some(v) = obj.get("tokens_per_s").and_then(Value::as_f64).filter(|v| v.is_finite() && *v > 0.0) {
            self.tokens_per_s = Some(v as f32);
        }
    }
}

/// `text` with the bare `NaN`, `Infinity` and `-Infinity` Python's `json`
/// writes made `null`, outside strings only, so the line still parses.
fn lenient(text: &str) -> String {
    if !text.contains("NaN") && !text.contains("Infinity") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut in_str = false;
    let mut escaped = false;
    let mut rest = text;
    while let Some(c) = rest.chars().next() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            rest = &rest[c.len_utf8()..];
            continue;
        }
        if c == '"' {
            in_str = true;
        }
        let token = ["-Infinity", "Infinity", "NaN"].into_iter().find(|t| rest.starts_with(t));
        match token {
            Some(t) => {
                out.push_str("null");
                rest = &rest[t.len()..];
            }
            None => {
                out.push(c);
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

/// A regular file owned by `uid`.
fn ours(meta: &std::fs::Metadata, uid: u32) -> bool {
    meta.file_type().is_file() && meta.uid() == uid
}

/// Reads what `path` gained since `m.offset`, at most `READ_CAP` bytes, and
/// takes its complete lines. A file shorter than the offset was replaced or
/// truncated: it is read again from the start. Returns the file's mtime.
fn tail(path: &Path, m: &mut Metrics, keys: &Keys, uid: u32) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !ours(&meta, uid) {
        return None;
    }
    if meta.len() < m.offset {
        *m = Metrics::default();
    }
    let mtime_ms = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64;
    if meta.len() == m.offset {
        return Some(mtime_ms);
    }
    file.seek(SeekFrom::Start(m.offset)).ok()?;
    let mut buf = Vec::new();
    file.take(READ_CAP as u64).read_to_end(&mut buf).ok()?;
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(end) => {
            for line in buf[..end].split(|&b| b == b'\n') {
                m.line(line, keys);
            }
            m.offset += end as u64 + 1;
        }
        // A line longer than the cap is no metrics line: skip it.
        None if buf.len() == READ_CAP => {
            m.bad_lines += 1;
            m.offset += buf.len() as u64;
        }
        // The last line is still being written.
        None => {}
    }
    Some(mtime_ms)
}

/// Whether process `pid` is alive and is the one that started at `start`
/// (clock ticks after boot), when that is known.
fn alive(pid: u32, start: Option<u64>) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    let Some(start) = start else { return true };
    let fields: Vec<&str> = stat.rsplit_once(')').map_or(Vec::new(), |(_, r)| r.split_whitespace().collect());
    fields.get(19).and_then(|v| v.parse::<u64>().ok()) == Some(start)
}

fn status(card: &Card, live: bool) -> RunStatus {
    match card.status.as_deref() {
        Some("finished") => RunStatus::Finished,
        Some("failed") => RunStatus::Failed,
        _ if live => RunStatus::Running,
        _ => RunStatus::Died,
    }
}

/// What the agent keeps about one run between updates.
#[derive(Default)]
struct Run {
    metrics: Metrics,
    metrics_path: PathBuf,
    /// (when, step) at each update that saw a new step.
    pace: VecDeque<(u64, u64)>,
    /// When the last new step was seen; the file's mtime at first.
    last_step_ms: u64,
    /// It was seen running: its end, a stall and a bad loss are news.
    seen_running: bool,
    status: Option<RunStatus>,
    stalled: bool,
    nonfinite_alerted: Option<u64>,
}

#[derive(Default)]
struct Queue {
    seen_running: bool,
    status: Option<RunStatus>,
}

/// An alert about a run, before `Engine` gives it an id.
pub struct Note {
    pub kind: AlertKind,
    pub subject: String,
    pub message: String,
}

#[derive(Default)]
pub struct Tracker {
    runs: HashMap<String, Run>,
    queues: HashMap<String, Queue>,
    /// Overrides `~/.local/state/runcards` (tests).
    pub dir: Option<PathBuf>,
}

fn fmt_loss(v: Option<f32>) -> String {
    v.map_or("-".into(), |v| format!("{v:.3}"))
}

impl Tracker {
    fn cards_dir(&self) -> Option<PathBuf> {
        self.dir.clone().or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(DIR)))
    }

    /// One update: the runs and queues as they stand, and what is news.
    pub fn update(&mut self, now_ms: u64, procs: &[ProcInfo], workloads: &[Workload]) -> (Runs, Vec<Note>) {
        let mut out = Runs::default();
        let mut notes = Vec::new();
        let uid = unsafe { libc::getuid() };
        let Some(dir) = self.cards_dir() else { return (out, notes) };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            self.runs.clear();
            self.queues.clear();
            return (out, notes);
        };
        let parents: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
        let jobs: HashSet<u32> = workloads.iter().map(|w| w.id).collect();
        let mut seen = HashSet::new();
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let Some(key) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if !ours(&meta, uid) || meta.len() > CARD_MAX {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(card) = serde_json::from_str::<Card>(&text) else { continue };
            if card.version != 1 {
                continue;
            }
            let ended = card.ended_ms.unwrap_or(0);
            if ended > 0 && now_ms.saturating_sub(ended) > FORGET_MS {
                continue;
            }
            let live = card.pid.is_some_and(|p| alive(p, card.pid_start));
            let key = key.to_string();
            seen.insert(key.clone());
            match card.kind.as_str() {
                "run" => {
                    if let Some(info) = self.run(&key, &card, live, now_ms, uid, &parents, &jobs, &mut notes) {
                        out.runs.push(info);
                    }
                }
                "queue" => out.queues.push(self.queue(&key, &card, live, now_ms, &mut notes)),
                _ => {}
            }
        }
        self.runs.retain(|k, _| seen.contains(k));
        self.queues.retain(|k, _| seen.contains(k));
        // Running first, then the most recently ended.
        out.runs.sort_by_key(|r| (r.status != RunStatus::Running, std::cmp::Reverse(r.ended_ms.max(r.started_ms))));
        out.queues.sort_by_key(|q| (q.status != RunStatus::Running, std::cmp::Reverse(q.updated_ms)));
        (out, notes)
    }

    #[allow(clippy::too_many_arguments)]
    fn run(
        &mut self,
        key: &str,
        card: &Card,
        live: bool,
        now_ms: u64,
        uid: u32,
        parents: &HashMap<u32, u32>,
        jobs: &HashSet<u32>,
        notes: &mut Vec<Note>,
    ) -> Option<RunInfo> {
        let metrics_path = PathBuf::from(card.metrics.as_deref()?);
        let name = card.name.clone().unwrap_or_else(|| key.to_string());
        let st = status(card, live);
        // A dead run that a fresh agent never saw running stays quiet, and
        // its file is not worth reading again and again: once is enough.
        let r = self.runs.entry(key.to_string()).or_default();
        if r.metrics_path != metrics_path {
            *r = Run { metrics_path: metrics_path.clone(), ..Default::default() };
        }
        let keys = Keys {
            step: card.step_key.as_deref().unwrap_or("step"),
            loss: card.loss_key.as_deref().unwrap_or("loss"),
            eval: card.eval_key.as_deref().unwrap_or("val_loss"),
            nonfinite: card.nonfinite_key.as_deref().unwrap_or("nonfinite"),
        };
        let before = r.metrics.step;
        let nonfinite_before = r.metrics.nonfinite;
        let first = r.last_step_ms == 0;
        if let Some(mtime) = tail(&metrics_path, &mut r.metrics, &keys, uid) {
            if first {
                r.last_step_ms = mtime;
            }
        }
        let m = &r.metrics;
        if m.step != before && !first {
            r.last_step_ms = now_ms;
            r.stalled = false;
        }
        if let Some(step) = m.step {
            if r.pace.back().is_none_or(|&(_, s)| s != step) {
                r.pace.push_back((now_ms, step));
            }
        }
        while r.pace.len() > 2 && r.pace.front().is_some_and(|&(t, _)| now_ms.saturating_sub(t) > PACE_WINDOW_MS) {
            r.pace.pop_front();
        }
        let measured = match (r.pace.front(), r.pace.back()) {
            (Some(&(t0, s0)), Some(&(t1, s1))) if t1.saturating_sub(t0) >= PACE_MIN_MS && s1 > s0 => {
                Some((t1 - t0) as f32 / 1000.0 / (s1 - s0) as f32)
            }
            _ => None,
        };
        let hinted = match (m.tokens_per_step, m.tokens_per_s) {
            (Some(tps), Some(rate)) => Some((tps / rate as f64) as f32),
            _ => None,
        };
        let step_s = measured.or(hinted).filter(|s| s.is_finite() && *s > 0.0);
        let start = card.start_step.unwrap_or(0);
        let max = card.max_steps.unwrap_or(0);
        let step = m.step.unwrap_or(start);
        let running = st == RunStatus::Running;
        let quiet_s = if running { (now_ms.saturating_sub(r.last_step_ms) / 1000) as u32 } else { 0 };
        let eta_s = match (running, step_s) {
            (true, Some(s)) if max > step => Some(((max - step) as f32 * s).min(u32::MAX as f32) as u32),
            _ => None,
        };

        // News, about runs this agent saw running.
        let subject = name.clone();
        if running {
            r.seen_running = true;
        }
        if r.seen_running && r.status != Some(st) && !running {
            let at = if max > 0 { format!("step {step} of {max}") } else { format!("step {step}") };
            let (kind, message) = match st {
                RunStatus::Finished => (
                    AlertKind::RunEnded,
                    format!("{name} finished at {at}: loss {}, eval {}", fmt_loss(m.loss), fmt_loss(m.eval)),
                ),
                RunStatus::Failed => (AlertKind::RunEnded, format!("{name} failed at {at}")),
                _ => (AlertKind::RunDied, format!("{name} died at {at}, without saying why")),
            };
            notes.push(Note { kind, subject: subject.clone(), message });
        }
        r.status = Some(st);
        if r.seen_running && m.nonfinite > nonfinite_before && !first {
            let at = m.nonfinite_step.unwrap_or(step);
            if r.nonfinite_alerted.is_none_or(|s| at >= s + NONFINITE_REARM as u64) {
                r.nonfinite_alerted = Some(at);
                notes.push(Note {
                    kind: AlertKind::RunNonfinite,
                    subject: subject.clone(),
                    message: format!("{name}: non-finite loss at step {at}"),
                });
            }
        }
        let stall_s = step_s.map_or(STALL_MIN_S, |s| (s * STALL_STEPS) as u32).max(STALL_MIN_S);
        if running && !r.stalled && quiet_s >= stall_s && r.seen_running && !first {
            r.stalled = true;
            notes.push(Note {
                kind: AlertKind::RunStalled,
                subject,
                message: format!("{name}: no new step for {} min, at step {step}", quiet_s / 60),
            });
        }

        // The workload it belongs to: its process, or the nearest ancestor that is one.
        let mut workload = 0;
        let mut pid = card.pid.unwrap_or(0);
        for _ in 0..64 {
            if pid <= 1 {
                break;
            }
            if jobs.contains(&pid) {
                workload = pid;
                break;
            }
            pid = parents.get(&pid).copied().unwrap_or(0);
        }
        let clamp = |v: u64| v.min(u32::MAX as u64) as u32;
        Some(RunInfo {
            key: key.to_string(),
            name,
            run_dir: card.run_dir.clone().unwrap_or_default(),
            status: st,
            workload: if running { workload } else { 0 },
            start_step: clamp(start),
            max_steps: clamp(max),
            step: clamp(step),
            loss: m.loss,
            eval: m.eval,
            eval_step: clamp(m.eval_step),
            tokens_per_s: m.tokens_per_s,
            step_s,
            eta_s,
            quiet_s,
            nonfinite: m.nonfinite,
            bad_lines: m.bad_lines,
            series: m.series.values(),
            evals: m.evals.iter().copied().collect(),
            started_ms: card.started_ms.unwrap_or(0),
            ended_ms: card.ended_ms.unwrap_or(0),
        })
    }

    fn queue(&mut self, key: &str, card: &Card, live: bool, now_ms: u64, notes: &mut Vec<Note>) -> QueueInfo {
        let st = status(card, live);
        let items = card.items.as_deref().unwrap_or(&[]);
        let count = |s: &str| items.iter().filter(|i| i.state == s).count().min(u16::MAX as usize) as u16;
        let name = card.name.clone().unwrap_or_else(|| key.to_string());
        let (done, failed, total) = (count("done"), count("failed"), items.len().min(u16::MAX as usize) as u16);
        let q = self.queues.entry(key.to_string()).or_default();
        if st == RunStatus::Running {
            q.seen_running = true;
        }
        if q.seen_running && q.status != Some(st) && st != RunStatus::Running {
            let message = match st {
                RunStatus::Finished => format!("queue {name} finished: {done} of {total} done"),
                RunStatus::Failed => format!("queue {name} failed: {done} of {total} done, {failed} failed"),
                _ => format!("queue {name} died: {done} of {total} done"),
            };
            let kind = if st == RunStatus::Died { AlertKind::RunDied } else { AlertKind::RunEnded };
            notes.push(Note { kind, subject: name.clone(), message });
        }
        q.status = Some(st);
        QueueInfo {
            key: key.to_string(),
            name,
            status: st,
            done,
            failed,
            total,
            current: if st == RunStatus::Running { card.current.clone().unwrap_or_default() } else { String::new() },
            started_ms: card.started_ms.unwrap_or(0),
            updated_ms: card.updated_ms.unwrap_or(now_ms),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn keys() -> Keys<'static> {
        Keys { step: "step", loss: "loss", eval: "val_loss", nonfinite: "nonfinite" }
    }

    #[test]
    fn python_nan_is_a_non_finite_loss_not_a_broken_line() {
        assert_eq!(
            lenient(r#"{"loss":NaN,"x":-Infinity,"s":"NaN Infinity"}"#),
            r#"{"loss":null,"x":null,"s":"NaN Infinity"}"#
        );
        assert_eq!(lenient(r#"{"s":"a\"NaN"}"#), r#"{"s":"a\"NaN"}"#);
        let mut m = Metrics::default();
        m.line(br#"{"step":5,"loss":NaN,"nonfinite":1}"#, &keys());
        m.line(br#"{"step":6,"loss":2.5,"nonfinite":0}"#, &keys());
        assert_eq!((m.nonfinite, m.nonfinite_step, m.bad_lines, m.loss, m.step), (1, Some(5), 0, Some(2.5), Some(6)));
        m.line(b"{not json", &keys());
        assert_eq!(m.bad_lines, 1);
    }

    #[test]
    fn events_evals_and_steps_are_told_apart() {
        let mut m = Metrics::default();
        m.line(br#"{"event":"start","tokens_per_step":524288,"max_steps":2400}"#, &keys());
        m.line(br#"{"step":2001,"loss":3.0,"tokens_per_s":86000.0,"nonfinite":0}"#, &keys());
        m.line(br#"{"event":"eval","step":2001,"val_loss":2.9}"#, &keys());
        assert_eq!((m.step, m.loss, m.eval, m.eval_step, m.bad_lines), (Some(2001), Some(3.0), Some(2.9), 2001, 0));
        assert_eq!(m.tokens_per_step, Some(524288.0));
        assert_eq!(m.series.values(), vec![3.0]);
    }

    #[test]
    fn a_thinned_series_stays_bounded_and_even() {
        let mut t = Thinned::default();
        for i in 0..10_000 {
            t.push(i as f32);
        }
        let v = t.values();
        assert!(v.len() <= SERIES + 1 && v.len() > SERIES / 2);
        assert!(v.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn tailing_waits_for_whole_lines_and_restarts_on_truncation() {
        let dir = std::env::temp_dir().join(format!("nerdfleet-runs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("train.jsonl");
        let mut f = File::create(&path).unwrap();
        write!(f, "{{\"step\":1,\"loss\":3.0}}\n{{\"step\":2,\"lo").unwrap();
        let uid = unsafe { libc::getuid() };
        let mut m = Metrics::default();
        tail(&path, &mut m, &keys(), uid).unwrap();
        assert_eq!((m.step, m.bad_lines), (Some(1), 0));
        writeln!(f, "ss\":2.5}}").unwrap();
        tail(&path, &mut m, &keys(), uid).unwrap();
        assert_eq!((m.step, m.loss), (Some(2), Some(2.5)));
        std::fs::write(&path, "{\"step\":1,\"loss\":9.0}\n").unwrap();
        tail(&path, &mut m, &keys(), uid).unwrap();
        assert_eq!((m.step, m.loss, m.series.values().len()), (Some(1), Some(9.0), 1));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn card(dir: &Path, key: &str, body: &str) {
        std::fs::write(dir.join(format!("{key}.json")), body).unwrap();
    }

    #[test]
    fn runs_report_progress_and_raise_news_only_about_runs_seen_running() {
        let dir = std::env::temp_dir().join(format!("nerdfleet-cards-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let metrics = dir.join("train.jsonl");
        std::fs::write(&metrics, "{\"step\":2001,\"loss\":3.0}\n{\"step\":2002,\"loss\":2.9}\n").unwrap();
        let me = std::process::id();
        let start = std::fs::read_to_string(format!("/proc/{me}/stat")).unwrap();
        let start: u64 = start.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().parse().unwrap();
        let running = format!(
            r#"{{"version":1,"kind":"run","name":"exp/a","run_dir":"/r","metrics":"{}","start_step":2000,
               "max_steps":2400,"pid":{me},"pid_start":{start},"status":"running"}}"#,
            metrics.display()
        );
        card(&dir, "a", &running);
        // A card of a run that ended long ago, and one that died before this agent started.
        card(&dir, "old", r#"{"version":1,"kind":"run","metrics":"/x","status":"finished","ended_ms":1}"#);
        card(&dir, "dead", &running.replace(&format!("\"pid\":{me}"), "\"pid\":999999999"));
        let mut t = Tracker { dir: Some(dir.clone()), ..Default::default() };
        let now = 10_000_000_000_000;
        let (runs, notes) = t.update(now, &[], &[]);
        assert!(notes.is_empty(), "nothing is news at first sight");
        let a = runs.runs.iter().find(|r| r.key == "a").unwrap();
        assert_eq!((a.status, a.step, a.max_steps, a.loss), (RunStatus::Running, 2002, 2400, Some(2.9)));
        let dead = runs.runs.iter().find(|r| r.key == "dead").unwrap();
        assert_eq!(dead.status, RunStatus::Died);
        assert!(runs.runs.iter().all(|r| r.key != "old"));
        assert_eq!(runs.runs[0].status, RunStatus::Running);

        // Steps arrive, then a NaN, then the run ends.
        let mut f = std::fs::OpenOptions::new().append(true).open(&metrics).unwrap();
        writeln!(f, "{{\"step\":2100,\"loss\":2.5}}").unwrap();
        let (runs, _) = t.update(now + 120_000, &[], &[]);
        let a = runs.runs.iter().find(|r| r.key == "a").unwrap();
        assert!(a.step_s.is_some_and(|s| (s - 120.0 / 98.0).abs() < 1e-3), "{:?}", a.step_s);
        assert!(a.eta_s.is_some_and(|e| e > 0));
        writeln!(f, "{{\"step\":2101,\"loss\":NaN,\"nonfinite\":1}}").unwrap();
        let (_, notes) = t.update(now + 130_000, &[], &[]);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].kind, AlertKind::RunNonfinite);
        let finished = format!("\"status\":\"finished\",\"ended_ms\":{}", now + 139_000);
        card(&dir, "a", &running.replace("\"status\":\"running\"", &finished));
        let (_, notes) = t.update(now + 140_000, &[], &[]);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].kind, AlertKind::RunEnded);
        assert!(notes[0].message.contains("finished at step 2101 of 2400"), "{}", notes[0].message);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
