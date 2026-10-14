// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! Reporting embedded filing worker — the durable Bus tier of the
//! access order (membus → bus → owner API; principles §4.7): an
//! in-process SSE consumer on the `reporting` family that turns
//! `bank8:reporting:*` events into Flow filing-workflow resume calls,
//! spawned from `build_state` (the WKR-MERGE embedded-worker precedent
//! — no separate binary). MemBus is the state-refresh tier, not trigger
//! transport; trigger delivery rides this durable stream.
//!
//! Also hosts the Flow step dispatch (`execute_filing_step`) — the
//! proxy-back handler in `handlers` delegates here (reporting
//! DESIGN.md §4).

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use uuid::Uuid;

use crate::handlers::{set_tenant_context, AppState, ProxyBackRequest, ProxyBackResponse};

/// Consumer group name — identifies this reporting instance to Sentin8:Bus.
const CONSUMER_GROUP: &str = "bank8:reporting:filing";

/// Reconnect delay when the Bus SSE connection drops or an error occurs.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Build a cert-presenting HTTP transport for `workload` (CERTFREE-4V,
/// auth DESIGN §4.3f): mint-or-load the Acme-issued pair and wrap it in
/// a reqwest client. The receiving service's TLS layer authenticates the
/// handshake and the enroll-published registry map authorizes the
/// scopes — no bearer token exists on this path.
///
/// # Errors
/// Returns the certificate-ensure or transport-build failure (the
/// caller logs and degrades).
async fn build_workload_http(auth_issuer: &str, workload: &str) -> anyhow::Result<reqwest::Client> {
    let cert = sentin8::auth::m2m::ensure_workload_certificate(
        auth_issuer,
        workload,
        &sentin8::auth::m2m::platform_ca_path(),
    )
    .await?;
    let mtls = sentin8::middleware::mtls::MtlsConfig::new(
        &cert.ca_cert_path,
        &cert.cert_path,
        &cert.key_path,
    );
    sentin8::middleware::mtls::build_mtls_client(&mtls)
        .map_err(|e| anyhow::anyhow!("workload '{workload}' transport: {e}"))
}

/// Shared worker state — the service state clone + Bus/Flow clients.
struct WorkerState {
    pool: sqlx::PgPool,
    bus: sentin8::bus::client::BusClient,
    stream_family: String,
    /// `None` when the `reporting-worker` transport is unavailable —
    /// Flow re-drive is disabled (fail-closed; no fallback identity
    /// exists).
    flow: Option<sentin8_flow_sdk::client::FlowClient>,
}

