// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! Reporting HTTP handlers - filing obligations, runs, submissions, and
//! the jurisdiction registry reads. See reporting DESIGN.md §4.
//!
//! Every handler: verify (steps 1-4) → Shield gate (step 5, the ONLY
//! gate) → same-transaction tenant context → explicit `WHERE env_id`
//! (defense-in-depth) → commit → emit (membus → bus, DESIGN.md §2).

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth;
use crate::config::ReportingConfig;
use crate::policy::Engine;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<ReportingConfig>,
    pub pool: sqlx::PgPool,
    /// Shield policy engine - step 5 is the ONLY gate: handlers verify
    /// the token (steps 1-4) then evaluate a
    /// `bank8:reporting:<resource>:<action>` target; no JWT scope check
    /// remains.
    pub policy: Arc<Engine>,
    /// Shared Sentin8:Policy admin surface (§3.6) - same engine instance
    /// the request path evaluates.
    pub admin: sentin8::shield::admin::SharedAdminState,
    /// MemBus handle (principles §4.7 - membus → bus → API): state
    /// refreshes ride the newest-wins ephemeral tier; durable events ride
    /// the local outbox → relay → Bus. Fail-soft - a Valkey outage only
    /// degrades refresh, never a mutation.
    pub membus: sentin8::membus::ServiceBus,
}

/// Set the transaction-local RLS tenant context on a connection. MUST be
/// awaited on the same connection every subsequent tenant query runs on -
/// `set_config(..., true)` scopes the setting to the transaction
/// (architecture-principles §4.3d).
pub(crate) async fn set_tenant_context(conn: &mut sqlx::PgConnection, env_id: Uuid) {
    let _ = sqlx::query("SELECT sentin8_set_tenant_context($1, NULL, NULL, NULL, false, false, false, NULL, 'reporting')")
        .bind(env_id)
        .execute(conn)
        .await;
}

/// Emit a reporting event through both tiers (DESIGN.md §2 - membus →
/// bus): the MemBus state refresh first (fail-soft, newest-wins), then
/// the durable outbox row the shell relay drains to Bus
/// `POST /v1/produce` (idempotency_key = row id). Errors are logged and
/// ignored - the committed domain row stays authoritative and the relay
/// retries from the outbox.
pub(crate) async fn emit_event(
    state: &AppState,
    env_id: Uuid,
    event_type: &str,
    payload: serde_json::Value,
) {
    state
        .membus
        .publish_pubsub("reporting", event_type, payload.clone())
        .await;
    if let Err(e) = sentin8::bus::outbox::insert_outbox_row_pool(
        &state.pool,
        env_id,
        "reporting",
        event_type,
        &payload,
    )
    .await
    {
        tracing::warn!(error = %e, event_type = %event_type, "bank8:reporting: failed to emit outbox event");
    }
}

// ── DTOs ──────────────────────────────────────────────────────────────────

/// A filing obligation as returned by the obligation reads.
#[derive(Debug, Serialize, ToSchema)]
pub struct ObligationResponse {
    pub id: Uuid,
    pub template_id: Uuid,
    pub period: String,
    pub next_due: chrono::DateTime<chrono::Utc>,
    pub status: String,
}

/// A filing run as returned by the run reads.
#[derive(Debug, Serialize, ToSchema)]
pub struct RunResponse {
    pub id: Uuid,
    pub obligation_id: Uuid,
    pub period: String,
    pub status: String,
    pub artifact_ref: Option<String>,
}

/// Query for listing filing runs.
#[derive(Debug, Deserialize, ToSchema, utoipa::IntoParams)]
pub struct ListRunsQuery {
    #[serde(default)]
    pub status: Option<String>,
}

/// Request body for the Flow proxy-back `POST /v1/internal/steps/execute`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProxyBackRequest {
    pub step_kind: String,
    pub workflow_id: Uuid,
    pub step_name: String,
    pub input: serde_json::Value,
    pub user_data: serde_json::Value,
    pub step_config: Option<serde_json::Value>,
}

/// Response from the proxy-back handler.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProxyBackResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
}

impl ProxyBackResponse {
    pub fn success(output: serde_json::Value) -> Self {
        Self {
            status: "success".into(),
            output: Some(output),
            error: None,
            next_step: None,
        }
    }
    pub fn failure(err: impl Into<String>) -> Self {
        Self {
            status: "failed".into(),
            output: None,
            error: Some(err.into()),
            next_step: None,
        }
    }
}

fn row_to_obligation(row: &sqlx::postgres::PgRow) -> ObligationResponse {
    ObligationResponse {
        id: row.get("id"),
        template_id: row.get("template_id"),
        period: row.get("period"),
        next_due: row.get("next_due"),
        status: row.get("status"),
    }
}

