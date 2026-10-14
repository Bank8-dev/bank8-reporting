// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! Reporting error responses — standard error format with error codes
//! and HTTP status mappings, integrated with Axum via `into_response`.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

/// Standard reporting error response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportingErrorResponse {
    /// HTTP status code.
    pub status_code: u16,
    /// Machine-readable error code.
    pub error_code: String,
    /// Human-readable error message.
    pub message: String,
    /// Optional additional details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ReportingErrorResponse {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { status_code: status, error_code: code.into(), message: message.into(), details: None }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self::new(422, "reporting_validation_failed", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "reporting_not_found", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, "reporting_already_submitted", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "reporting_internal", message)
    }
}

impl IntoResponse for ReportingErrorResponse {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status_code)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self)).into_response()
    }
}

/// Map a sqlx error to an internal-error tuple for handlers returning
/// `(StatusCode, String)`.
pub fn err_db(e: sqlx::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}"))
}