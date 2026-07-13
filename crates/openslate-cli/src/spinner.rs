//! Spinner display for model calls using indicatif.
//!
//! A background renderer thread redraws the spinner line ~12×/sec with the
//! elapsed time (1-decimal seconds — which indicatif's built-in `{elapsed}`
//! placeholders cannot format), TTFT, the streamed token split
//! (`↑in ↓content r reasoning`), and throughput. Token counters live in a
//! shared [`LiveState`]; the `Spinner` methods only mutate it.
//!
//! Final summary on completion:
//! `✓ {model} {elapsed}s · TTFT {t}s · ↑{in} ↓{out} · {tps}tok/s`

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};
use openslate_core::provider::ProgressCallback;
use openslate_core::types::Usage;

const TICK_CHARS: &[&str] = &[
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⏳",
];

const RENDER_INTERVAL: Duration = Duration::from_millis(80);

/// Mutable state shared between the `Spinner` (token updates, from the async
/// runtime) and the background renderer thread (display).
#[derive(Default)]
struct LiveState {
    content: u64,
    reasoning: u64,
    input: Option<u32>,
    ttft: Option<Duration>,
    /// When the current request started; elapsed is measured from here.
    start: Option<Instant>,
}

/// Manages a spinner display during model calls.
pub struct Spinner {
    pb: ProgressBar,
    model: String,
    quiet: bool,
    state: Arc<Mutex<LiveState>>,
    stop: Arc<AtomicBool>,
    render: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Create a new spinner for the given model name.
    ///
    /// If `quiet` is true, the spinner is hidden and no renderer thread runs.
    pub fn new(model: &str, quiet: bool) -> Self {
        let pb = if quiet {
            ProgressBar::hidden()
        } else {
            let pb = ProgressBar::new_spinner();
            // Template:  ⠋ {model}{msg}
            //   Elapsed / tokens / throughput all live in {msg}, computed by the
            //   renderer thread, so we control the exact format (1-decimal secs).
            let template = format!("{{spinner}} {}{{msg}}", model);
            pb.set_style(
                ProgressStyle::with_template(&template)
                    .expect("valid template")
                    .tick_strings(TICK_CHARS),
            );
            pb.enable_steady_tick(Duration::from_millis(100));
            pb
        };
        pb.set_message("");

        let state = Arc::new(Mutex::new(LiveState::default()));
        let stop = Arc::new(AtomicBool::new(false));

        let render = if quiet {
            None
        } else {
            let pb2 = pb.clone();
            let state2 = Arc::clone(&state);
            let stop2 = Arc::clone(&stop);
            Some(thread::spawn(move || loop {
                if stop2.load(Ordering::Relaxed) {
                    break;
                }
                let msg = render_message(&state2.lock().expect("live state lock"));
                pb2.set_message(msg);
                thread::sleep(RENDER_INTERVAL);
            }))
        };

        Self {
            pb,
            model: model.to_owned(),
            quiet,
            state,
            stop,
            render,
        }
    }

    /// Reset for a new request: zero the counters, clear TTFT, restart the clock.
    pub fn set_waiting(&mut self) {
        let mut s = self.state.lock().expect("live state lock");
        s.content = 0;
        s.reasoning = 0;
        s.input = None;
        s.ttft = None;
        s.start = Some(Instant::now());
    }

    /// Record TTFT and enter the generating phase.
    pub fn set_generating(&mut self, ttft: Duration) {
        self.state.lock().expect("live state lock").ttft = Some(ttft);
    }

    /// Add a content (answer) delta. Rough token count: ~4 chars/token.
    pub fn on_delta(&mut self, text: &str) {
        let est = (text.len() as u64).max(1) / 4;
        self.state.lock().expect("live state lock").content += est;
    }

    /// Add a reasoning/thinking delta (counted separately, shown as `r{N}`).
    pub fn on_reasoning(&mut self, text: &str) {
        let est = (text.len() as u64).max(1) / 4;
        self.state.lock().expect("live state lock").reasoning += est;
    }