fn row_to_run(row: &sqlx::postgres::PgRow) -> RunResponse {
    RunResponse {
        id: row.get("id"),
        obligation_id: row.get("obligation_id"),
        period: row.get("period"),
        status: row.get("status"),
        artifact_ref: row.get("artifact_ref"),
    }
}

// ── Health ────────────────────────────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/v1/health",
    responses(
        (status = 200, description = "Liveness (version, build)"),
    ),
    security(),
    tag = "meta",
)]
/// `GET /v1/health` - service health payload.
///
/// Carries the binary's build identity (`version`/`build`,) so
/// captain's monitor populates `make status`'s VERSION column for every
/// instance - the consuming BINARY's identity, never a shared crate's.
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let db_ok = sqlx::query("SELECT 1").execute(&state.pool).await.is_ok();
    Json(
        serde_json::json!({ "status": if db_ok { "ok" } else { "degraded" }, "service": "bank8:reporting", "version": sentin8::build_info::id(), "build": sentin8::build_info::build_hash() }),
    )
}

// ── Filing obligations ────────────────────────────────────────────────────

/// `GET /v1/obligations` - list filing obligations for the tenant env.
#[utoipa::path(get, path = "/v1/obligations", responses((status = 200, body = [ObligationResponse])))]
pub async fn list_obligations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ObligationResponse>>, (StatusCode, String)> {
    let a = auth::verify_token(&state.config, &headers).await?;
    crate::policy::require(
        &state.policy,
        &a,
        &headers,
        sentin8::shield::EvalTarget::new("bank8:reporting:obligations:read"),
    )
    .await?;
    // Same-transaction discipline (DESIGN.md §5): the tenant context is
    // transaction-local, so context + query share one connection/tx.
    let mut conn = state.pool.begin().await.map_err(crate::errors::err_db)?;
    set_tenant_context(&mut conn, a.env_id()).await;
    let rows = sqlx::query(
        r#"SELECT id, template_id, period, next_due, status
             FROM filing_obligations
            WHERE env_id = $1
            ORDER BY next_due ASC LIMIT 200"#,
    )
    .bind(a.env_id())
    .fetch_all(&mut *conn)
    .await
    .map_err(crate::errors::err_db)?;
    Ok(Json(rows.iter().map(row_to_obligation).collect()))
}

// ── Filing runs ───────────────────────────────────────────────────────────

/// `GET /v1/runs` - list filing runs, optionally by status.
#[utoipa::path(get, path = "/v1/runs", params(ListRunsQuery), responses((status = 200, body = [RunResponse])))]
pub async fn list_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListRunsQuery>,
) -> Result<Json<Vec<RunResponse>>, (StatusCode, String)> {
    let a = auth::verify_token(&state.config, &headers).await?;
    crate::policy::require(
        &state.policy,
        &a,
        &headers,
        sentin8::shield::EvalTarget::new("bank8:reporting:runs:read"),
    )
    .await?;
    let mut conn = state.pool.begin().await.map_err(crate::errors::err_db)?;
    set_tenant_context(&mut conn, a.env_id()).await;
    let rows = sqlx::query(
        r#"SELECT id, obligation_id, period, status, artifact_ref
             FROM filing_runs
            WHERE env_id = $1 AND ($2::text IS NULL OR status = $2)
            ORDER BY created_at DESC LIMIT 200"#,
    )
    .bind(a.env_id())
    .bind(q.status)
    .fetch_all(&mut *conn)
    .await
    .map_err(crate::errors::err_db)?;
    Ok(Json(rows.iter().map(row_to_run).collect()))
}

/// `GET /v1/runs/{id}` - filing run detail.
#[utoipa::path(get, path = "/v1/runs/{id}", responses((status = 200, body = RunResponse)))]
pub async fn get_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<RunResponse>, (StatusCode, String)> {
    let a = auth::verify_token(&state.config, &headers).await?;
    crate::policy::require(
        &state.policy,
        &a,
        &headers,
        sentin8::shield::EvalTarget::new("bank8:reporting:runs:read"),
    )
    .await?;
    let mut conn = state.pool.begin().await.map_err(crate::errors::err_db)?;
    set_tenant_context(&mut conn, a.env_id()).await;
    let row = sqlx::query(
        r#"SELECT id, obligation_id, period, status, artifact_ref
             FROM filing_runs WHERE id = $1 AND env_id = $2"#,
    )
    .bind(id)
    .bind(a.env_id())
    .fetch_one(&mut *conn)
    .await
    .map_err(crate::errors::err_db)?;
    Ok(Json(row_to_run(&row)))
}

