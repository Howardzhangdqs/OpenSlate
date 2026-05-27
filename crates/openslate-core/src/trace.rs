//! Trace event collection for Chrome Trace JSON (Perfetto) export.
//!
//! Provides a [`TraceCollector`] that records span begin/end, instant, and counter
//! events and serializes them to the Chrome Trace JSON format understood by
//! `chrome://tracing` and [Perfetto UI](https://ui.perfetto.dev).

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// TraceSpanId
// ---------------------------------------------------------------------------

/// Opaque identifier tying a `DurationBegin` event to its `DurationEnd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceSpanId {
    name: String,
    cat: String,
    id: u64,
}

// ---------------------------------------------------------------------------
// TraceEvent
// ---------------------------------------------------------------------------

/// A single trace event in Chrome Trace JSON format.
///
/// See <https://docs.google.com/document/d/1CvAClvFfyA5R-PhYUmn5OOQtYMH4h6I0nSsKcasNAojs>
#[derive(Debug, Clone, PartialEq)]
pub enum TraceEvent {
    /// Begin a duration span (phase **B**).
    DurationBegin {
        name: String,
        cat: String,
        pid: i32,
        tid: i32,
        ts: u64,
        args: HashMap<String, serde_json::Value>,
    },
    /// End a duration span (phase **E**).
    DurationEnd {
        name: String,
        cat: String,
        pid: i32,
        tid: i32,
        ts: u64,
    },
    /// A complete span with known duration (phase **X**).
    Complete {
        name: String,
        cat: String,
        pid: i32,
        tid: i32,
        ts: u64,
        dur: u64,
        args: HashMap<String, serde_json::Value>,
    },
    /// An instant event (phase **i**).
    Instant {
        name: String,
        cat: String,
        pid: i32,
        tid: i32,
        ts: u64,
        s: &'static str,
    },
    /// A counter event (phase **C**).
    Counter {
        name: String,
        cat: String,
        pid: i32,
        tid: i32,
        ts: u64,
        values: HashMap<String, f64>,
    },
}

impl TraceEvent {
    /// Phase code used in the Chrome Trace JSON `ph` field.
    pub fn phase(&self) -> &'static str {
        match self {
            Self::DurationBegin { .. } => "B",
            Self::DurationEnd { .. } => "E",
            Self::Complete { .. } => "X",
            Self::Instant { .. } => "i",
            Self::Counter { .. } => "C",
        }
    }

    fn ts(&self) -> u64 {
        match self {
            Self::DurationBegin { ts, .. }
            | Self::DurationEnd { ts, .. }
            | Self::Complete { ts, .. }
            | Self::Instant { ts, .. }
            | Self::Counter { ts, .. } => *ts,
        }
    }

    fn pid(&self) -> i32 {
        match self {
            Self::DurationBegin { pid, .. }
            | Self::DurationEnd { pid, .. }
            | Self::Complete { pid, .. }
            | Self::Instant { pid, .. }
            | Self::Counter { pid, .. } => *pid,
        }
    }

    fn tid(&self) -> i32 {
        match self {
            Self::DurationBegin { tid, .. }
            | Self::DurationEnd { tid, .. }
            | Self::Complete { tid, .. }
            | Self::Instant { tid, .. }
            | Self::Counter { tid, .. } => *tid,
        }
    }

    fn name(&self) -> &str {
        match self {
            Self::DurationBegin { name, .. }
            | Self::DurationEnd { name, .. }
            | Self::Complete { name, .. }
            | Self::Instant { name, .. }
            | Self::Counter { name, .. } => name,
        }
    }

    fn cat(&self) -> &str {
        match self {
            Self::DurationBegin { cat, .. }
            | Self::DurationEnd { cat, .. }
            | Self::Complete { cat, .. }
            | Self::Instant { cat, .. }
            | Self::Counter { cat, .. } => cat,
        }
    }
}

// ---------------------------------------------------------------------------
// TraceCollector
// ---------------------------------------------------------------------------

static SPAN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Collects trace events and serializes to Chrome Trace JSON.
///
/// # Example
///
/// ```
/// use openslate_core::trace::TraceCollector;
///
/// let mut tc = TraceCollector::new(1, 1);
/// let span = tc.begin_span("run_agent", "runtime");
/// // ... do work ...
/// tc.end_span(span);
/// let json = tc.to_chrome_trace_json();
/// assert!(json.contains("\"ph\":\"B\""));
/// ```
#[derive(Debug)]
pub struct TraceCollector {
    events: Vec<TraceEvent>,
    process_id: i32,
    thread_id: i32,
    #[allow(dead_code)]
    epoch: Instant,
}