    /// Early input-token estimate so `↑N` shows during streaming, before the
    /// provider's real usage arrives.
    pub fn set_input_estimate(&mut self, tokens: u32) {
        self.state.lock().expect("live state lock").input = Some(tokens);
    }

    /// Streaming estimate of content (answer) tokens generated so far.
    pub fn content_tokens(&self) -> u64 {
        self.state.lock().expect("live state lock").content
    }

    /// Streaming estimate of reasoning tokens generated so far.
    pub fn reasoning_tokens(&self) -> u64 {
        self.state.lock().expect("live state lock").reasoning
    }

    /// Real usage from the provider. When reasoning was streamed, keep the
    /// streamed content/reasoning split (additive); otherwise use the accurate
    /// output count for the content counter.
    pub fn on_usage(&mut self, usage: Usage) {
        let mut s = self.state.lock().expect("live state lock");
        s.input = Some(usage.input_tokens);
        if s.reasoning == 0 {
            s.content = usage.output_tokens as u64;
        }
    }

    /// Print a line above the spinner without disrupting it.
    pub fn println(&self, msg: &str) {
        self.pb.println(msg);
    }

    /// Finish with the success summary line.
    pub fn finish(mut self) {
        self.stop_renderer();
        if !self.quiet {
            let summary = {
                let s = self.state.lock().expect("live state lock");
                build_summary_line(&self.model, &s, true)
            };
            self.pb
                .set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
            self.pb.finish_with_message(summary);
        } else {
            self.pb.finish_and_clear();
        }
    }

    /// Finish with an error line.
    pub fn finish_with_error(mut self, error: &str) {
        self.stop_renderer();
        if !self.quiet {
            self.pb
                .set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
            self.pb
                .finish_with_message(format!("✗ {} error: {}", self.model, error));
        } else {
            self.pb.finish_and_clear();
        }
    }

    /// Stop the renderer thread (called by finish/finish_with_error/drop).
    fn stop_renderer(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.render.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        // Safety net in case finish() wasn't called.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.render.take() {
            let _ = h.join();
        }
    }
}

/// Build the live spinner message: ` {elapsed}s` while waiting, or
/// ` {elapsed}s · TTFT {t}s · ↑{in} ↓{c}r{r} · {tps}tok/s` while generating.
fn render_message(s: &LiveState) -> String {
    let mut parts: Vec<String> = Vec::new();
    let elapsed_secs = match s.start {
        Some(st) => {
            let e = st.elapsed().as_secs_f64();
            parts.push(format!("{:.1}s", e));
            e
        }
        None => 0.0,
    };
    // Only show TTFT/tokens/throughput once generating has begun.
    if let Some(t) = s.ttft {
        parts.push(format!("TTFT {:.1}s", t.as_secs_f64()));
        parts.push(token_segment(&s.input, s.content, s.reasoning));
        let total = s.content + s.reasoning;
        if elapsed_secs > 0.0 && total > 0 {
            let tps = (total as f64 / elapsed_secs).round() as u64;
            if tps > 0 {
                parts.push(format!("{}tok/s", tps));
            }
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" · "))
    }
}

/// Build the terminal summary line (`✓` on success, `✗` on error).
fn build_summary_line(model: &str, s: &LiveState, success: bool) -> String {
    let mark = if success { "✓" } else { "✗" };
    let elapsed = s.start.map(|st| st.elapsed()).unwrap_or_default();
    let elapsed_secs = elapsed.as_secs_f64();
    let total = s.content + s.reasoning;
    let tps = if elapsed_secs > 0.0 {
        (total as f64 / elapsed_secs).round() as u64
    } else {
        0
    };

    let mut parts: Vec<String> = vec![model.to_string(), format!("{:.1}s", elapsed_secs)];
    if let Some(t) = s.ttft {
        parts.push(format!("TTFT {:.1}s", t.as_secs_f64()));
    }
    parts.push(token_segment(&s.input, s.content, s.reasoning));
    if tps > 0 {
        parts.push(format!("{}tok/s", tps));
    }
    format!("{} {}", mark, parts.join(" · "))
}