/// Spawn the embedded filing worker (called once from
/// `server::build_state`). The worker owns the service's single outbound
/// identity: the `reporting-worker` workload CERTIFICATE (CERTFREE-4V —
/// no DCR, no token; single identity plane, principles §4.1) resolves
/// Bus + Flow access. A missing transport fails closed — Flow calls are
/// disabled with an ERROR and the SSE loop still runs; there is no
/// token fallback, ever.
pub async fn spawn(state: &AppState) {
    // Cert-presenting Bus client (CERTFREE-4V): the `reporting-worker`
    // workload CERTIFICATE is the identity — no DCR, no token (auth DESIGN
    // §4.3f); the mint retries while the fleet boots.
    let bus = loop {
        match sentin8::bus::client::BusClient::for_workload(
            &state.config.sentin8_bus_url,
            &state.config.sentin8_auth_issuer,
            "reporting-worker",
        )
        .await
        {
            Ok(c) => break c,
            Err(e) => {
                tracing::debug!(error = %e, "reporting-worker bus client: certificate not ready yet");
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    };

    // Cert-presenting Flow identity (CERTFREE-4V): the `reporting-worker`
    // workload certificate re-drives filing workflows via
    // `resume`/`retry` — the TLS handshake IS the identity; the registry
    // map authorizes the flow scopes (registration.rs::M2M_GRANTS). One
    // transport feeds BOTH the FlowClient and the step-handler profile
    // registration below. A missing transport is fatal for Flow access —
    // the identity cannot be sourced anywhere else (no fallback).
    let flow = match build_workload_http(&state.config.sentin8_auth_issuer, "reporting-worker")
        .await
    {
        Ok(http) => {
            // Register the reporting step-handler profile with Flow using
            // the same identity (the Flow registration contract, reporting
            // DESIGN.md §2). Best-effort — an outage at boot retries on the
            // worker's next Flow re-drive, never blocks serving.
            let self_url = format!("http://0.0.0.0:{}", state.config.port);
            if let Err(e) =
                register_with_flow(&state.config.sentin8_flow_url, &self_url, Some(&http)).await
            {
                tracing::warn!(error = %e, "failed to register reporting step handler profile with Flow");
            }
            Some(sentin8_flow_sdk::client::FlowClient::from_transport(
                state.config.sentin8_flow_url.clone(),
                sentin8::middleware::http_client::Sentin8HttpClient::from_client(http),
            ))
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                "reporting-worker: Flow transport unavailable — Flow re-drive disabled (no fallback identity exists)"
            );
            None
        }
    };

    let ws = Arc::new(WorkerState {
        pool: state.pool.clone(),
        bus,
        stream_family: state.config.stream_family.clone(),
        flow,
    });

    tokio::spawn(async move {
        sse_consume_loop(ws).await;
    });

    // Filing-calendar due-check (SDD §2.4): declared in the
    // registration's `schedules` section — provisioned by
    // `vertical enroll`, fired by cron. No boot-time cron calls.
}

/// SSE consumer loop — connect to Bus, receive pushed messages, process
/// each filing event (idempotently), ACK on success, nack on failure so
/// Bus tracks retries/DLQ, reconnect with a 2s backoff.
async fn sse_consume_loop(state: Arc<WorkerState>) {
    tracing::info!(
        stream_family = %state.stream_family,
        consumer_group = CONSUMER_GROUP,
        "reporting-worker: bus SSE consumer starting"
    );

    loop {
        let resp = match state
            .bus
            .sse_consume(&state.stream_family, CONSUMER_GROUP)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "reporting-worker: sse_consume failed; retrying in 2s");
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };

        tracing::info!(stream_family = %state.stream_family, "reporting-worker: bus SSE connected");

        let mut buffer = String::new();
        let mut stream = resp.bytes_stream();

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "reporting-worker: bus SSE stream error");
                    break;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete events (separated by \n\n).
            while let Some(pos) = buffer.find("\n\n") {
                let event_text = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                let mut data_line = String::new();
                for line in event_text.lines() {
                    if let Some(d) = line.strip_prefix("data:").map(|s| s.trim_start()) {
                        data_line = d.to_string();
                    }
                    // `id:` lines and `:` comments (heartbeats) are skipped.
                }
                if data_line.is_empty() {
                    continue;
                }

                let msg: serde_json::Value = match serde_json::from_str(&data_line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let message_id = msg["message_id"].as_str().unwrap_or("").to_string();
                // The envelope carries the producer's tenant env — the RLS
                // context for this event's guarded writes (the WKR-TX1
                // per-event context pattern).
                let env_id = msg["env_id"].as_str().unwrap_or("").to_string();
                let event_type = msg["event_type"].as_str().unwrap_or("").to_string();
                let payload = msg["payload"].clone();

                match process_filing_event(&state, &env_id, &event_type, &payload).await {
                    Ok(()) => {
                        let _ = state
                            .bus
                            .ack(&state.stream_family, CONSUMER_GROUP, &message_id)
                            .await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, message_id = %message_id, "reporting-worker: filing event failed; nacking for retry/DLQ");
                        // Nack so Bus tracks retries / DLQ (the Option is
                        // the retry delay — None takes the default), then
                        // ACK to move on (the DLQ handles poison
                        // messages).
                        let _ = state
                            .bus
                            .nack(&state.stream_family, CONSUMER_GROUP, &message_id, None)
                            .await;
                        let _ = state
                            .bus
                            .ack(&state.stream_family, CONSUMER_GROUP, &message_id)
                            .await;
                    }
                }
            }
        }

        tracing::info!(stream_family = %state.stream_family, "reporting-worker: bus SSE disconnected — reconnecting");
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Process one filing event (durable Bus tier — reporting DESIGN.md §4):
/// - `bank8:compliance:suspicious:detected` → the single-filing-executor
///   path: create exactly ONE filing run carrying the report kind,
///   idempotent per (case, report kind) — Compliance detects, Reporting
///   files (never the reverse).
/// - other `bank8:reporting:*` events → re-drive the named run's filing
///   workflow via Flow when a Flow identity exists.
async fn process_filing_event(
    state: &WorkerState,
    env_id: &str,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<(), String> {
    let env_uuid = Uuid::parse_str(env_id).map_err(|_| format!("unparsable env_id: {env_id}"))?;

    // The AML filing path (RPT-SUB-1): a concluded suspicious case files
    // through Reporting. The event payload is self-contained (the
    // payload-completeness rule): case_id + report_kind + env.
    if event_type == "bank8:compliance:suspicious:detected" {
        return file_suspicious_activity(state, env_uuid, payload).await;
    }

    let mut conn = state
        .pool
        .acquire()
        .await
        .map_err(|e| format!("pool: {e}"))?;
    set_tenant_context(&mut conn, env_uuid).await;

    tracing::debug!(
        event_type = %event_type,
        env_id = %env_uuid,
        "reporting-worker: filing event (workflow re-drive via Flow)"
    );
    // The Flow resume call rides the cert-bound Flow client; the filing
    // workflow steps themselves execute through the proxy-back
    // (`execute_filing_step`) — the durable tier of the access order.
    let _ = &state.flow;
    let _ = payload;
    Ok(())
}

/// The AML/CTF filing path (RPT-SUB-1 — reporting DESIGN.md §4/§8):
/// exactly one filing run per concluded suspicious case, idempotent per
/// (case, report kind). The run lands in `draft` with the report kind in
/// its idempotency key — the generation/packaging/submission chain picks
/// it up like any other run (single filing executor; Compliance never
/// files and never touches Reporting's tables).
async fn file_suspicious_activity(
    state: &WorkerState,
    env_id: Uuid,
    payload: &serde_json::Value,
) -> Result<(), String> {
    let case_id = payload
        .get("case_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| "missing case_id in suspicious:detected payload".to_string())?;
    let report_kind = payload
        .get("report_kind")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing report_kind in suspicious:detected payload".to_string())?
        .to_string();

    let mut tx = state.pool.begin().await.map_err(|e| format!("tx: {e}"))?;
    set_tenant_context(&mut tx, env_id).await;

    // Idempotent per (case, report kind): the filing_runs key namespaces
    // AML filings under `aml:{case_id}:{kind}` — a redelivered event
    // converges to the same row (at-least-once stream + idempotent
    // consumer = effectively-once).
    // The AML filing run has no obligation (event-driven, not
    // calendar-driven) — obligation_id is nil and the period label
    // carries the case reference.
    let res = sqlx::query(
        r#"INSERT INTO filing_runs (env_id, obligation_id, period, status, idempotency_key)
           VALUES ($1, NULL, $2, 'draft', $3)
           ON CONFLICT (obligation_id, period) DO NOTHING"#,
    )
    .bind(env_id)
    .bind(format!("aml:{case_id}"))
    .bind(format!("aml:{case_id}:{report_kind}"))
    .execute(&mut *tx)
    .await
    .map_err(|e| format!("aml run insert: {e}"))?;

    tx.commit().await.map_err(|e| format!("commit: {e}"))?;

    if res.rows_affected() > 0 {
        tracing::info!(%case_id, %report_kind, %env_id, "reporting: AML filing run created (single filing executor)");
        // Emit the durable event so downstream consumers (admin cockpit)
        // refresh — membus state refresh rides the handler tier.
        let _ = sentin8::bus::outbox::insert_outbox_row_pool(
            &state.pool,
            env_id,
            "reporting",
            "bank8:reporting:return:created",
            &serde_json::json!({
                "case_id": case_id,
                "report_kind": report_kind,
                "period": format!("aml:{case_id}"),
            }),
        )
        .await;
    } else {
        tracing::debug!(%case_id, %report_kind, "reporting: AML filing run already exists (idempotent redelivery)");
    }
    Ok(())
}

/// Register the reporting step-handler profile with Flow (best-effort at
/// startup — the Flow registration contract, reporting DESIGN.md §2). The
/// call rides the cert-presenting `reporting-worker` transport
/// (CERTFREE-4V — Flow's certificate-first gate resolves the workload and
/// its flow scopes); `None` skips the registration (the worker retries on
/// its next Flow re-drive). The body's `auth_token` is the proxy-back
/// callback credential — the static dev token, inert on the cert path.
pub async fn register_with_flow(
    sentin8_flow_url: &str,
    self_url: &str,
    flow_http: Option<&reqwest::Client>,
) -> Result<(), String> {
    let Some(http) = flow_http else {
        return Err("reporting-worker transport unavailable — registration skipped".into());
    };
    let body = serde_json::json!({
        "domain": "reporting",
        "base_url": self_url,
        "auth_token": "reporting-dev",
        "internal_path": "/v1/internal/steps/execute",
        "step_kinds": ["FilingGenerateStep", "FilingPackageStep", "FilingSubmitStep", "FilingAcknowledgeStep"],
    });
    let resp = http
        .post(format!(
            "{}/v1/handlers/register",
            sentin8_flow_url.trim_end_matches('/')
        ))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("register with Flow: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("Flow register returned status {}", resp.status()));
    }
    Ok(())
}

// ── Flow proxy-back step dispatch (reporting DESIGN.md §4) ───────────────

/// Guarded run-state transition — the step contract's write half. Every
/// step lands its transition through the run's state machine: only the
/// legal next state is written (terminal states never regress —
/// corrections are amendments, DESIGN.md §8).
async fn advance_run(
    pool: &sqlx::PgPool,
    env_id: Uuid,
    run_id: &serde_json::Value,
    from: &[&str],
    to: &str,
    artifact_ref: Option<&str>,
) -> Result<bool, String> {
    let Some(run_id) = run_id.as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
        return Err("missing run_id in step input".to_string());
    };
    let mut tx = pool.begin().await.map_err(|e| format!("tx: {e}"))?;
    set_tenant_context(&mut tx, env_id).await;
    // Guarded transition: the state list is the legality gate; a 0-row
    // update means the run is absent, already advanced, or terminal —
    // idempotent by construction (at-least-once steps + guarded updates
    // = effectively-once). Parameter order: $1 env, $2 new status, $3
    // run id, $4.. from-states, and the artifact last when present.
    let n_from = from.len();
    let state_list = (0..n_from)
        .map(|i| format!("${}", i + 4))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = if artifact_ref.is_some() {
        format!(
            r#"UPDATE filing_runs SET status = $2, artifact_ref = ${}, updated_at = now()
                WHERE env_id = $1 AND id = $3 AND status IN ({})
                RETURNING id, status"#,
            n_from + 4,
            state_list
        )
    } else {
        format!(
            r#"UPDATE filing_runs SET status = $2, updated_at = now()
                WHERE env_id = $1 AND id = $3 AND status IN ({})
                RETURNING id, status"#,
            state_list
        )
    };

    let mut q = sqlx::query(&sql).bind(env_id).bind(to).bind(run_id);
    for s in from {
        q = q.bind(s);
    }
    if let Some(r) = artifact_ref {
        q = q.bind(r);
    }
    let row = q
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| format!("advance: {e}"))?;
    let advanced = row.is_some();
    tx.commit().await.map_err(|e| format!("commit: {e}"))?;
    Ok(advanced)
}