/// `POST /v1/runs/{id}/regenerate` - reset a non-submitted run to draft
/// (guarded: terminal states never regress; corrections are amendments).
#[utoipa::path(post, path = "/v1/runs/{id}/regenerate", responses((status = 200, body = RunResponse), (status = 409)))]
pub async fn regenerate_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<RunResponse>, (StatusCode, String)> {
    let a = auth::verify_token(&state.config, &headers).await?;
    crate::policy::require(
        &state.policy,
        &a,
        &headers,
        sentin8::shield::EvalTarget::new("bank8:reporting:runs:write"),
    )
    .await?;
    let mut conn = state.pool.begin().await.map_err(crate::errors::err_db)?;
    set_tenant_context(&mut conn, a.env_id()).await;
    let row = sqlx::query(
        r#"UPDATE filing_runs SET status = 'draft', updated_at = now()
            WHERE id = $1 AND env_id = $2
              AND status NOT IN ('submitted', 'acknowledged')
            RETURNING id, obligation_id, period, status, artifact_ref"#,
    )
    .bind(id)
    .bind(a.env_id())
    .fetch_optional(&mut *conn)
    .await
    .map_err(crate::errors::err_db)?;
    let Some(row) = row else {
        return Err((
            StatusCode::CONFLICT,
            "run is already submitted - corrections are amendments".into(),
        ));
    };
    let run = row_to_run(&row);
    conn.commit().await.map_err(crate::errors::err_db)?;
    emit_event(
        &state,
        a.env_id(),
        "bank8:reporting:return:updated",
        serde_json::to_value(&run).unwrap_or_default(),
    )
    .await;
    Ok(Json(run))
}

// ── Submissions ───────────────────────────────────────────────────────────

/// `GET /v1/submissions` - list submissions for the tenant env.
#[utoipa::path(get, path = "/v1/submissions", responses((status = 200, body = Vec<SubmissionRow>)))]
pub async fn list_submissions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<SubmissionRow>>, (StatusCode, String)> {
    let a = auth::verify_token(&state.config, &headers).await?;
    crate::policy::require(
        &state.policy,
        &a,
        &headers,
        sentin8::shield::EvalTarget::new("bank8:reporting:submissions:read"),
    )
    .await?;
    let mut conn = state.pool.begin().await.map_err(crate::errors::err_db)?;
    set_tenant_context(&mut conn, a.env_id()).await;
    let rows = sqlx::query(
        r#"SELECT s.id, s.run_id, s.regulator_ref, s.acknowledgment, s.submitted_at, s.acknowledged_at,
                  r.period, r.status AS run_status
             FROM submissions s JOIN filing_runs r ON r.id = s.run_id
            WHERE s.env_id = $1
            ORDER BY s.created_at DESC LIMIT 200"#,
    )
    .bind(a.env_id())
    .fetch_all(&mut *conn).await.map_err(crate::errors::err_db)?;
    let out: Vec<SubmissionRow> = rows
        .iter()
        .map(|r| SubmissionRow {
            id: r.get::<Uuid, _>("id"),
            run_id: r.get::<Uuid, _>("run_id"),
            period: r.get::<String, _>("period"),
            run_status: r.get::<String, _>("run_status"),
            regulator_ref: r.get::<Option<String>, _>("regulator_ref"),
            acknowledgment: r.get::<Option<String>, _>("acknowledgment"),
            submitted_at: r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("submitted_at"),
            acknowledged_at: r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("acknowledged_at"),
        })
        .collect();
    Ok(Json(out))
}

// ── Calendar due-check (Cron proxy-back - RPT-CAL-1) ───────────────────

/// The fixed body envelope Cron posts on every fire (ticker.rs
/// execute_proxy_back) - the env arrives IN THE BODY (Cron is a
/// platform M2M whose own identity carries no tenant env). The unused
/// fields are part of the envelope contract and stay for documentation.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CronFireRequest {
    #[allow(dead_code)]
    pub schedule_id: Uuid,
    #[allow(dead_code)]
    pub schedule_type: String,
    pub env_id: Uuid,
    #[allow(dead_code)]
    pub correlation_id: Option<String>,
    pub idempotency_key: Option<String>,
}