/// Token segment in the user's format:
/// `↑569 ↓514r600` (content 514, reasoning 600), `↑569 ↓r600` (reasoning only),
/// `↑569 ↓514` (content only), or `↓0` (nothing yet).
fn token_segment(input: &Option<u32>, content: u64, reasoning: u64) -> String {
    let mut seg = String::new();
    if let Some(in_tok) = input {
        seg.push_str(&format!("↑{} ", in_tok));
    }
    seg.push('↓');
    match (content, reasoning) {
        (0, 0) => seg.push('0'),
        (c, 0) => seg.push_str(&c.to_string()),
        (0, r) => {
            seg.push('r');
            seg.push_str(&r.to_string());
        }
        (c, r) => {
            seg.push_str(&c.to_string());
            seg.push('r');
            seg.push_str(&r.to_string());
        }
    }
    seg
}

// ---------------------------------------------------------------------------
// SpinnerCallback — bridges ProgressCallback events to the Spinner.
// ---------------------------------------------------------------------------

/// Bridges `ProgressCallback` events to `Spinner` display updates.
pub struct SpinnerCallback {
    spinner: Spinner,
    request_start: Instant,
    /// Line buffer for reasoning/thinking chunks. Complete lines are printed
    /// above the spinner, dimmed.
    reasoning_buf: String,
    /// Whether the generating phase has begun for the current request (on the
    /// first token of ANY kind — reasoning or content). TTFT is captured once.
    started: bool,
}

impl SpinnerCallback {
    /// Create a new callback wrapping a spinner for the given model.
    pub fn new(model: &str, quiet: bool) -> Self {
        Self {
            spinner: Spinner::new(model, quiet),
            request_start: Instant::now(),
            reasoning_buf: String::new(),
            started: false,
        }
    }

    /// Consume the callback and finish the spinner.
    pub fn finish(self) {
        self.spinner.finish();
    }

    /// Consume the callback and finish with an error.
    pub fn finish_with_error(self, error: &str) {
        self.spinner.finish_with_error(error);
    }

    /// Streaming estimate of content (answer) tokens from the last run.
    pub fn content_tokens(&self) -> u64 {
        self.spinner.content_tokens()
    }

    /// Streaming estimate of reasoning/thinking tokens from the last run.
    pub fn reasoning_tokens(&self) -> u64 {
        self.spinner.reasoning_tokens()
    }

    /// Enter the generating phase on the first token of any kind. Idempotent.
    fn mark_started(&mut self) {
        if !self.started {
            self.started = true;
            self.spinner.set_generating(self.request_start.elapsed());
        }
    }

    /// Flush any buffered reasoning line above the spinner at turn end.
    fn flush_reasoning(&mut self) {
        let remainder = std::mem::take(&mut self.reasoning_buf);
        let trimmed = remainder.trim_end();
        if !trimmed.is_empty() {
            self.spinner.println(&format_dim_reasoning(trimmed));
        }
    }
}

impl ProgressCallback for SpinnerCallback {
    fn on_request_start(&mut self, _step: u32, _model_id: &str) {
        self.request_start = Instant::now();
        self.started = false;
        self.reasoning_buf.clear();
        self.spinner.set_waiting();
    }

    fn on_input_estimate(&mut self, tokens: u32) {
        self.spinner.set_input_estimate(tokens);
    }

    fn on_first_token(&mut self) {
        // Called by the runtime on the first CONTENT delta. If reasoning already
        // started the generating phase, this is a no-op.
        self.mark_started();
    }

    fn on_delta(&mut self, text: &str) {
        self.spinner.on_delta(text);
    }

