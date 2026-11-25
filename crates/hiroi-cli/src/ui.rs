use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use hiroi_engine::{DownloadId, DownloadListener, DownloadSnapshot, DownloadStatus, EngineError};
use indicatif::{
    HumanBytes, HumanDuration, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle,
};

// `wide_msg` trims the message to the terminal width; wrapped lines would
// break redrawing.
const BAR_TEMPLATE: &str = "{prefix:.bold} [{bar:24.cyan/blue}] {percent:>3}% {wide_msg}";
const SPINNER_TEMPLATE: &str = "{prefix:.bold} {spinner} {wide_msg}";
const NAME_WIDTH: usize = 24;

/// Progress bars for the terminal, one per download.
#[derive(Debug)]
pub struct ProgressView {
    multi: MultiProgress,
    bars: Mutex<HashMap<DownloadId, ProgressBar>>,
}

impl ProgressView {
    pub fn new(hidden: bool) -> Self {
        let multi = MultiProgress::new();
        if hidden {
            multi.set_draw_target(ProgressDrawTarget::hidden());
        }
        Self {
            multi,
            bars: Mutex::new(HashMap::new()),
        }
    }

    /// Prints a line above the bars.
    pub fn note(&self, message: &str) {
        if self.multi.is_hidden() {
            eprintln!("{message}");
        } else {
            let _ = self.multi.println(message);
        }
    }

    /// Removes the bars once everything is settled, so the summary that
    /// follows starts on a clean line.
    pub fn finish(&self) {
        // Each bar first: a finished bar is drawn again by its ticker
        // after a clear of the whole set, which repeated the summary.
        let bars = self.bars.lock().unwrap_or_else(PoisonError::into_inner);
        for bar in bars.values() {
            bar.finish_and_clear();
        }
        let _ = self.multi.clear();
    }

    fn bar(&self, snapshot: &DownloadSnapshot) -> ProgressBar {
        let mut bars = self.bars.lock().unwrap_or_else(PoisonError::into_inner);
        let bar = bars.entry(snapshot.id).or_insert_with(|| {
            let style = ProgressStyle::with_template(SPINNER_TEMPLATE).expect("valid template");
            let bar = self.multi.add(ProgressBar::no_length().with_style(style));
            bar.enable_steady_tick(Duration::from_millis(200));
            bar
        });
        let name = snapshot
            .file_name
            .as_deref()
            .unwrap_or_else(|| url_tail(&snapshot.url));
        bar.set_prefix(shorten(name, NAME_WIDTH));
        match snapshot.total_size {
            Some(total) if bar.length() != Some(total) => {
                bar.set_style(
                    ProgressStyle::with_template(BAR_TEMPLATE)
                        .expect("valid template")
                        .progress_chars("=> "),
                );
                bar.set_length(total);
            }
            None if bar.length().is_none() => {
                bar.set_style(
                    ProgressStyle::with_template(SPINNER_TEMPLATE).expect("valid template"),
                );
            }
            _ => {}
        }
        bar.clone()
    }
}

impl DownloadListener for ProgressView {
    fn on_status_changed(&self, snapshot: &DownloadSnapshot) {
        let bar = self.bar(snapshot);
        match &snapshot.status {
            DownloadStatus::Queued => bar.set_message("queued"),
            DownloadStatus::Connecting => bar.set_message("connecting…"),
            DownloadStatus::Verifying => bar.set_message("verifying checksum…"),
            DownloadStatus::Paused => {
                bar.abandon_with_message(format!("paused at {}", HumanBytes(snapshot.downloaded)))
            }
            DownloadStatus::Cancelled => bar.abandon_with_message("cancelled"),
            _ => {}
        }
    }

    fn on_progress(&self, snapshot: &DownloadSnapshot) {
        let bar = self.bar(snapshot);
        bar.set_position(snapshot.downloaded);
        let mut message = HumanBytes(snapshot.downloaded).to_string();
        if let Some(total) = snapshot.total_size {
            message.push_str(&format!("/{}", HumanBytes(total)));
        }
        if snapshot.speed > 0 {
            message.push_str(&format!("  {}/s", HumanBytes(snapshot.speed)));
        }
        if let Some(eta) = snapshot.eta {
            message.push_str(&format!("  ETA {}", HumanDuration(eta)));
        }
        message.push_str(&format!("  {} conn", snapshot.connections));
        bar.set_message(message);
    }

    fn on_retry(&self, id: DownloadId, reason: &str, delay: Duration) {
        self.note(&format!(
            "{id} {reason}, retrying in {}",
            HumanDuration(delay)
        ));
    }

    fn on_completed(&self, snapshot: &DownloadSnapshot) {
        let bar = self.bar(snapshot);
        bar.set_position(snapshot.downloaded);
        bar.finish_with_message(format!("done, {}", HumanBytes(snapshot.downloaded)));
    }

    fn on_failed(&self, snapshot: &DownloadSnapshot, error: &EngineError) {
        self.bar(snapshot)
            .abandon_with_message(format!("failed: {error}"));
    }
}

/// Last path part of a display URL, for downloads that have no name yet.
fn url_tail(url: &str) -> &str {
    let path = url.split('?').next().unwrap_or(url);
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

fn shorten(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return format!("{name:<max$}");
    }
    let tail: String = name
        .chars()
        .rev()
        .take(max - 1)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}
