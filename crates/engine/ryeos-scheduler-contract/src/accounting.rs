//! Offline reset report vocabulary; no SQLite dependency.

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerFireHistoryDiscardReport {
    pub fires: usize,
    pub outbox_snapshots: usize,
    pub cursors: usize,
}

impl SchedulerFireHistoryDiscardReport {
    pub fn total_rows(&self) -> Option<usize> {
        self.fires
            .checked_add(self.outbox_snapshots)?
            .checked_add(self.cursors)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SchedulerFireHistoryAccounting {
    Exact {
        counts: SchedulerFireHistoryDiscardReport,
    },
    UnavailableIncompatibleSchema {},
}

impl SchedulerFireHistoryAccounting {
    pub fn total_rows(&self) -> Option<usize> {
        match self {
            Self::Exact { counts } => counts.total_rows(),
            Self::UnavailableIncompatibleSchema {} => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_accounting_round_trips_exact_and_unavailable_evidence() {
        for value in [
            SchedulerFireHistoryAccounting::Exact {
                counts: SchedulerFireHistoryDiscardReport {
                    fires: 2,
                    outbox_snapshots: 3,
                    cursors: 4,
                },
            },
            SchedulerFireHistoryAccounting::UnavailableIncompatibleSchema {},
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            assert_eq!(
                serde_json::from_slice::<SchedulerFireHistoryAccounting>(&bytes).unwrap(),
                value
            );
        }
        let exact = SchedulerFireHistoryDiscardReport {
            fires: 2,
            outbox_snapshots: 3,
            cursors: 4,
        };
        assert_eq!(exact.total_rows(), Some(9));
        assert_eq!(
            (SchedulerFireHistoryAccounting::UnavailableIncompatibleSchema {}).total_rows(),
            None
        );
    }

    #[test]
    fn strict_accounting_refuses_missing_extra_negative_fractional_counts() {
        for value in [
            serde_json::json!({"status":"exact", "counts":{"fires":1,"cursors":0}}),
            serde_json::json!({"status":"exact", "counts":{"fires":1,"outbox_snapshots":0,"cursors":0,"other":0}}),
            serde_json::json!({"status":"exact", "counts":{"fires":-1,"outbox_snapshots":0,"cursors":0}}),
            serde_json::json!({"status":"exact", "counts":{"fires":1.5,"outbox_snapshots":0,"cursors":0}}),
            serde_json::json!({"status":"unavailable_incompatible_schema", "counts":{"fires":0,"outbox_snapshots":0,"cursors":0}}),
            serde_json::json!({"status":"exact", "counts":{"fires":0,"outbox_snapshots":0,"cursors":0},"authority":true}),
            serde_json::json!({"status":"unknown"}),
        ] {
            assert!(serde_json::from_value::<SchedulerFireHistoryAccounting>(value).is_err());
        }
    }

    #[test]
    fn aggregate_overflow_never_becomes_an_exact_total() {
        for counts in [
            SchedulerFireHistoryDiscardReport {
                fires: usize::MAX,
                outbox_snapshots: 1,
                cursors: 0,
            },
            SchedulerFireHistoryDiscardReport {
                fires: usize::MAX - 1,
                outbox_snapshots: 1,
                cursors: 1,
            },
        ] {
            assert_eq!(counts.total_rows(), None);
            assert_eq!(
                SchedulerFireHistoryAccounting::Exact { counts }.total_rows(),
                None
            );
        }
    }
}
