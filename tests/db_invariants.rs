// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! DB-backed schema-invariant tests (runtime-skip) — reporting DESIGN.md
//! §9. Exercises the schema's load-bearing invariants against a live
//! PostgreSQL using the SQL the handlers run: FORCE-RLS rejects
//! context-less inserts; the (obligation, period) unique index makes
//! calendar/idempotency dedupe real; the guarded state machine transitions
//! only legal states. Skipped when `DATABASE_URL` is unreachable —
//! matching the platform's TEST-SYS-3 pattern (ledger
//! `tests/db_handlers.rs` is the template).

use sqlx::PgPool;
use uuid::Uuid;

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("BANK8_REPORTING_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| "postgres://bank8_reporting_service@localhost:5432/bank8".into());
    match sqlx::PgPool::connect(&url).await {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("skipping DB-backed reporting tests — no database: {e}");
            None
        }
    }
}

async fn seed_template(pool: &PgPool) -> Option<(Uuid, Uuid)> {
    // Seed a jurisdiction + regulator + return template (registry data,
    // platform-scoped — no tenant context needed) and return
    // (jurisdiction id, template id).
    let jid = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO reporting.jurisdictions (country_code, metadata)
           VALUES ('XX', '{}') ON CONFLICT (country_code) DO UPDATE SET metadata = '{}'
           RETURNING id"#,
    )
    .fetch_one(pool)
    .await
    .ok()?;
    let rid = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO reporting.regulators (jurisdiction_id, name, authority_kind)
           VALUES ($1, 'test-prudential-authority', 'prudential')
           ON CONFLICT (jurisdiction_id, name) DO UPDATE SET name = EXCLUDED.name
           RETURNING id"#,
    )
    .bind(jid)
    .fetch_one(pool)
    .await
    .ok()?;
    let tid = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO reporting.return_templates (regulator_id, return_code, output_format, cadence, definition)
           VALUES ($1, 'test-return', 'XBRL', 'quarterly', '{"family": "capital"}')
           ON CONFLICT (regulator_id, return_code) DO UPDATE SET definition = EXCLUDED.definition
           RETURNING id"#,
    )
    .bind(rid).fetch_one(pool).await.ok()?;
    Some((jid, tid))
}

/// FORCE-RLS: a tenant-table insert WITHOUT an initialized context is
/// rejected — the RLS backstop is live on filing_obligations.
#[tokio::test]
async fn obligation_insert_without_context_rejected() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let Some((_jid, tid)) = seed_template(&pool).await else {
        return;
    };
    let env_id = Uuid::new_v4();
    let res = sqlx::query(
        r#"INSERT INTO filing_obligations (env_id, template_id, period, next_due)
           VALUES ($1, $2, 'NOCTX', now())"#,
    )
    .bind(env_id)
    .bind(tid)
    .execute(&pool)
    .await;
    assert!(res.is_err(), "context-less insert must violate FORCE RLS");
}

/// The idempotency invariant: the (obligation, period) unique index makes
/// a re-fired calendar converge — the second INSERT with the same key
/// is a no-op under ON CONFLICT.
#[tokio::test]
async fn obligation_seed_is_idempotent() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let Some((_jid, tid)) = seed_template(&pool).await else {
        return;
    };
    let env_id = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("tx");
    let _ = sqlx::query("SELECT sentin8_set_tenant_context($1, '', 'm2m', 'sys:admin')")
        .bind(env_id)
        .execute(&mut *tx)
        .await;

    let sql = r#"INSERT INTO filing_obligations (env_id, template_id, period, next_due, status)
                 VALUES ($1, $2, '2030Q1', now() + interval '30 days', 'scheduled')
                 ON CONFLICT (env_id, template_id, period) DO NOTHING"#;
    let first = sqlx::query(sql)
        .bind(env_id)
        .bind(tid)
        .execute(&mut *tx)
        .await
        .expect("first insert");
    let second = sqlx::query(sql)
        .bind(env_id)
        .bind(tid)
        .execute(&mut *tx)
        .await
        .expect("second insert");
    tx.commit().await.expect("commit");
    assert_eq!(first.rows_affected(), 1);
    assert_eq!(
        second.rows_affected(),
        0,
        "re-fired calendar must NOT double-create"
    );
}

