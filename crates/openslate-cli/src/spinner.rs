//! Spinner display for model calls using indicatif.
//!
//! Shows a visual spinner during model API calls with two phases:
//! - Waiting: `⠋ {model} {elapsed}s`
//! - Generating: `⠋ {model} {elapsed}s · TTFT {ttft}s · ↓{tokens}tok · {tok/s}tok/s`

use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};
use openslate_core::provider::ProgressCallback;
use openslate_core::types::Usage;

const TICK_CHARS: &[&str] = &[
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⏳",
];

/// Manages a spinner display during model calls.
pub struct Spinner {
    pb: ProgressBar,
    model: String,
    quiet: bool,
    total_tokens: u64,
    ttft: Option<Duration>,
}

impl Spinner {
    /// Create a new spinner for the given model name.
    ///
    /// If `quiet` is true, the spinner is hidden.
    pub fn new(model: &str, quiet: bool) -> Self {
        let pb = if quiet {
            ProgressBar::hidden()
        } else {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::with_template("{spinner} {msg}")
                    .expect("valid template")
                    .tick_strings(TICK_CHARS),
            );
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            pb
        };
        pb.set_message(model.to_owned());

        Self {
            pb,
            model: model.to_owned(),
            quiet,
            total_tokens: 0,
            ttft: None,
        }
    }

    /// Reset to the waiting phase.
    pub fn set_waiting(&mut self) {
        self.total_tokens = 0;
        self.ttft = None;
        self.pb.set_message(self.model.clone());
    }

    /// Transition to the generating phase, recording TTFT.
    pub fn set_generating(&mut self, ttft: Duration) {
        self.ttft = Some(ttft);
        self.update_generating_message();
    }

    /// Update the spinner with a new delta content chunk.
    pub fn on_delta(&mut self, text: &str) {
        // Rough token count: ~4 chars per token
        let estimated_tokens = (text.len() as u64).max(1) / 4;
        self.total_tokens += estimated_tokens;
        self.update_generating_message();
    }

    /// Update with actual usage info.
    pub fn on_usage(&mut self, output_tokens: u32) {
        self.total_tokens = output_tokens as u64;
        self.update_generating_message();
    }

    /// Print a line above the spinner without disrupting it.
    pub fn println(&self, msg: &str) {
        self.pb.println(msg);
    }

    /// Finish the spinner, clearing it from the terminal.
    pub fn finish(self) {
        if !self.quiet {
            let elapsed = elapsed_string(&self.pb);
            let in_tok = self.ttft.is_some();
            self.pb.finish_with_message(format!(
                "✓ {} {}{}",
                self.model,
                elapsed,
                if self.total_tokens > 0 {
                    format!(" · {}tok", self.total_tokens)
                } else {
                    String::new()
                }
            ));
            let _ = in_tok; // suppress unused warning
        } else {
            self.pb.finish_and_clear();
        }
    }

    /// Finish the spinner with an error message.
    pub fn finish_with_error(self, error: &str) {
        if !self.quiet {
            self.pb
                .finish_with_message(format!("✗ {} error: {}", self.model, error));
        } else {
            self.pb.finish_and_clear();
        }
    }

    fn update_generating_message(&self) {
        let elapsed = elapsed_string(&self.pb);
        let elapsed_secs = elapsed_secs(&self.pb);
        let tok_per_s = if elapsed_secs > 0.0 {
            (self.total_tokens as f64 / elapsed_secs).round() as u64
        } else {
            0
        };
        let ttft_str = self
            .ttft
            .map(|t| format!(" · TTFT {:.1}s", t.as_secs_f64()))
            .unwrap_or_default();
        self.pb.set_message(format!(
            "{} {}{} · ↓{}tok · {}tok/s",
            self.model, elapsed, ttft_str, self.total_tokens, tok_per_s
        ));
    }
}

/// Bridges `ProgressCallback` events to `Spinner` display updates.
pub struct SpinnerCallback {
    spinner: Spinner,
    request_start: Instant,
}

impl SpinnerCallback {
    /// Create a new callback wrapping a spinner for the given model.
    pub fn new(model: &str, quiet: bool) -> Self {
        Self {
            spinner: Spinner::new(model, quiet),
            request_start: Instant::now(),
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
}

impl ProgressCallback for SpinnerCallback {
    fn on_request_start(&mut self, _step: u32, _model_id: &str) {
        self.request_start = Instant::now();
        self.spinner.set_waiting();
    }

    fn on_first_token(&mut self) {
        let ttft = self.request_start.elapsed();
        self.spinner.set_generating(ttft);
    }

    fn on_delta(&mut self, text: &str) {
        self.spinner.on_delta(text);
    }

    fn on_usage(&mut self, usage: Usage) {
        self.spinner.on_usage(usage.output_tokens);
    }

    fn on_request_end(&mut self) {
        // Spinner stays visible until finish() is called by the CLI.
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

/// Format spinner for the waiting phase (exported for testing).
#[allow(dead_code)]
pub fn format_waiting_message(model: &str) -> String {
    model.to_owned()
}

fn elapsed_string(pb: &ProgressBar) -> String {
    let secs = elapsed_secs(pb);
    format!("{:.1}s", secs)
}

fn elapsed_secs(pb: &ProgressBar) -> f64 {
    pb.elapsed().as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spinner_quiet_mode() {
        let spinner = Spinner::new("test-model", true);
        assert!(spinner.quiet);
        spinner.finish();
    }

    #[test]
    fn test_spinner_normal_mode() {
        let spinner = Spinner::new("test-model", false);
        assert!(!spinner.quiet);
        spinner.finish();
    }

    #[test]
    fn test_spinner_on_delta_updates_tokens() {
        let mut spinner = Spinner::new("test", false);
        spinner.on_delta("Hello world test"); // 16 chars / 4 = 4 tokens
        assert!(spinner.total_tokens > 0);
        spinner.finish();
    }

    #[test]
    fn test_spinner_on_usage() {
        let mut spinner = Spinner::new("test", false);
        spinner.on_usage(100);
        assert_eq!(spinner.total_tokens, 100);
        spinner.finish();
    }

    #[test]
    fn test_spinner_set_generating() {
        let mut spinner = Spinner::new("glm-5.1", false);
        spinner.set_generating(Duration::from_millis(500));
        assert!(spinner.ttft.is_some());
        spinner.finish();
    }

    #[test]
    fn test_spinner_set_waiting_resets() {
        let mut spinner = Spinner::new("test", false);
        spinner.on_usage(100);
        spinner.set_generating(Duration::from_millis(200));
        spinner.set_waiting();
        assert_eq!(spinner.total_tokens, 0);
        assert!(spinner.ttft.is_none());
        spinner.finish();
    }

    #[test]
    fn test_spinner_finish_with_error() {
        let spinner = Spinner::new("test-model", false);
        spinner.finish_with_error("timeout");
        // Should not panic
    }

    #[test]
    fn test_spinner_callback_implements_trait() {
        fn assert_progress_callback<T: ProgressCallback>() {}
        assert_progress_callback::<SpinnerCallback>();
    }
}
