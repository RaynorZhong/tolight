//! Short, key-revalidated transactions for the private conformance state.
//! The coordinator supplies a pure decision function; SQL remains here.
use super::{Agent, Result, Store, check_key, db, lock_agent};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tolight_core::models::AgentContext;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ConformanceRun {
    pub run_id: String,
    pub agent_id: String,
    pub suite_version: String,
    pub started_at: DateTime<Utc>,
    pub event_ids: Vec<String>,
    pub limited_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub passed: Option<bool>,
    pub failure_reason: Option<String>,
}
impl ConformanceRun {
    pub fn candidate(agent_id: &str, now: DateTime<Utc>, ordinal: usize) -> Self {
        Self {
            // The monotonic per-identity prefix disambiguates starts on an equal
            // injected clock (and wall-clock rollback), without reusing identities.
            run_id: format!("R-{ordinal:020}-{}", Uuid::new_v4()),
            agent_id: agent_id.into(),
            suite_version: tolight_core::CONFORMANCE_SUITE_VERSION.into(),
            started_at: now,
            event_ids: (0..4).map(|_| format!("E-{}", Uuid::new_v4())).collect(),
            limited_at: None,
            finished_at: None,
            passed: None,
            failure_reason: None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ConformanceData {
    pub agent: Agent,
    pub runs: Vec<ConformanceRun>,
}
impl ConformanceData {
    pub fn last_run(&self) -> Option<&ConformanceRun> {
        self.runs.last()
    }
    pub fn current(&self) -> Option<&ConformanceRun> {
        self.runs.iter().find(|run| run.finished_at.is_none())
    }
    pub fn current_mut(&mut self) -> Option<&mut ConformanceRun> {
        self.runs.iter_mut().find(|run| run.finished_at.is_none())
    }
    pub fn runs_today(&self, now: DateTime<Utc>) -> usize {
        self.runs
            .iter()
            .filter(|run| run.started_at.date_naive() == now.date_naive())
            .count()
    }
}
impl Store {
    /// The callback must not perform I/O. Its return may itself be a Result:
    /// business rejections (429/403) still commit the session transition.
    pub async fn conformance_transaction<T, F>(
        &self,
        context: &AgentContext,
        decide: F,
    ) -> Result<T>
    where
        T: Send,
        F: FnOnce(&mut ConformanceData) -> T + Send,
    {
        let mut tx = self.pool.begin().await.map_err(db)?;
        let agent = lock_agent(&mut tx, &context.agent_id).await?;
        check_key(&agent, &context.public_key)?;
        let runs: Vec<ConformanceRun> = sqlx::query_as("SELECT run_id,agent_id,suite_version,started_at,event_ids,limited_at,finished_at,passed,failure_reason FROM conformance_runs WHERE agent_id=$1 ORDER BY run_id")
            .bind(&context.agent_id).fetch_all(&mut *tx).await.map_err(db)?;
        let original_runs = runs.clone();
        let original_basis = (
            agent.conformance_passed_at,
            agent.conformance_suite_version.clone(),
        );
        let mut data = ConformanceData { agent, runs };
        let output = decide(&mut data);
        for run in &data.runs {
            if original_runs.iter().any(|old| old == run) {
                continue;
            }
            sqlx::query("INSERT INTO conformance_runs (run_id,agent_id,suite_version,started_at,event_ids,limited_at,finished_at,passed,failure_reason) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(run_id) DO UPDATE SET limited_at=EXCLUDED.limited_at,finished_at=EXCLUDED.finished_at,passed=EXCLUDED.passed,failure_reason=EXCLUDED.failure_reason")
                .bind(&run.run_id).bind(&run.agent_id).bind(&run.suite_version).bind(run.started_at).bind(&run.event_ids)
                .bind(run.limited_at).bind(run.finished_at).bind(run.passed).bind(&run.failure_reason).execute(&mut *tx).await.map_err(db)?;
        }
        if original_basis
            != (
                data.agent.conformance_passed_at,
                data.agent.conformance_suite_version.clone(),
            )
        {
            sqlx::query("UPDATE agents SET conformance_passed_at=$2,conformance_suite_version=$3,application_revision=application_revision+1 WHERE agent_id=$1")
                .bind(&context.agent_id).bind(data.agent.conformance_passed_at).bind(&data.agent.conformance_suite_version)
                .execute(&mut *tx).await.map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        Ok(output)
    }

    /// Operator metadata, in one snapshot; does not start or advance a session.
    pub async fn conformance_runs(&self, agent_id: &str) -> Result<Vec<ConformanceRun>> {
        sqlx::query_as("SELECT run_id,agent_id,suite_version,started_at,event_ids,limited_at,finished_at,passed,failure_reason FROM conformance_runs WHERE agent_id=$1 ORDER BY run_id")
            .bind(agent_id).fetch_all(&self.pool).await.map_err(db)
    }
}