    fn on_reasoning(&mut self, text: &str) {
        // Start generating on the first reasoning chunk so live token/throughput
        // info shows while the model thinks.
        self.mark_started();
        // Count reasoning tokens into the SEPARATE reasoning counter (`r{N}`).
        self.spinner.on_reasoning(text);
        // Line-buffer the reasoning text; print each complete line above the
        // spinner (dimmed) without disrupting the progress display.
        self.reasoning_buf.push_str(text);
        while let Some(idx) = self.reasoning_buf.find('\n') {
            let line: String = self.reasoning_buf.drain(..=idx).collect();
            let line = line.trim_end_matches('\n').trim_end();
            if !line.is_empty() {
                self.spinner.println(&format_dim_reasoning(line));
            }
        }
    }

    fn on_usage(&mut self, usage: Usage) {
        self.spinner.on_usage(usage);
    }

    fn on_request_end(&mut self) {
        // Flush any trailing reasoning line that didn't end in a newline.
        self.flush_reasoning();
    }

    fn on_tool_start(&mut self, name: &str, args: &str) {
        self.spinner.println(&format!("  -> {}({})", name, args));
    }

    fn on_tool_end(&mut self, name: &str, bytes: usize, truncated: bool) {
        let suffix = if truncated { " ..." } else { "" };
        self.spinner
            .println(&format!("  <- {} [{} bytes]{}", name, bytes, suffix));
    }
}

/// Format a reasoning/thinking line dimmed (gray). No prefix bar — long
/// thoughts wrap across lines and a per-line marker looks misaligned.
fn format_dim_reasoning(line: &str) -> String {
    format!("\x1b[2m{}\x1b[0m", line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_segment_formats() {
        assert_eq!(token_segment(&None, 0, 0), "↓0");
        assert_eq!(token_segment(&None, 514, 0), "↓514");
        assert_eq!(token_segment(&None, 0, 600), "↓r600");
        assert_eq!(token_segment(&None, 514, 600), "↓514r600");
        assert_eq!(token_segment(&Some(569), 514, 600), "↑569 ↓514r600");
        assert_eq!(token_segment(&Some(569), 0, 600), "↑569 ↓r600");
    }

    #[test]
    fn spinner_counts_content_and_reasoning_separately() {
        let mut spinner = Spinner::new("test", true);
        spinner.on_delta("hello world test"); // 16 chars / 4 = 4
        spinner.on_reasoning("abcdefgh"); // 8 chars / 4 = 2
        assert!(spinner.content_tokens() > 0);
        assert!(spinner.reasoning_tokens() > 0);
        assert_ne!(spinner.content_tokens(), spinner.reasoning_tokens());
        spinner.finish();
    }

    #[test]
    fn spinner_on_usage_keeps_split_when_reasoning_present() {
        let mut spinner = Spinner::new("test", true);
        spinner.on_reasoning("some reasoning text here"); // reasoning > 0
        spinner.on_usage(Usage {
            input_tokens: 50,
            output_tokens: 100,
        });
        // input updated from real usage; content NOT overwritten (reasoning split kept)
        assert_eq!(spinner.content_tokens(), 0); // no content deltas, not overridden
        spinner.finish();
    }

    #[test]
    fn spinner_on_usage_overrides_content_when_no_reasoning() {
        let mut spinner = Spinner::new("test", true);
        spinner.on_usage(Usage {
            input_tokens: 50,
            output_tokens: 100,
        });
        // no reasoning -> content counter takes the accurate output count
        assert_eq!(spinner.content_tokens(), 100);
        spinner.finish();
    }

    #[test]
    fn spinner_quiet_and_normal_modes_finish_cleanly() {
        Spinner::new("test-model", true).finish();
        Spinner::new("test-model", false).finish();
        Spinner::new("test-model", false).finish_with_error("timeout");
    }

    #[test]
    fn spinner_callback_implements_trait() {
        fn assert_progress_callback<T: ProgressCallback>() {}
        assert_progress_callback::<SpinnerCallback>();
    }
}
