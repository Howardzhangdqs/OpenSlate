//! Spinner display for model calls using indicatif.
//!
//! Shows a visual spinner during model API calls with two phases:
//! - Waiting: `⠋ {model} {elapsed}`
//! - Generating: `⠋ {model} {elapsed} · TTFT {ttft}s · ↓{tokens}tok · {tok/s}tok/s`
//!
//! On completion the spinner is replaced by a final summary line:
//! `✓ {model} {elapsed} · TTFT {ttft}s · ↑{in}↓{out}tok · {tok/s}tok/s`
//!
//! The elapsed time is rendered by indicatif's built-in `{elapsed_precise}`
//! template field, so it auto-updates on every tick — including during the
//! waiting phase before the first token arrives.

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
    input_tokens: Option<u32>,
    ttft: Option<Duration>,
}

impl Spinner {
    /// Create a new spinner for the given model name.
    ///
    /// If `quiet` is true, the spinner is hidden.
    ///
    /// The template bakes the model name in as a literal and uses indicatif's
    /// built-in `{elapsed_precise}` so the elapsed time auto-updates on every
    /// steady tick — even during the waiting phase before the first token.
    pub fn new(model: &str, quiet: bool) -> Self {
        let pb = if quiet {
            ProgressBar::hidden()
        } else {
            let pb = ProgressBar::new_spinner();
            // Template:  ⠋ {model} {elapsed}{msg}
            //   - {elapsed_precise} auto-updates every tick (e.g. "1.23s")
            //   - {msg} is empty during waiting, fills with token info during generation
            let template = format!("{{spinner}} {} {{elapsed_precise}}{{msg}}", model);
            pb.set_style(
                ProgressStyle::with_template(&template)
                    .expect("valid template")
                    .tick_strings(TICK_CHARS),
            );
            pb.enable_steady_tick(std::time::Duration::from_millis(100));
            pb
        };
        // Empty message for the waiting phase — model + elapsed come from the template.
        pb.set_message("");

        Self {
            pb,
            model: model.to_owned(),
            quiet,
            total_tokens: 0,
            input_tokens: None,
            ttft: None,
        }
    }

    /// Reset to the waiting phase.
    pub fn set_waiting(&mut self) {
        self.total_tokens = 0;
        self.input_tokens = None;
        self.ttft = None;
        // Empty message — model + elapsed come from the template, nothing else to show.
        self.pb.set_message("");
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

    /// Update with actual usage info from the provider.
    pub fn on_usage(&mut self, usage: Usage) {
        self.total_tokens = usage.output_tokens as u64;
        self.input_tokens = Some(usage.input_tokens);
        self.update_generating_message();
    }

    /// Print a line above the spinner without disrupting it.
    pub fn println(&self, msg: &str) {
        self.pb.println(msg);
    }

    /// Finish the spinner, replacing it with a final summary line.
    ///
    /// Summary format:
    /// `✓ {model} {elapsed}s · TTFT {ttft}s · ↑{in}↓{out}tok · {tok/s}tok/s`
    ///
    /// Falls back gracefully when usage/ttft data is unavailable.
    pub fn finish(self) {
        if !self.quiet {
            let summary = self.build_summary_line(true);
            // Switch to a plain {msg} template so the baked-in model/elapsed
            // don't double-render on the final line.
            self.pb
                .set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
            self.pb.finish_with_message(summary);
        } else {
            self.pb.finish_and_clear();
        }
    }

    /// Finish the spinner with an error message.
    pub fn finish_with_error(self, error: &str) {
        if !self.quiet {
            self.pb
                .set_style(ProgressStyle::with_template("{msg}").expect("valid template"));
            self.pb
                .finish_with_message(format!("✗ {} error: {}", self.model, error));
        } else {
            self.pb.finish_and_clear();
        }
    }

    /// Build the final summary line.
    ///
    /// When `success` is true the line is prefixed with `✓`, otherwise `✗`.
    fn build_summary_line(&self, success: bool) -> String {
        let mark = if success { "✓" } else { "✗" };
        let elapsed = elapsed_string(&self.pb);
        let elapsed_secs = elapsed_secs(&self.pb);

        let ttft_str = self
            .ttft
            .map(|t| format!(" · TTFT {:.1}s", t.as_secs_f64()))
            .unwrap_or_default();

        let tok_per_s = if elapsed_secs > 0.0 {
            (self.total_tokens as f64 / elapsed_secs).round() as u64
        } else {
            0
        };

        let tokens_str = if self.total_tokens > 0 {
            match self.input_tokens {
                Some(in_tok) => format!(" · ↑{}↓{}tok", in_tok, self.total_tokens),
                None => format!(" · ↓{}tok", self.total_tokens),
            }
        } else {
            String::new()
        };

        let tps_str = if tok_per_s > 0 {
            format!(" · {}tok/s", tok_per_s)
        } else {
            String::new()
        };

        format!(
            "{} {} {}{}{}{}",
            mark, self.model, elapsed, ttft_str, tokens_str, tps_str
        )
    }

    fn update_generating_message(&self) {
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
        // Elapsed comes from the template ({elapsed_precise}), so the message only
        // carries the token/throughput suffix.
        self.pb.set_message(format!(
            "{} · ↓{}tok · {}tok/s",
            ttft_str, self.total_tokens, tok_per_s
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
        self.spinner.on_usage(usage);
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
///
/// During waiting, only the model name and elapsed time are shown (both come
/// from the template), so the message portion is empty.
#[allow(dead_code)]
pub fn format_waiting_message(_model: &str) -> String {
    String::new()
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
        spinner.on_usage(Usage {
            input_tokens: 50,
            output_tokens: 100,
        });
        assert_eq!(spinner.total_tokens, 100);
        assert_eq!(spinner.input_tokens, Some(50));
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
        spinner.on_usage(Usage {
            input_tokens: 30,
            output_tokens: 100,
        });
        spinner.set_generating(Duration::from_millis(200));
        spinner.set_waiting();
        assert_eq!(spinner.total_tokens, 0);
        assert!(spinner.ttft.is_none());
        assert!(spinner.input_tokens.is_none());
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