impl TraceCollector {
    /// Create a new collector for the given process/thread.
    pub fn new(process_id: i32, thread_id: i32) -> Self {
        Self {
            events: Vec::new(),
            process_id,
            thread_id,
            epoch: Instant::now(),
        }
    }

    /// Return the current timestamp in microseconds since Unix epoch.
    fn now_us(&self) -> u64 {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        since_epoch.as_micros() as u64
    }

    /// Begin a duration span (phase B). Returns a [`TraceSpanId`] for later
    /// pairing with [`end_span`](Self::end_span).
    pub fn begin_span(&mut self, name: &str, category: &str) -> TraceSpanId {
        self.begin_span_with_args(name, category, HashMap::new())
    }

    /// Begin a duration span with attached args (phase B).
    pub fn begin_span_with_args(
        &mut self,
        name: &str,
        category: &str,
        args: HashMap<String, serde_json::Value>,
    ) -> TraceSpanId {
        let id = SPAN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let span_id = TraceSpanId {
            name: name.to_owned(),
            cat: category.to_owned(),
            id,
        };
        self.events.push(TraceEvent::DurationBegin {
            name: name.to_owned(),
            cat: category.to_owned(),
            pid: self.process_id,
            tid: self.thread_id,
            ts: self.now_us(),
            args,
        });
        span_id
    }

    /// End the duration span started by [`begin_span`](Self::begin_span) (phase E).
    pub fn end_span(&mut self, span_id: TraceSpanId) {
        self.events.push(TraceEvent::DurationEnd {
            name: span_id.name,
            cat: span_id.cat,
            pid: self.process_id,
            tid: self.thread_id,
            ts: self.now_us(),
        });
    }

    /// Record a complete span with known duration (phase X).
    pub fn record_complete(
        &mut self,
        name: &str,
        category: &str,
        dur_us: u64,
        args: HashMap<String, serde_json::Value>,
    ) {
        let ts = self.now_us();
        self.events.push(TraceEvent::Complete {
            name: name.to_owned(),
            cat: category.to_owned(),
            pid: self.process_id,
            tid: self.thread_id,
            ts: ts.saturating_sub(dur_us),
            dur: dur_us,
            args,
        });
    }

    /// Record an instant event (phase i).
    pub fn record_instant(&mut self, name: &str, category: &str) {
        self.events.push(TraceEvent::Instant {
            name: name.to_owned(),
            cat: category.to_owned(),
            pid: self.process_id,
            tid: self.thread_id,
            ts: self.now_us(),
            s: "t",
        });
    }

    /// Record a counter event (phase C) with named values.
    pub fn record_counter(&mut self, name: &str, values: HashMap<String, f64>) {
        self.events.push(TraceEvent::Counter {
            name: name.to_owned(),
            cat: "counter".to_owned(),
            pid: self.process_id,
            tid: self.thread_id,
            ts: self.now_us(),
            values,
        });
    }

    /// Serialize all collected events to Chrome Trace JSON.
    ///
    /// Output format: `[{"ph":"B",...}, {"ph":"E",...}]`
    pub fn to_chrome_trace_json(&self) -> String {
        let json_events: Vec<serde_json::Value> = self
            .events
            .iter()
            .map(event_to_json)
            .collect();
        serde_json::to_string(&json_events).unwrap_or_else(|_| "[]".to_owned())
    }

    /// Return a reference to the collected events.
    pub fn events(&self) -> &[TraceEvent] {
        &self.events
    }

    /// Remove all collected events.
    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Export the Chrome Trace JSON to a file, creating parent directories if needed.
    pub fn export_to_file(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let json = self.to_chrome_trace_json();
        std::fs::write(path, json)
    }
}

// ---------------------------------------------------------------------------
// JSON serialization helpers
// ---------------------------------------------------------------------------

