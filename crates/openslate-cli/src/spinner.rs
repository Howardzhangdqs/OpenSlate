//! Spinner display for model calls using indicatif.
//!
//! Shows a visual spinner during model API calls with two phases:
//! - Waiting: `⠋ Waiting for {model}... {elapsed}s`
//! - Generating: `⠋ {model} generating... {elapsed}s · ↓{tokens}tok · {tok/s}tok/s`

use indicatif::{ProgressBar, ProgressStyle};

const TICK_CHARS: &[&str] = &[
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⏳",
];

/// Manages a spinner display during model calls.
pub struct Spinner {
    pb: ProgressBar,
    model: String,
    quiet: bool,
    total_tokens: u64,
}

impl Spinner {
    /// Create a new spinner for the given model name.
    ///
    /// If `quiet` is true, the spinner is hidden.
    pub fn new(model: &str, quiet: bool) -> Self {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner} {msg}")
                .expect("valid template")
                .tick_strings(TICK_CHARS),
        );
        pb.set_message(format!("Waiting for {}...", model));
        pb.enable_steady_tick(std::time::Duration::from_millis(100));

        if quiet {
            pb.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        }

        Self {
            pb,
            model: model.to_owned(),
            quiet,
            total_tokens: 0,
        }
    }

    /// Transition to the generating phase with token count info.
    #[allow(dead_code)]
    pub fn set_generating(&mut self) {
        let elapsed = elapsed_string(&self.pb);
        self.pb.set_message(format!(
            "{} generating... {}",
            self.model, elapsed
        ));
    }

    /// Update the spinner with a new delta content chunk.
    #[allow(dead_code)]
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

    /// Finish the spinner, clearing it from the terminal.
    pub fn finish(self) {
        if !self.quiet {
            let elapsed = elapsed_string(&self.pb);
            self.pb.finish_with_message(format!(
                "✓ {} done · {}tok · {}",
                self.model, self.total_tokens, elapsed
            ));
        } else {
            self.pb.finish_and_clear();
        }
    }

    /// Finish the spinner with an error message.
    pub fn finish_with_error(self, error: &str) {
        if !self.quiet {
            self.pb.finish_with_message(format!("✗ {} error: {}", self.model, error));
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
        self.pb.set_message(format!(
            "{} generating... {} · ↓{}tok · {}tok/s",
            self.model, elapsed, self.total_tokens, tok_per_s
        ));
    }
}

/// Format spinner for the waiting phase (exported for testing).
#[allow(dead_code)]
pub fn format_waiting_message(model: &str) -> String {
    format!("Waiting for {}...", model)
}

/// Format spinner for the generating phase (exported for testing).
#[allow(dead_code)]
pub fn format_generating_message(model: &str, tokens: u64, tok_per_s: u64, elapsed: &str) -> String {
    format!(
        "{} generating... {} · ↓{}tok · {}tok/s",
        model, elapsed, tokens, tok_per_s
    )
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
    fn test_format_waiting_message() {
        let msg = format_waiting_message("glm-5.1");
        assert_eq!(msg, "Waiting for glm-5.1...");
    }

    #[test]
    fn test_format_generating_message() {
        let msg = format_generating_message("glm-5.1", 42, 10, "4.2s");
        assert_eq!(msg, "glm-5.1 generating... 4.2s · ↓42tok · 10tok/s");
    }

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
        spinner.set_generating();
        // Should not panic
        spinner.finish();
    }

    #[test]
    fn test_spinner_finish_with_error() {
        let spinner = Spinner::new("test-model", false);
        spinner.finish_with_error("timeout");
        // Should not panic
    }
}
