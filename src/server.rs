// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! Axum server setup for Reporting — shell boot composition.
//! See `services/reporting/DESIGN.md` §2.

use std::sync::Arc;
use axum::routing::{get, post};
use crate::handlers::{self, AppState};

pub async fn build_state(core: sentin8::shell::ServiceCore) -> anyhow::Result<AppState> {
    let config = Arc::new(crate::config::ReportingConfig::from_env());

    // ── Sentin8:Policy (§4.2b — shell-owned boot): consume the engine the
    // shell booted in step 8 — no second engine, consumer, or DCR
    // registration here. Step 5 is the ONLY gate.
    let policy_engine = core.admin.policy.clone();

    // ── MemBus (principles §4.7 — membus → bus → API): the newest-wins
    // state-refresh tier. Fail-soft by contract — an unreachable Valkey
    // disables refresh with a WARN, never a startup error.
    let membus = sentin8::membus::ServiceBus::connect(
        "bank8:reporting",
        &core.config.valkey_url,
    ).await;

    let state = AppState {
        config,
        pool: core.pool.clone(),
        policy: policy_engine.clone(),
        admin: sentin8::shield::admin::SharedAdminState {
            service: "bank8:reporting".to_string(),
            pool: core.pool.clone(),
            policy: policy_engine,
            auth_client: Arc::new(core.config.sentin8_auth_client.clone()),
        },
        membus,
    };

    // Spawn the embedded filing worker: consumes the `reporting` family
    // and turns filing events into Flow resume calls (the WKR-MERGE
    // embedded-worker precedent — no separate binary). The worker owns
    // the service's single outbound identity: the `reporting-worker`
    // workload CERTIFICATE (CERTFREE-4V — no DCR, no token; single
    // identity plane, principles §4.1) resolves Bus + Flow access — a
    // missing transport fails closed, no token fallback. The worker also
    // registers the Flow step-handler profile with the same identity.
    crate::worker::spawn(&state).await;

    // Spawn the env-init consumer: `sentin8:sys:service:updated` (sys
    // family) seeds the env's filing obligations from the jurisdiction
    // registry, idempotently (the fdx event-consumer pattern).
    // Cert-presenting Bus client (CERTFREE-4V): the `reporting-envinit`
    // workload CERTIFICATE is the identity; the mint retries while the
    // fleet boots.
    let envinit_bus = loop {
        match sentin8::bus::client::BusClient::for_workload(
            &state.config.sentin8_bus_url,
            &state.config.sentin8_auth_issuer,
            "reporting-envinit",
        )
        .await
        {
            Ok(c) => break c,
            Err(e) => {
                tracing::debug!(error = %e, "reporting-envinit bus client: certificate not ready yet");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };
    crate::event_consumer::spawn(core.pool.clone(), envinit_bus);

    Ok(state)
}

/// Add this service's routes onto the shell-provided router. The shell
/// owns the boot sequence, state binding, OpenAPI merge and the standard
/// middleware stack; this adds ONLY routes (including the service's own
/// `/v1/health`).
pub fn routes(router: axum::Router<AppState>, state: AppState) -> axum::Router<AppState> {
    router
        .route("/v1/health", get(handlers::health))
        // Filing obligations + runs + submissions (DESIGN.md §4)
        .route("/v1/obligations", get(handlers::list_obligations))
        .route("/v1/runs", get(handlers::list_runs))
        .route("/v1/runs/{id}", get(handlers::get_run))
        .route("/v1/runs/{id}/regenerate", post(handlers::regenerate_run))
        .route("/v1/submissions", get(handlers::list_submissions))
        // Flow proxy-back (DESIGN.md §4 — one of the narrow internal-API
        // exceptions: the steps are already event-triggered).
        .route("/v1/internal/steps/execute", post(handlers::execute_step))
        // Calendar due-check (Cron proxy-back — RPT-CAL-1): Cron posts to
        // {internalPath}/schedules/run with its fixed body envelope.
        .route("/v1/internal/calendar/schedules/run", post(handlers::calendar_due_check))
        // ── Admin UI (RFW) + policy admin (§3.6 — shared implementation) ──
        .route("/v1/admin/ui/{*page}", get(crate::admin_ui::get_admin_ui))
        .route("/v1/admin/overview", get(crate::admin_ui::admin_overview))
        .route("/v1/admin/policy-sets",
            get(sentin8::shield::admin::list_policy_sets::<AppState>))
        .route("/v1/admin/policy-sets/{name}",
            get(sentin8::shield::admin::get_policy_set::<AppState>)
                .put(sentin8::shield::admin::put_policy_set::<AppState>)
                .delete(sentin8::shield::admin::delete_policy_set::<AppState>))
        .route("/v1/admin/policy-sets/{name}/test",
            post(sentin8::shield::admin::test_policy_set::<AppState>))
        .route("/v1/admin/policy-sets/enforce",
            post(sentin8::shield::admin::admin_enforce_policy_set::<AppState>))
        .route("/v1/admin/policy-sets/shadow",
            post(sentin8::shield::admin::admin_shadow_policy_set::<AppState>))
        .route("/v1/admin/policy-sets/disable",
            post(sentin8::shield::admin::admin_disable_policy_set::<AppState>))
        .route("/v1/admin/relations",
            get(sentin8::shield::admin::list_relations::<AppState>)
                .put(sentin8::shield::admin::put_relation::<AppState>)
                .delete(sentin8::shield::admin::delete_relation::<AppState>))
        .route("/v1/admin/relations/check",
            get(sentin8::shield::admin::check_relation::<AppState>))
        .route("/v1/admin/attributes",
            get(sentin8::shield::admin::list_attributes::<AppState>)
                .put(sentin8::shield::admin::put_attribute::<AppState>)
                .delete(sentin8::shield::admin::delete_attribute::<AppState>))
        .with_state(state)
}

/// Shared policy-admin surface accessor (§3.6) — the shared handlers
/// extract this service's admin state via [`HasPolicyAdmin`].
impl sentin8::shield::admin::HasPolicyAdmin for AppState {
    fn policy_admin(&self) -> &sentin8::shield::admin::SharedAdminState {
        &self.admin
    }
}