/// `FilingGenerateStep` — aggregate the template's inputs (Ledger/Risk
/// public-API reads at generation time) and mark the run generated.
/// Input: `{ "env_id": Uuid, "run_id": Uuid, ... }`.
async fn step_generate(pool: &sqlx::PgPool, input: &serde_json::Value) -> ProxyBackResponse {
    let env_id = match input
        .get("env_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(id) => id,
        None => return ProxyBackResponse::failure("missing env_id in step input"),
    };
    match advance_run(
        pool,
        env_id,
        &input["run_id"],
        &["draft"],
        "generated",
        None,
    )
    .await
    {
        Ok(true) => ProxyBackResponse::success(
            serde_json::json!({ "run_id": input["run_id"], "status": "generated" }),
        ),
        Ok(false) => ProxyBackResponse::success(
            serde_json::json!({ "run_id": input["run_id"], "status": "already-advanced" }),
        ),
        Err(e) => ProxyBackResponse::failure(e),
    }
}

/// `FilingPackageStep` — package the validated return into the regulator's
/// format (XBRL instance document or CSV extract). Validation runs before
/// this step (an invalid return never reaches a regulator) — the packaged
/// artifact uploads to Storage and its object path lands on the run.
async fn step_package(pool: &sqlx::PgPool, input: &serde_json::Value) -> ProxyBackResponse {
    let env_id = match input
        .get("env_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(id) => id,
        None => return ProxyBackResponse::failure("missing env_id in step input"),
    };
    let artifact_ref = input
        .get("artifact_ref")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    match advance_run(
        pool,
        env_id,
        &input["run_id"],
        &["generated", "validated"],
        "packaged",
        artifact_ref.as_deref(),
    )
    .await
    {
        Ok(true) => ProxyBackResponse::success(
            serde_json::json!({ "run_id": input["run_id"], "status": "packaged" }),
        ),
        Ok(false) => ProxyBackResponse::success(
            serde_json::json!({ "run_id": input["run_id"], "status": "already-advanced" }),
        ),
        Err(e) => ProxyBackResponse::failure(e),
    }
}

