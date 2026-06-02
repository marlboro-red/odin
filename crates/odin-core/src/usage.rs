//! Token/cost accounting, shared by the trait surface and the public contract.

use serde::{Deserialize, Serialize};

/// Accounting for one or more provider invocations.
///
/// All fields are best-effort: not every CLI reports usage. **Convention: unreported
/// usage is recorded as `0`** — a genuine zero and an absent report are not
/// distinguished (these fields are `u64`, not `Option`). Cost is stored as integer
/// micro-dollars (`1_000_000` = `$1.00`) so the durable record never accumulates
/// floating-point drift.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    /// Prompt/input tokens (`0` if unreported).
    pub input_tokens: u64,
    /// Completion/output tokens (`0` if unreported).
    pub output_tokens: u64,
    /// Cost in USD micro-dollars (`1_000_000` = `$1.00`). Integer: no float drift.
    pub cost_micros: u64,
}

impl Usage {
    /// Folds another usage record into this one (for run-level aggregation).
    ///
    /// Saturating arithmetic: a pathological provider report can never panic the engine.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cost_micros = self.cost_micros.saturating_add(other.cost_micros);
    }

    /// Cost rendered as fractional US dollars, for display only.
    ///
    /// The durable record always stays in integer [`Self::cost_micros`]; this lossy
    /// conversion exists purely for human-facing output.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn cost_usd(&self) -> f64 {
        self.cost_micros as f64 / 1_000_000.0
    }
}

#[cfg(test)]
mod tests {
    use super::Usage;

    #[test]
    fn add_accumulates() {
        let mut a = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cost_micros: 1_500_000,
        };
        a.add(Usage {
            input_tokens: 2,
            output_tokens: 3,
            cost_micros: 500_000,
        });
        assert_eq!(a.input_tokens, 12);
        assert_eq!(a.output_tokens, 8);
        assert_eq!(a.cost_micros, 2_000_000);
        assert!((a.cost_usd() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn add_saturates_instead_of_overflowing() {
        let mut a = Usage {
            cost_micros: u64::MAX,
            ..Usage::default()
        };
        a.add(Usage {
            cost_micros: 10,
            ..Usage::default()
        });
        assert_eq!(a.cost_micros, u64::MAX);
    }
}
