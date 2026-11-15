//! Shared monotonic timing context for one application launch transaction.

use std::time::Instant;

/// Timing origin shared by the MIDI and OSC workers for one launch transaction.
/// `go_received_at` is retained through waits and active Cue completion when a
/// transaction descends from an operator/remote GO. It is absent for independent
/// timer or external-trigger transactions that have no GO ancestor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DispatchTiming {
    pub go_received_at: Option<Instant>,
    pub transaction_started_at: Instant,
}

impl DispatchTiming {
    /// Elapsed microseconds since GO, or `None` when this transaction was not
    /// initiated by GO or when the measured stage predates the GO timestamp.
    pub(crate) fn go_elapsed_us(self, at: Instant) -> Option<u128> {
        self.go_received_at.and_then(|started_at| {
            at.checked_duration_since(started_at)
                .map(|elapsed| elapsed.as_micros())
        })
    }

    /// When an outer input-poll transaction already existed before a GO action
    /// was handled inside it, report that offset instead of inventing a zero GO
    /// to transaction-start latency.
    pub(crate) fn transaction_to_go_received_us(self) -> Option<u128> {
        self.go_received_at.and_then(|go_at| {
            go_at
                .checked_duration_since(self.transaction_started_at)
                .map(|elapsed| elapsed.as_micros())
        })
    }

    /// Elapsed microseconds since the logical launch transaction began.
    pub(crate) fn transaction_elapsed_us(self, at: Instant) -> u128 {
        at.saturating_duration_since(self.transaction_started_at)
            .as_micros()
    }
}

#[cfg(test)]
mod tests {
    use super::DispatchTiming;
    use std::time::{Duration, Instant};

    #[test]
    fn dispatch_timing_reports_go_and_transaction_offsets() {
        let origin = Instant::now();
        let transaction_started_at = origin + Duration::from_millis(2);
        let mark = origin + Duration::from_millis(7);
        let timing = DispatchTiming {
            go_received_at: Some(origin),
            transaction_started_at,
        };

        assert_eq!(timing.go_elapsed_us(mark), Some(7_000));
        assert_eq!(timing.transaction_elapsed_us(mark), 5_000);
    }

    #[test]
    fn dispatch_timing_does_not_report_transaction_start_before_go_as_zero_latency() {
        let transaction_started_at = Instant::now();
        let go_received_at = transaction_started_at + Duration::from_millis(3);
        let timing = DispatchTiming {
            go_received_at: Some(go_received_at),
            transaction_started_at,
        };

        assert_eq!(timing.go_elapsed_us(transaction_started_at), None);
        assert_eq!(timing.transaction_to_go_received_us(), Some(3_000));
        assert_eq!(timing.go_elapsed_us(go_received_at), Some(0));
    }

    #[test]
    fn dispatch_timing_marks_non_go_transactions_as_not_applicable() {
        let now = Instant::now();
        let timing = DispatchTiming {
            go_received_at: None,
            transaction_started_at: now,
        };

        assert_eq!(timing.go_elapsed_us(now), None);
        assert_eq!(timing.transaction_elapsed_us(now), 0);
    }
}