/// The guarded state machine: `draft → generated` via a guarded UPDATE
/// advances exactly once; replaying it (at-least-once step delivery) is a
/// 0-row no-op (the at-most-once / effectively-once invariant §8).
#[tokio::test]
async fn guarded_step_transition_is_effectively_once() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let Some((_jid, tid)) = seed_template(&pool).await else {
        return;
    };
    let env_id = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("tx");
    let _ = sqlx::query("SELECT sentin8_set_tenant_context($1, '', 'm2m', 'sys:admin')")
        .bind(env_id)
        .execute(&mut *tx)
        .await;
    let oid = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO filing_obligations (env_id, template_id, period, next_due, status)
           VALUES ($1, $2, '2030Q2', now() + interval '30 days', 'due') RETURNING id"#,
    )
    .bind(env_id)
    .bind(tid)
    .fetch_one(&mut *tx)
    .await
    .expect("obligation");
    let run_id = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO filing_runs (env_id, obligation_id, period, status, idempotency_key)
           VALUES ($1, $2, '2030Q2', 'draft', 'gk-1') RETURNING id"#,
    )
    .bind(env_id)
    .bind(oid)
    .fetch_one(&mut *tx)
    .await
    .expect("run");

    // Guarded transition (the advance_run shape: $1 env, $2 new status,
    // $3 run id, $4.. from-states).
    let sql = r#"UPDATE filing_runs SET status = $2, updated_at = now()
                 WHERE env_id = $1 AND id = $3 AND status IN ($4)
                 RETURNING id, status"#;
    let first = sqlx::query(sql)
        .bind(env_id)
        .bind("generated")
        .bind(run_id)
        .bind("draft")
        .fetch_optional(&mut *tx)
        .await
        .expect("first advance");
    let replay = sqlx::query(sql)
        .bind(env_id)
        .bind("generated")
        .bind(run_id)
        .bind("draft")
        .fetch_optional(&mut *tx)
        .await
        .expect("replay");
    tx.commit().await.expect("commit");
    assert!(first.is_some(), "the legal transition advances once");
    assert!(
        replay.is_none(),
        "the replay is a 0-row no-op (effectively-once)"
    );
}

/// Terminal states never regress: submitted → acknowledged holds; no
/// update from an acknowledged run back to draft (corrections are
/// amendments, never mutations — §8).
#[tokio::test]
async fn terminal_states_never_regress() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let Some((_jid, tid)) = seed_template(&pool).await else {
        return;
    };
    let env_id = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("tx");
    let _ = sqlx::query("SELECT sentin8_set_tenant_context($1, '', 'm2m', 'sys:admin')")
        .bind(env_id)
        .execute(&mut *tx)
        .await;
    let oid = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO filing_obligations (env_id, template_id, period, next_due, status)
           VALUES ($1, $2, '2030Q3', now() + interval '30 days', 'due') RETURNING id"#,
    )
    .bind(env_id)
    .bind(tid)
    .fetch_one(&mut *tx)
    .await
    .expect("obligation");
    let run_id = sqlx::query_scalar::<_, Uuid>(
        r#"INSERT INTO filing_runs (env_id, obligation_id, period, status, idempotency_key)
           VALUES ($1, $2, '2030Q3', 'acknowledged', 'gk-2') RETURNING id"#,
    )
    .bind(env_id)
    .bind(oid)
    .fetch_one(&mut *tx)
    .await
    .expect("run");

    // The regenerate guard: acknowledged runs are excluded from any
    // draft-reset.
    let blocked = sqlx::query(
        r#"UPDATE filing_runs SET status = 'draft'
            WHERE env_id = $1 AND id = $2 AND status NOT IN ('submitted', 'acknowledged')
            RETURNING id"#,
    )
    .bind(env_id)
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .expect("regenerate guard");
    tx.commit().await.expect("commit");
    assert_eq!(
        blocked.rows_affected(),
        0,
        "an acknowledged run never regresses to draft"
    );
}

/// The AML filing path's idempotency: an (obligation NULL, period
/// `aml:{case}`) run converges on redelivery — one filing run per
/// (case, report kind).
#[tokio::test]
async fn aml_filing_run_is_idempotent_per_case() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let env_id = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("tx");
    let _ = sqlx::query("SELECT sentin8_set_tenant_context($1, '', 'm2m', 'sys:admin')")
        .bind(env_id)
        .execute(&mut *tx)
        .await;

    let case_key = "aml:00000000-0000-0000-0000-0000000000aa";
    let sql = r#"INSERT INTO filing_runs (env_id, obligation_id, period, status, idempotency_key)
                 VALUES ($1, NULL, $2, 'draft', $3)
                 ON CONFLICT (obligation_id, period) DO NOTHING"#;
    let first = sqlx::query(sql)
        .bind(env_id)
        .bind(case_key)
        .bind("aml:case-sar")
        .execute(&mut *tx)
        .await
        .expect("first");
    let second = sqlx::query(sql)
        .bind(env_id)
        .bind(case_key)
        .bind("aml:case-sar")
        .execute(&mut *tx)
        .await
        .expect("second");
    tx.commit().await.expect("commit");
    assert_eq!(first.rows_affected(), 1);
    assert_eq!(
        second.rows_affected(),
        0,
        "redelivered suspicious:detected must NOT double-file"
    );
}
