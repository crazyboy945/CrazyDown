use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

pub struct Ui {
    multi: Option<MultiProgress>,
    quiet: bool,
    sink: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

impl Ui {
    pub fn new(quiet: bool, no_progress: bool) -> Self {
        let enabled = !quiet && !no_progress && std::io::stderr().is_terminal();
        let multi = enabled.then(MultiProgress::new);
        Self {
            multi,
            quiet,
            sink: None,
        }
    }

    pub fn with_sink(sink: impl Fn(String) + Send + Sync + 'static) -> Self {
        Self {
            multi: None,
            quiet: false,
            sink: Some(Arc::new(sink)),
        }
    }

    pub fn add_bar(&self, prefix: &str) -> ProgressBar {
        match &self.multi {
            Some(multi) => {
                let bar = multi.add(ProgressBar::new(0));
                bar.set_style(spinner_style());
                bar.set_prefix(prefix.to_string());
                bar.enable_steady_tick(Duration::from_millis(120));
                bar
            }
            None => ProgressBar::hidden(),
        }
    }

    pub fn log(&self, message: impl AsRef<str>) {
        if let Some(sink) = &self.sink {
            sink(message.as_ref().to_string());
            return;
        }
        if self.quiet {
            return;
        }
        match &self.multi {
            Some(multi) => {
                let _ = multi.println(message.as_ref());
            }
            None => eprintln!("{}", message.as_ref()),
        }
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        self.log(format!("警告: {}", message.as_ref()));
    }

    pub fn fail_bar(&self, bar: &ProgressBar, message: impl Into<String>) {
        bar.abandon_with_message(message.into());
    }
}

pub fn set_total(bar: &ProgressBar, total: u64) {
    if total == 0 {
        set_unknown(bar);
        return;
    }
    bar.set_style(determinate_style());
    bar.set_length(total);
}

pub fn set_unknown(bar: &ProgressBar) {
    bar.set_style(spinner_style());
    bar.set_length(0);
}

fn determinate_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} {prefix:.bold} [{bar:28.cyan/blue}] {percent:>3}% {bytes:>10}/{total_bytes:>10} {bytes_per_sec:>11} ETA {eta:>6} {msg}",
    )
    .expect("valid progress template")
    .progress_chars("=>-")
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.cyan} {prefix:.bold} {bytes:>10} {bytes_per_sec:>11} {msg}",
    )
    .expect("valid spinner template")
    .progress_chars("=>-")
}