/// `POST /v1/internal/calendar/schedules/run` - Cron fires this daily.
/// Marks due obligations and creates their filing runs - idempotent per
/// (obligation, period): the unique index is the dedupe floor, so a
/// re-fired calendar never double-creates (reporting DESIGN.md §5/§8).
/// Cron owns timing; this owns correctness. The `sentin8:cron` workload
/// is granted `bank8:reporting:runs:write` in this service's registration
/// (the ledger-storage precedent), so the same Shield gate every human
/// mutation passes also gates the scheduled fire - no bypass.
#[utoipa::path(
    post,
    path = "/v1/internal/calendar/schedules/run",
    request_body = CronFireRequest,
    responses(
        (status = 200, description = "Filing-calendar due check completed", body = CalendarDueCheckResponse),
    ),
    security(),
    tag = "internal",
)]
pub async fn calendar_due_check(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CronFireRequest>,
) -> Result<Json<CalendarDueCheckResponse>, (StatusCode, String)> {
    let a = auth::verify_token(&state.config, &headers).await?;
    crate::policy::require(
        &state.policy,
        &a,
        &headers,
        sentin8::shield::EvalTarget::new("bank8:reporting:runs:write"),
    )
    .await?;

    let mut tx = state.pool.begin().await.map_err(crate::errors::err_db)?;
    set_tenant_context(&mut tx, req.env_id).await;

    // Mark due obligations (due-date passed, still scheduled).
    let due = sqlx::query(
        r#"UPDATE filing_obligations SET status = 'due', updated_at = now()
            WHERE env_id = $1 AND status = 'scheduled' AND next_due <= now()
            RETURNING id, template_id, period"#,
    )
    .bind(req.env_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(crate::errors::err_db)?;

    let mut created = 0usize;
    for row in &due {
        let obligation_id: Uuid = row.get("id");
        let period: String = row.get("period");
        // Create the draft run - idempotent per (obligation, period);
        // the idempotency key stays stable across re-fires.
        let run_key = req
            .idempotency_key
            .clone()
            .unwrap_or_else(|| format!("run:{obligation_id}:{period}"));
        let res = sqlx::query(
            r#"INSERT INTO filing_runs (env_id, obligation_id, period, status, idempotency_key)
               VALUES ($1, $2, $3, 'draft', $4)
               ON CONFLICT (obligation_id, period) DO NOTHING"#,
        )
        .bind(req.env_id)
        .bind(obligation_id)
        .bind(&period)
        .bind(&run_key)
        .execute(&mut *tx)
        .await
        .map_err(crate::errors::err_db)?;
        created += res.rows_affected() as usize;
    }

    tx.commit().await.map_err(crate::errors::err_db)?;

    let payload = CalendarDueCheckResponse {
        obligations_due: due.len(),
        runs_created: created,
        env_id: req.env_id,
    };
    if created > 0 {
        emit_event(
            &state,
            req.env_id,
            "bank8:reporting:return:created",
            serde_json::to_value(&payload).unwrap_or_default(),
        )
        .await;
    }
    Ok(Json(payload))
}

// ── Flow proxy-back ───────────────────────────────────────────────────────

/// `POST /v1/internal/steps/execute` - Flow proxy-back for filing
/// workflow steps (DESIGN.md §4; the proxy-back contract is one of the
/// narrow internal-API exceptions - the steps are already
/// event-triggered, the proxy-back is their data access). The env rides
/// the step input (Flow records it at workflow creation); the dispatch
/// owns the guarded state transitions.
#[utoipa::path(
    post,
    path = "/v1/internal/steps/execute",
    request_body = ProxyBackRequest,
    responses(
        (status = 200, description = "Step executed", body = ProxyBackResponse),
    ),
    security(),
    tag = "internal",
)]
pub async fn execute_step(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProxyBackRequest>,
) -> Result<Json<ProxyBackResponse>, (StatusCode, String)> {
    let _a = auth::verify_token(&state.config, &headers).await?;
    let resp = crate::worker::execute_filing_step(&state.pool, &req).await;
    Ok(Json(resp))
}

// ── Typed proxy-back replies ──────────────────────────────────
/// The `POST /v1/internal/calendar/schedules/run` reply.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct CalendarDueCheckResponse {
    /// Obligations whose period closed in this pass.
    pub obligations_due: usize,
    /// Filing runs created.
    pub runs_created: usize,
    /// The fired env.
    pub env_id: uuid::Uuid,
}

/// One regulatory submission row (`GET /v1/submissions`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SubmissionRow {
    /// The submission row id.
    pub id: uuid::Uuid,
    /// The filing run that produced it.
    pub run_id: uuid::Uuid,
    /// The filing period.
    pub period: String,
    /// Submission status.
    pub run_status: String,
    /// The regulator's submission reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regulator_ref: Option<String>,
    /// The acknowledgment note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledgment: Option<String>,
    /// Sent instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Acknowledged instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<chrono::DateTime<chrono::Utc>>,
}