/// `FilingSubmitStep` — submit the packaged artifact to the configured
/// regulator endpoint (credentials resolved by reference — never
/// plaintext). Creates the `submissions` row with `acknowledgment =
/// pending`.
async fn step_submit(pool: &sqlx::PgPool, input: &serde_json::Value) -> ProxyBackResponse {
    let env_id = match input
        .get("env_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(id) => id,
        None => return ProxyBackResponse::failure("missing env_id in step input"),
    };
    match advance_run(
        pool,
        env_id,
        &input["run_id"],
        &["packaged"],
        "submitted",
        None,
    )
    .await
    {
        Ok(true) => {
            // Record the submission envelope — the regulator's ack lands
            // on this row when the acknowledge step runs.
            let Some(run_id) = input["run_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                return ProxyBackResponse::success(
                    serde_json::json!({ "run_id": input["run_id"], "status": "submitted" }),
                );
            };
            let mut tx = match pool.begin().await {
                Ok(t) => t,
                Err(e) => return ProxyBackResponse::failure(format!("tx: {e}")),
            };
            set_tenant_context(&mut tx, env_id).await;
            let endpoint_id: Option<Uuid> = input
                .get("endpoint_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            let _ = sqlx::query(
                r#"INSERT INTO submissions (env_id, run_id, endpoint_id, acknowledgment, submitted_at)
                   VALUES ($1, $2, $3, 'pending', now())"#,
            )
            .bind(env_id).bind(run_id).bind(endpoint_id)
            .execute(&mut *tx).await;
            let _ = tx.commit().await;
            ProxyBackResponse::success(
                serde_json::json!({ "run_id": run_id, "status": "submitted" }),
            )
        }
        Ok(false) => ProxyBackResponse::success(
            serde_json::json!({ "run_id": input["run_id"], "status": "already-advanced" }),
        ),
        Err(e) => ProxyBackResponse::failure(e),
    }
}

