//! Terminal progress bar and indicators for import and long-running operations.

use indicatif::{ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressMode {
    Auto,
    Always,
    Never,
}

impl ProgressMode {
    pub fn from_flags(progress: bool, no_progress: bool) -> Self {
        if no_progress {
            ProgressMode::Never
        } else if progress {
            ProgressMode::Always
        } else {
            ProgressMode::Auto
        }
    }

    pub fn should_show(&self) -> bool {
        match self {
            ProgressMode::Always => true,
            ProgressMode::Never => false,
            ProgressMode::Auto => std::io::stderr().is_terminal(),
        }
    }
}

/// Create a progress bar for importing sources.
///
/// When `total` is known (e.g. multiple explicit input files), a bounded bar is
/// displayed. When `total` is `None` (e.g. directory walking / expanding archives),
/// a dynamic spinner with count is displayed.
pub fn create_import_progress(mode: ProgressMode, total: Option<u64>) -> ProgressBar {
    if !mode.should_show() {
        return ProgressBar::hidden();
    }

    let pb = match total {
        Some(len) => {
            let pb = ProgressBar::new(len);
            let style = ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=>-")
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");
            pb.set_style(style);
            pb.enable_steady_tick(Duration::from_millis(100));
            pb
        }
        None => {
            let pb = ProgressBar::new_spinner();
            let style = ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] {pos} sources processed {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏");
            pb.set_style(style);
            pb.enable_steady_tick(Duration::from_millis(100));
            pb
        }
    };

    if mode == ProgressMode::Always {
        pb.set_draw_target(indicatif::ProgressDrawTarget::stderr());
    }

    pb
}

/// Increment progress by one source, expanding length dynamically if needed.
pub fn inc_progress(pb: &ProgressBar) {
    if let Some(len) = pb.length() {
        if pb.position() >= len {
            pb.set_length(pb.position() + 1);
        }
    }
    pb.inc(1);
}

/// Finish the progress bar with a final status message.
pub fn finish_progress(pb: &ProgressBar, msg: &str) {
    pb.finish_with_message(msg.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_mode_flags() {
        assert_eq!(
            ProgressMode::from_flags(false, false),
            ProgressMode::Auto
        );
        assert_eq!(
            ProgressMode::from_flags(true, false),
            ProgressMode::Always
        );
        assert_eq!(
            ProgressMode::from_flags(false, true),
            ProgressMode::Never
        );
        assert_eq!(
            ProgressMode::from_flags(true, true),
            ProgressMode::Never
        );
        assert!(ProgressMode::Always.should_show());
        assert!(!ProgressMode::Never.should_show());
    }

    #[test]
    fn hidden_progress_bar_when_never() {
        let pb = create_import_progress(ProgressMode::Never, Some(10));
        assert!(pb.is_hidden());
        inc_progress(&pb);
        finish_progress(&pb, "done");
    }

    #[test]
    fn bounded_progress_bar_when_always() {
        let pb = create_import_progress(ProgressMode::Always, Some(2));
        assert_eq!(pb.length(), Some(2));
        assert_eq!(pb.position(), 0);
        inc_progress(&pb);
        assert_eq!(pb.position(), 1);
        inc_progress(&pb);
        assert_eq!(pb.position(), 2);
        // Exceeding initial length dynamically expands length
        inc_progress(&pb);
        assert_eq!(pb.position(), 3);
        assert_eq!(pb.length(), Some(3));
        finish_progress(&pb, "done");
    }

    #[test]
    fn spinner_progress_bar_when_always_and_none_length() {
        let pb = create_import_progress(ProgressMode::Always, None);
        assert_eq!(pb.length(), None);
        assert_eq!(pb.position(), 0);
        inc_progress(&pb);
        assert_eq!(pb.position(), 1);
        finish_progress(&pb, "done");
    }
}

