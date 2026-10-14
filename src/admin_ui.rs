// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! Bank8:Reporting Admin UI handlers — RFW (Remote Flutter Widget)
//! definitions for the filing cockpit. Icons are Material Icons code
//! points (hex strings), per principles §4.15.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use sentin8::rfw::page::*;
use sentin8::rfw::response::AdminUiResponse;
use sentin8::rfw::widget::*;
use sqlx::Row;

use crate::auth;
use crate::handlers::{set_tenant_context, AppState};

/// GET /v1/admin/ui/{page} — return the RFW definition for the requested
/// admin page.
pub async fn get_admin_ui(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(page): Path<String>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    let _a = auth::verify_token(&state.config, &headers).await?;
    crate::policy::require(
        &state.policy,
        &_a,
        &headers,
        sentin8::shield::EvalTarget::new("bank8:reporting:runs:read"),
    )
    .await?;

    let page_def = match page.as_str() {
        "overview" => overview_page(),
        "calendar" => calendar_page(),
        "runs" => runs_page(),
        // ── Policy Studio (design §3.7) — shared page constructors; the
        // guard guarantees `Some` (§3.7 dispatch).
        p if sentin8::rfw::policy_page("bank8:reporting", p).is_some() => {
            sentin8::rfw::policy_page("bank8:reporting", p)
                .expect("policy_page checked in guard")
                .build()
        }
        _ => {
            return Err((
                axum::http::StatusCode::NOT_FOUND,
                format!("Unknown admin UI page: {page}"),
            ))
        }
    };

    Ok(Json(
        serde_json::to_value(AdminUiResponse::page(page_def)).unwrap(),
    ))
}

fn overview_page() -> AdminUiPage {
    AdminPage::new("Reporting Overview")
        .widget(RemoteWidget::Card(WidgetCard {
            title: Some("Filing Cockpit".into()),
            subtitle: None,
            children: vec![
                RemoteWidget::Text(WidgetText { text: "Filing calendar, obligation status, run lifecycle, and submission history — the vertical's single filing executor.".into(), text_type: WidgetTextType::Body, align: TextAlign::Left, max_lines: None }),
            ],
            actions: vec![],
        }))
        .build()
}

fn calendar_page() -> AdminUiPage {
    AdminPage::new("Filing Calendar")
        .widget(RemoteWidget::Card(WidgetCard {
            title: Some("Obligations".into()),
            subtitle: None,
            children: vec![
                RemoteWidget::Text(WidgetText { text: "Due returns per regulator cadence; firing a due obligation is idempotent per (return, period) — a re-fired calendar can never double-submit.".into(), text_type: WidgetTextType::Body, align: TextAlign::Left, max_lines: None }),
            ],
            actions: vec![],
        }))
        .build()
}

fn runs_page() -> AdminUiPage {
    AdminPage::new("Filing Runs")
        .widget(RemoteWidget::Card(WidgetCard {
            title: Some("Run Lifecycle".into()),
            subtitle: None,
            children: vec![
                RemoteWidget::Text(WidgetText { text: "draft → generated → validated → packaged → submitted → acknowledged/rejected, with amended corrections — terminal states never regress.".into(), text_type: WidgetTextType::Body, align: TextAlign::Left, max_lines: None }),
            ],
            actions: vec![],
        }))
        .build()
}

/// GET /v1/admin/overview — the data endpoint behind the overview page:
/// obligation/run/submission counts for the tenant env.
pub async fn admin_overview(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
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

    let obligations = sqlx::query("SELECT count(*) AS n FROM filing_obligations WHERE env_id = $1")
        .bind(a.env_id())
        .fetch_one(&mut *conn)
        .await
        .map_err(crate::errors::err_db)?
        .get::<i64, _>("n");
    let runs_open = sqlx::query(
        "SELECT count(*) AS n FROM filing_runs WHERE env_id = $1 AND status NOT IN ('acknowledged', 'rejected')",
    )
    .bind(a.env_id()).fetch_one(&mut *conn).await.map_err(crate::errors::err_db)?
    .get::<i64, _>("n");
    let submitted = sqlx::query(
        "SELECT count(*) AS n FROM submissions WHERE env_id = $1 AND acknowledgment = 'acknowledged'",
    )
    .bind(a.env_id()).fetch_one(&mut *conn).await.map_err(crate::errors::err_db)?
    .get::<i64, _>("n");

    Ok(Json(serde_json::json!({
        "obligations": obligations,
        "runs_open": runs_open,
        "submissions_acknowledged": submitted,
        "service": "bank8:reporting",
    })))
}
