// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! Sentin8:Bus env-init consumer - reacts to `sentin8:sys:service:updated`
//! when the `reporting` service is enabled for an org/env: materializes
//! that env's filing obligations from the jurisdiction registry
//! (platform-scoped rows provisioned by `vertical init`), idempotently -
//! the (env, template, period) unique index is the dedupe floor
//! (reporting DESIGN.md §3; JUR-REG-1/RPT-CAL-1).
//!
//! Uses the same SSE pattern as `bank8-fdx/src/event_consumer.rs`.

use std::time::Duration;

use futures_util::StreamExt;
use sqlx::PgPool;
use uuid::Uuid;

/// Consumer group name - identifies this reporting instance to Sentin8:Bus.
const CONSUMER_GROUP: &str = "bank8:reporting:envinit";

/// Reconnect delay when the Bus SSE connection drops or an error occurs.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Spawn the Bus consumer background task. Called once from
/// `server::build_state`.
pub fn spawn(pool: PgPool, bus_client: sentin8::bus::client::BusClient) {
    tokio::spawn(async move {
        run(pool, bus_client).await;
    });
}

/// Run the consumer loop: connect to Bus SSE for the `sys` stream family,
/// read messages, dispatch relevant events, ACK on success, reconnect on
/// disconnect or error.
async fn run(pool: PgPool, bus_client: sentin8::bus::client::BusClient) {
    tracing::info!("reporting: bus env-init consumer started (consumer_group={CONSUMER_GROUP})");

    // Startup backfill: re-derive obligations for envs already known
    // locally (obligation rows exist from a prior enablement) - the
    // platform re-emits nothing, so a redeployed consumer heals from
    // local state (idempotent).
    backfill_known_envs(&pool).await;

    loop {
        match bus_client.sse_consume("sys", CONSUMER_GROUP).await {
            Ok(resp) => {
                tracing::info!("reporting: bus SSE connected to stream family 'sys'");
                sse_loop(&pool, &bus_client, resp).await;
                tracing::info!("reporting: bus SSE disconnected - reconnecting");
            }
            Err(e) => {
                tracing::warn!(error = %e, "reporting: bus SSE consume failed - reconnecting");
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// Process messages from a single Bus SSE connection until it drops.
async fn sse_loop(
    pool: &PgPool,
    bus_client: &sentin8::bus::client::BusClient,
    resp: reqwest::Response,
) {
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk_result) = stream.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "reporting: bus SSE stream error");
                return;
            }
        };

        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(blank_pos) = buffer.find("\n\n") {
            let event_str = buffer[..blank_pos].to_string();
            buffer = buffer[blank_pos + 2..].to_string();

            let data_line = event_str
                .lines()
                .filter_map(|l| l.strip_prefix("data:").map(|s| s.trim_start()))
                .collect::<Vec<_>>()
                .join("");
            if data_line.is_empty() {
                continue;
            }

            let msg: sentin8::bus::contracts::Message = match serde_json::from_str(&data_line) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "reporting: failed to parse SSE message");
                    continue;
                }
            };

            handle_message(pool, &msg).await;

            if let Err(e) = bus_client.ack("sys", CONSUMER_GROUP, &msg.message_id).await {
                tracing::warn!(error = %e, message_id = %msg.message_id, "reporting: ack failed");
            }
        }
    }
}

/// Startup backfill - re-derive obligations for envs known locally.
async fn backfill_known_envs(pool: &PgPool) {
    let rows: Vec<(Uuid,)> =
        match sqlx::query_as(r#"SELECT DISTINCT env_id FROM filing_obligations"#)
            .fetch_all(pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "reporting: backfill query failed");
                return;
            }
        };

    tracing::info!(
        "reporting: backfill - {} known env(s) re-verified",
        rows.len()
    );

    for (env_id,) in rows {
        seed_obligations(pool, env_id).await;
    }
}

/// Dispatch an env-init event: `sentin8:sys:service:updated` with
/// `service == "reporting"` and `enabled == true` seeds that env's
/// obligations from the registry (idempotent).
async fn handle_message(pool: &PgPool, msg: &sentin8::bus::contracts::Message) {
    // Event names use ALL-COLON segments (AGENTS.md event-naming rule).
    if msg.event_type != "sentin8:sys:service:updated" {
        return;
    }

    let payload: &serde_json::Value = &msg.payload;
    let service = payload
        .get("service")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let enabled = payload
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if service != "reporting" || !enabled {
        return;
    }

    let env_id = match payload
        .get("env_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
    {
        Some(id) => id,
        None => return,
    };

    // VDC-2 consumer-side defense-in-depth: the event's first colon
    // segment must match the recorded producer vertical - verifies the
    // message Bus delivered before seeding tenant state.
    if let Err(reason) = sentin8::bus::client::BusClient::require_publisher(msg) {
        tracing::warn!(reason = %reason, "reporting: untrusted publisher - skipping env-init");
        return;
    }

    tracing::info!(%env_id, "reporting: seeding filing obligations (event-driven)");
    seed_obligations(pool, env_id).await;
}

/// Materialize the env's filing obligations from the jurisdiction
/// registry - one obligation per prudential return template for the
/// current period, next-due derived from the template cadence
/// (RPT-CAL-1). Idempotent: the (env, template, period) unique index is
/// the dedupe floor - a re-fired env-init never double-creates.
///
/// Registry tables are platform-scoped (no RLS); obligation rows are
/// FORCE-RLS'd, so the insert carries the env context on the SAME
/// transaction (the FDX-RLS1 lesson - never set-then-autocommit).
async fn seed_obligations(pool: &PgPool, env_id: Uuid) {
    let mut tx = match pool.begin().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, %env_id, "reporting: obligation-seed tx begin failed");
            return;
        }
    };
    let _ = sqlx::query("SELECT sentin8_set_tenant_context($1, '', 'm2m', 'sys:admin')")
        .bind(env_id)
        .execute(&mut *tx)
        .await;

    // One obligation per prudential template for the current period.
    // The period label derives from the cadence (quarter start, month
    // start, or today) - the calendar engine advances next_due per
    // cadence; the seed only plants the current period's row.
    let inserted = match sqlx::query(
        r#"
        INSERT INTO filing_obligations (env_id, template_id, period, next_due, grace_period, status)
        SELECT $1,
               t.id,
               CASE t.cadence
                   WHEN 'quarterly' THEN to_char(date_trunc('quarter', now()), 'YYYY-"Q"Q')
                   WHEN 'monthly'   THEN to_char(date_trunc('month', now()), 'YYYY-MM')
                   ELSE to_char(now(), 'YYYY-MM-DD')
               END,
               now() + interval '7 days',
               'P7D',
               'scheduled'
          FROM reporting.return_templates t
         WHERE t.definition->>'family' = 'capital'
        ON CONFLICT (env_id, template_id, period) DO NOTHING
        "#,
    )
    .bind(env_id)
    .execute(&mut *tx)
    .await
    {
        Ok(res) => res.rows_affected(),
        Err(e) => {
            tracing::warn!(error = %e, %env_id, "reporting: obligation seed insert failed");
            let _ = tx.rollback().await;
            return;
        }
    };
    let _ = tx.commit().await;
    tracing::info!(%env_id, inserted, "reporting: filing obligations ensured");
}