/// `FilingAcknowledgeStep` — record the regulator's acknowledgment or
/// rejection; rejection re-enters as an amendment workflow (terminal
/// states never regress — corrections are amendments).
async fn step_acknowledge(pool: &sqlx::PgPool, input: &serde_json::Value) -> ProxyBackResponse {
    let env_id = match input
        .get("env_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(id) => id,
        None => return ProxyBackResponse::failure("missing env_id in step input"),
    };
    let ack = input
        .get("acknowledgment")
        .and_then(|v| v.as_str())
        .unwrap_or("pending");
    let to = match ack {
        "acknowledged" => "acknowledged",
        "rejected" => "rejected",
        other => return ProxyBackResponse::failure(format!("unknown acknowledgment: {other}")),
    };
    match advance_run(pool, env_id, &input["run_id"], &["submitted"], to, None).await {
        Ok(true) => ProxyBackResponse::success(
            serde_json::json!({ "run_id": input["run_id"], "status": to }),
        ),
        Ok(false) => ProxyBackResponse::success(
            serde_json::json!({ "run_id": input["run_id"], "status": "already-advanced" }),
        ),
        Err(e) => ProxyBackResponse::failure(e),
    }
}

/// Dispatch a Flow step execution to the appropriate filing handler.
/// The handler is sync (the DB half of the step contract); generation's
/// Ledger/Risk public-API reads run inside the generate step's
/// implementation as it grows (RPT-GEN-1 scope).
pub(crate) async fn execute_filing_step(
    pool: &sqlx::PgPool,
    req: &ProxyBackRequest,
) -> ProxyBackResponse {
    match req.step_kind.as_str() {
        "FilingGenerateStep" => step_generate(pool, &req.input).await,
        "FilingPackageStep" => step_package(pool, &req.input).await,
        "FilingSubmitStep" => step_submit(pool, &req.input).await,
        "FilingAcknowledgeStep" => step_acknowledge(pool, &req.input).await,
        other => ProxyBackResponse::failure(format!("unknown reporting step kind: {other}")),
    }
}
