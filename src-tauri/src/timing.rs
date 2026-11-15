//! Opt-in latency logging. Start the app with `WINDOW_SWITCHER_TIMING=1` to
//! log, for every opening, how long each step from the hotkey to a fully drawn
//! overlay took, and the same for the release that activates a window. When
//! the variable is unset nothing is measured or logged.

use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};

static ENABLED: OnceLock<bool> = OnceLock::new();
/// When the current opening started, keyed by its generation, so steps that
/// finish elsewhere (webview, icon worker, thumbnails) can report against it.
static OPENED: Mutex<Option<(u64, Instant)>> = Mutex::new(None);

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var_os("WINDOW_SWITCHER_TIMING").is_some_and(|value| value != "0")
    })
}

/// Milliseconds between two instants, rounded to 0.01 ms.
fn ms(from: Instant, to: Instant) -> f64 {
    (to.duration_since(from).as_secs_f64() * 100_000.0).round() / 100.0
}

/// Cumulative timestamps for one operation, logged as a single line.
pub struct Stopwatch {
    label: &'static str,
    start: Instant,
    marks: Vec<(&'static str, Instant)>,
}

impl Stopwatch {
    pub fn start(label: &'static str) -> Option<Self> {
        enabled().then(|| Self {
            label,
            start: Instant::now(),
            marks: Vec::with_capacity(8),
        })
    }

    pub fn start_instant(&self) -> Instant {
        self.start
    }
}

pub fn mark(watch: &mut Option<Stopwatch>, name: &'static str) {
    if let Some(watch) = watch {
        watch.marks.push((name, Instant::now()));
    }
}

pub fn finish(watch: Option<Stopwatch>, detail: &str) {
    let Some(watch) = watch else { return };
    let steps = watch
        .marks
        .iter()
        .map(|(name, at)| format!("{name}={}", ms(watch.start, *at)))
        .collect::<Vec<_>>()
        .join(" ");
    log::info!("[timing] {}: {steps} ms {detail}", watch.label);
}

/// Wall-clock time for comparing with the webview's clock, when enabled.
pub fn unix_ms() -> Option<f64> {
    if !enabled() {
        return None;
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs_f64() * 1000.0)
}

pub fn opened(generation: u64, at: Instant) {
    if let Ok(mut opened) = OPENED.lock() {
        *opened = Some((generation, at));
    }
}

/// Logs `stage` as milliseconds since opening `generation` began.
pub fn since_open(generation: u64, stage: &str, detail: &str) {
    if !enabled() {
        return;
    }
    let started = OPENED
        .lock()
        .ok()
        .and_then(|opened| *opened)
        .filter(|(opened, _)| *opened == generation);
    if let Some((_, at)) = started {
        log::info!(
            "[timing] open #{generation} {stage} at {} ms {detail}",
            ms(at, Instant::now())
        );
    }
}
