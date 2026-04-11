use std::sync::Arc;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::{BudgetInfo, Database};
use crate::services::event_store::EventStoreService;

#[derive(Debug, Clone)]
pub struct BudgetService {
    db: Arc<Database>,
    events: Arc<EventStoreService>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetReserveParams {
    pub thread_id: String,
    pub budget_parent_id: String,
    pub reserved_spend: f64,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetReportParams {
    pub thread_id: String,
    pub actual_spend: f64,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetReleaseParams {
    pub thread_id: String,
    pub status: String,
    #[serde(default)]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetGetParams {
    pub thread_id: String,
}

impl BudgetService {
    pub fn new(db: Arc<Database>, events: Arc<EventStoreService>) -> Self {
        Self { db, events }
    }

    pub fn reserve(&self, params: &BudgetReserveParams) -> Result<BudgetInfo> {
        if params.reserved_spend < 0.0 {
            bail!("reserved_spend must be non-negative");
        }

        let (budget, persisted) = self.db.reserve_budget(
            &params.thread_id,
            &params.budget_parent_id,
            params.reserved_spend,
            params.metadata.as_ref(),
        )?;
        self.events.publish_persisted_batch(&persisted);
        Ok(budget)
    }

    pub fn report(&self, params: &BudgetReportParams) -> Result<BudgetInfo> {
        if params.actual_spend < 0.0 {
            bail!("actual_spend must be non-negative");
        }

        let (budget, persisted) = self.db.report_budget(
            &params.thread_id,
            params.actual_spend,
            params.metadata.as_ref(),
        )?;
        self.events.publish_persisted_batch(&persisted);
        Ok(budget)
    }

    pub fn release(&self, params: &BudgetReleaseParams) -> Result<BudgetInfo> {
        match params.status.as_str() {
            "released" | "cancelled" => {}
            other => bail!("invalid budget status: {other}"),
        }

        let (budget, persisted) =
            self.db
                .release_budget(&params.thread_id, &params.status, params.metadata.as_ref())?;
        self.events.publish_persisted_batch(&persisted);
        Ok(budget)
    }

    pub fn get(&self, params: &BudgetGetParams) -> Result<Option<BudgetInfo>> {
        self.db.get_budget(&params.thread_id)
    }
}
