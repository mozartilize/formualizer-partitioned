//! One evaluation instant for every workbook a call builds.
//!
//! `NOW` and `TODAY` read the engine clock. The partitioned path builds a fresh
//! engine per batch, and the whole-file path builds one more, so an unpinned
//! clock lets those engines answer differently for the same file. That is a
//! disagreement between two paths that must return the same grid, not a
//! property of the workbook.
//!
//! Every entry point starts a run. Every workbook built during that run reads
//! the instant the run started, so all of them agree. A later call starts a new
//! run and sees the new time.
//!
//! The UTC offset is captured with the instant. The engine default is the local
//! timezone, and deterministic mode rejects it, so a fixed offset stands in for
//! the local wall clock rather than silently moving `NOW` to UTC.

use std::cell::Cell;

use chrono::{DateTime, Local, Utc};
use formualizer::eval::engine::DeterministicMode;
use formualizer::eval::timezone::TimeZoneSpec;
use formualizer::workbook::WorkbookConfig;

thread_local! {
    static RUN: Cell<Option<(DateTime<Utc>, i32)>> = const { Cell::new(None) };
}

/// Start a run. Call this first in every entry point that evaluates.
pub fn begin_run() {
    begin_run_at(None);
}

/// Start a run at a caller-chosen instant, given as Unix seconds.
///
/// Two calls are two runs, so `NOW` may move by a second between them. A
/// caller comparing one evaluation with another passes the same instant to
/// both, which is what the corpus harness does.
pub fn begin_run_at(unix_seconds: Option<f64>) {
    let instant = match unix_seconds {
        Some(seconds) => DateTime::from_timestamp_nanos((seconds * 1e9) as i64),
        None => Utc::now(),
    };
    let offset = Local::now().offset().local_minus_utc();
    RUN.with(|run| run.set(Some((instant, offset))));
}

/// Pin `config` to the instant this run started.
pub fn pin(mut config: WorkbookConfig) -> WorkbookConfig {
    // A caller that builds a workbook without starting a run gets the current
    // time, which is what an unpinned engine would have read anyway.
    if RUN.with(|run| run.get()).is_none() {
        begin_run();
    }
    let (timestamp_utc, offset) = RUN.with(|run| run.get()).expect("run started above");
    config.eval.deterministic_mode = DeterministicMode::Enabled {
        timestamp_utc,
        timezone: TimeZoneSpec::FixedOffsetSeconds(offset),
    };
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use formualizer::common::value::LiteralValue;
    use formualizer::workbook::Workbook;

    /// `NOW` truncates to whole seconds, so two unpinned engines agree until
    /// the wall clock crosses a second boundary and then disagree. Crossing one
    /// deliberately shows the pin holding, and shows a later run moving on.
    fn now_in_a_fresh_workbook() -> LiteralValue {
        let mut wb = Workbook::new_with_config(pin(WorkbookConfig::ephemeral()));
        wb.add_sheet("S").unwrap();
        wb.set_formula("S", 1, 1, "=NOW()").unwrap();
        wb.evaluate_all().unwrap();
        wb.get_value("S", 1, 1).unwrap()
    }

    #[test]
    fn one_run_gives_every_workbook_the_same_now() {
        begin_run();
        let first = now_in_a_fresh_workbook();

        let subsecond = Utc::now().timestamp_subsec_nanos() as u64;
        std::thread::sleep(std::time::Duration::from_nanos(
            1_000_000_000 - subsecond + 20_000_000,
        ));

        assert_eq!(first, now_in_a_fresh_workbook(), "same run, same instant");
        begin_run();
        assert_ne!(first, now_in_a_fresh_workbook(), "a new run reads the clock");
    }
}