fn event_to_json(ev: &TraceEvent) -> serde_json::Value {
    use serde_json::Value;

    let mut obj = serde_json::Map::with_capacity(8);
    obj.insert("ph".into(), Value::String(ev.phase().to_owned()));
    obj.insert("name".into(), Value::String(ev.name().to_owned()));
    obj.insert("cat".into(), Value::String(ev.cat().to_owned()));
    obj.insert("pid".into(), Value::Number(ev.pid().into()));
    obj.insert("tid".into(), Value::Number(ev.tid().into()));
    obj.insert("ts".into(), Value::Number(ev.ts().into()));

    match ev {
        TraceEvent::DurationBegin { args, .. } => {
            obj.insert("args".into(), args_to_value(args));
        }
        TraceEvent::Complete { dur, args, .. } => {
            obj.insert("dur".into(), Value::Number((*dur).into()));
            obj.insert("args".into(), args_to_value(args));
        }
        TraceEvent::Instant { s, .. } => {
            obj.insert("s".into(), Value::String(s.to_string()));
        }
        TraceEvent::Counter { values, .. } => {
            obj.insert("args".into(), counter_values_to_value(values));
        }
        TraceEvent::DurationEnd { .. } => {}
    }

    Value::Object(obj)
}

fn args_to_value(args: &HashMap<String, serde_json::Value>) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> =
        args.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    serde_json::Value::Object(map)
}

