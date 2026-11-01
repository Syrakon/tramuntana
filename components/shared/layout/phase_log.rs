/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A log of slow work on the script thread, split into phases.
//!
//! With `RUMB_SLOW_RENDER_MS=<ms>` set, every span opened with [`PhaseLog::begin`] that
//! takes at least that long prints one line to stderr: when it started (Unix time in ms,
//! to line it up with timestamps taken by page script), its total time, how long each
//! phase marked with [`mark`] took, and how many spans ran nested inside it. Without the
//! variable, every call returns at once.

use std::cell::RefCell;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static THRESHOLD_MS: LazyLock<Option<f64>> =
    LazyLock::new(|| std::env::var("RUMB_SLOW_RENDER_MS").ok()?.parse().ok());

struct Span {
    start: Instant,
    unix_start_ms: f64,
    marks: Vec<(&'static str, Instant)>,
    nested_count: u32,
    nested_time: Duration,
}

thread_local! {
    static OPEN_SPANS: RefCell<Vec<Span>> = const { RefCell::new(Vec::new()) };
}

fn as_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// An open span. Dropping it without calling [`PhaseLog::finish`] closes it silently.
pub struct PhaseLog(());

impl PhaseLog {
    /// Opens a span on this thread, or returns `None` when the log is off.
    pub fn begin() -> Option<Self> {
        THRESHOLD_MS.as_ref()?;
        let unix_start_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, as_ms);
        OPEN_SPANS.with_borrow_mut(|spans| {
            spans.push(Span {
                start: Instant::now(),
                unix_start_ms,
                marks: Vec::new(),
                nested_count: 0,
                nested_time: Duration::ZERO,
            })
        });
        Some(PhaseLog(()))
    }

    /// Closes the span and prints it if it reached the threshold. `describe` names the
    /// work and is only called when the line is printed.
    pub fn finish(self, describe: impl FnOnce() -> String) {
        let Some(threshold) = *THRESHOLD_MS else {
            return;
        };
        let line = OPEN_SPANS.with_borrow(|spans| {
            let span = spans.last()?;
            let total = as_ms(span.start.elapsed());
            if total < threshold {
                return None;
            }
            let mut line = format!("[slow-render] t={:.0} total={total:.1}ms", span.unix_start_ms);
            let mut previous = span.start;
            for (name, at) in &span.marks {
                line.push_str(&format!(" {name}={:.1}", as_ms(*at - previous)));
                previous = *at;
            }
            if span.nested_count > 0 {
                line.push_str(&format!(
                    " nested={}/{:.1}ms",
                    span.nested_count,
                    as_ms(span.nested_time)
                ));
            }
            Some(line)
        });
        if let Some(line) = line {
            eprintln!("{line} | {}", describe());
        }
    }
}

impl Drop for PhaseLog {
    fn drop(&mut self) {
        OPEN_SPANS.with_borrow_mut(|spans| {
            let Some(span) = spans.pop() else {
                return;
            };
            if let Some(parent) = spans.last_mut() {
                parent.nested_count += 1;
                parent.nested_time += span.start.elapsed();
            }
        });
    }
}

/// Ends the current phase of the innermost open span on this thread, naming it.
pub fn mark(phase: &'static str) {
    if THRESHOLD_MS.is_none() {
        return;
    }
    OPEN_SPANS.with_borrow_mut(|spans| {
        if let Some(span) = spans.last_mut() {
            span.marks.push((phase, Instant::now()));
        }
    });
}