fn counter_values_to_value(values: &HashMap<String, f64>) -> serde_json::Value {
    let mut map = serde_json::Map::with_capacity(values.len());
    for (k, v) in values {
        map.insert(
            k.clone(),
            serde_json::Value::Number(
                serde_json::Number::from_f64(*v).unwrap_or_else(|| serde_json::Number::from(0)),
            ),
        );
    }
    serde_json::Value::Object(map)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_collector() -> TraceCollector {
        TraceCollector::new(1, 1)
    }

    #[test]
    fn begin_end_span_produces_b_and_e() {
        let mut tc = make_collector();
        let span = tc.begin_span("run_agent", "runtime");
        tc.end_span(span);
        let events = tc.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].phase(), "B");
        assert_eq!(events[1].phase(), "E");
    }

    #[test]
    fn begin_span_copies_name_and_cat() {
        let mut tc = make_collector();
        let span = tc.begin_span("llm_call", "model");
        tc.end_span(span);
        let events = tc.events();
        assert_eq!(events[0].name(), "llm_call");
        assert_eq!(events[0].cat(), "model");
        assert_eq!(events[1].name(), "llm_call");
        assert_eq!(events[1].cat(), "model");
    }

    #[test]
    fn record_instant_produces_i_event() {
        let mut tc = make_collector();
        tc.record_instant("checkpoint", "debug");
        assert_eq!(tc.events().len(), 1);
        assert_eq!(tc.events()[0].phase(), "i");
        assert_eq!(tc.events()[0].name(), "checkpoint");
    }

    #[test]
    fn record_counter_produces_c_event_with_values() {
        let mut tc = make_collector();
        let mut values = HashMap::new();
        values.insert("memory_mb".to_owned(), 256.0);
        values.insert("cpu_pct".to_owned(), 42.5);
        tc.record_counter("resources", values);
        assert_eq!(tc.events().len(), 1);
        let event = &tc.events()[0];
        assert_eq!(event.phase(), "C");
        if let TraceEvent::Counter { values, .. } = event {
            assert_eq!(values.len(), 2);
            assert_eq!(values.get("memory_mb"), Some(&256.0));
            assert_eq!(values.get("cpu_pct"), Some(&42.5));
        } else {
            panic!("expected Counter event");
        }
    }

    #[test]
    fn to_chrome_trace_json_produces_valid_json_array() {
        let mut tc = make_collector();
        let span = tc.begin_span("run_agent", "runtime");
        tc.end_span(span);
        let json = tc.to_chrome_trace_json();
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(parsed.is_array());
        let arr = parsed.as_array().expect("array");
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn json_events_have_correct_phase_codes() {
        let mut tc = make_collector();
        let span = tc.begin_span("s1", "c1");
        tc.end_span(span);
        tc.record_instant("snap", "debug");
        let mut v = HashMap::new();
        v.insert("x".to_owned(), 1.0);
        tc.record_counter("cnt", v);

        let json = tc.to_chrome_trace_json();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0]["ph"], "B");
        assert_eq!(parsed[1]["ph"], "E");
        assert_eq!(parsed[2]["ph"], "i");
        assert_eq!(parsed[3]["ph"], "C");
    }

    #[test]
    fn multiple_spans_tracked_correctly() {
        let mut tc = make_collector();
        let s1 = tc.begin_span("span_a", "cat");
        let s2 = tc.begin_span("span_b", "cat");
        tc.end_span(s2);
        tc.end_span(s1);
        let events = tc.events();
        assert_eq!(events.len(), 4);
        // B A, B B, E B, E A
        assert_eq!(events[0].name(), "span_a");
        assert_eq!(events[0].phase(), "B");
        assert_eq!(events[1].name(), "span_b");
        assert_eq!(events[1].phase(), "B");
        assert_eq!(events[2].name(), "span_b");
        assert_eq!(events[2].phase(), "E");
        assert_eq!(events[3].name(), "span_a");
        assert_eq!(events[3].phase(), "E");
    }

    #[test]
    fn clear_removes_all_events() {
        let mut tc = make_collector();
        let span = tc.begin_span("x", "y");
        tc.end_span(span);
        assert_eq!(tc.events().len(), 2);
        tc.clear();
        assert!(tc.events().is_empty());
        // JSON should be empty array
        assert_eq!(tc.to_chrome_trace_json(), "[]");
    }

    #[test]
    fn nested_spans_work_correctly() {
        let mut tc = make_collector();
        let outer = tc.begin_span("outer", "cat");
        let inner = tc.begin_span("inner", "cat");
        tc.end_span(inner);
        tc.end_span(outer);
        let events = tc.events();
        // B outer, B inner, E inner, E outer
        assert_eq!(events[0].name(), "outer");
        assert_eq!(events[1].name(), "inner");
        assert_eq!(events[2].name(), "inner");
        assert_eq!(events[3].name(), "outer");
        // Timestamps should be non-decreasing
        let ts: Vec<u64> = events.iter().map(|e| e.ts()).collect();
        assert!(ts.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn pid_and_tid_set_correctly() {
        let mut tc = TraceCollector::new(42, 7);
        tc.record_instant("ev", "cat");
        let ev = &tc.events()[0];
        assert_eq!(ev.pid(), 42);
        assert_eq!(ev.tid(), 7);
    }

    #[test]
    fn begin_span_with_args_preserved_in_json() {
        let mut tc = make_collector();
        let mut args = HashMap::new();
        args.insert("key".to_owned(), serde_json::Value::String("value".to_owned()));
        let span = tc.begin_span_with_args("task", "work", args);
        tc.end_span(span);

        let json = tc.to_chrome_trace_json();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
        let begin = &parsed[0];
        assert_eq!(begin["ph"], "B");
        assert_eq!(begin["args"]["key"], "value");
    }

    #[test]
    fn record_complete_span() {
        let mut tc = make_collector();
        tc.record_complete("init", "startup", 500, HashMap::new());
        assert_eq!(tc.events().len(), 1);
        assert_eq!(tc.events()[0].phase(), "X");
        if let TraceEvent::Complete { dur, .. } = &tc.events()[0] {
            assert_eq!(*dur, 500);
        }
    }

    #[test]
    fn empty_collector_produces_empty_array() {
        let tc = make_collector();
        assert_eq!(tc.to_chrome_trace_json(), "[]");
    }

    #[test]
    fn export_to_file_creates_valid_json_file() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("trace.json");

        let mut tc = make_collector();
        let span = tc.begin_span("agent_exec", "runtime");
        tc.end_span(span);
        tc.record_instant("checkpoint", "debug");

        tc.export_to_file(&path).expect("export should succeed");

        let content = std::fs::read_to_string(&path).expect("read file");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert!(parsed.is_array());
        let arr = parsed.as_array().expect("array");
        assert_eq!(arr.len(), 3); // B, E, i
    }

    #[test]
    fn export_to_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("nested").join("dir").join("trace.json");

        let tc = make_collector();
        tc.export_to_file(&path).expect("export should succeed");

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).expect("read file");
        assert_eq!(content, "[]");
    }

    #[test]
    fn export_to_file_chrome_trace_parseable() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join("trace.json");

        let mut tc = make_collector();
        let s1 = tc.begin_span("agent", "runtime");
        let s2 = tc.begin_span("model_call", "model");
        tc.end_span(s2);
        tc.end_span(s1);
        tc.record_instant("done", "runtime");

        tc.export_to_file(&path).expect("export should succeed");

        let content = std::fs::read_to_string(&path).expect("read file");
        let events: Vec<serde_json::Value> = serde_json::from_str(&content).expect("parse events");

        assert_eq!(events.len(), 5);
        assert_eq!(events[0]["ph"], "B");
        assert_eq!(events[0]["name"], "agent");
        assert_eq!(events[1]["ph"], "B");
        assert_eq!(events[1]["name"], "model_call");
        assert_eq!(events[2]["ph"], "E");
        assert_eq!(events[3]["ph"], "E");
        assert_eq!(events[4]["ph"], "i");
    }
}
