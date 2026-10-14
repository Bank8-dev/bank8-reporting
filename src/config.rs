// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! Reporting configuration.
//!
//! URLs only — no tokens. Every outbound platform call (Bus, Flow,
//! Storage, peer public-API reads) sources its token from the service's
//! own cert-bound DCR identity at the call site (single identity plane,
//! principles §4.1): a failed DCR fails the call closed; there is no
//! static-token fallback.

use std::env;

use sentin8::auth::client::{Client as AuthClient, ClientOptions};

/// Reporting configuration.
#[derive(Clone)]
pub struct ReportingConfig {
    pub port: u16,
    pub database_url: String,
    pub sentin8_auth_issuer: String,
    pub sentin8_auth_jwks_url: String,
    /// Bank8:Ledger's public API — the generation engine's balance reads
    /// (never its tables).
    pub ledger_url: String,
    /// Bank8:Risk's public API — the generation engine's exposure inputs.
    pub risk_url: String,
    pub sentin8_bus_url: String,
    pub stream_family: String,
    pub sentin8_flow_url: String,
    pub sentin8_storage_url: String,
    /// Shared Auth client — constructed once so the discovery and JWKS
    /// caches are reused across every request.
    pub sentin8_auth_client: AuthClient,
}

impl ReportingConfig {
    pub fn from_env() -> Self {
        let cfg = sentin8::config::Sentin8Config::from_env();
        Self {
            port: env::var("BANK8_REPORTING_PORT").expect("BANK8_REPORTING_PORT must be set by captain").parse().expect("BANK8_REPORTING_PORT must be a valid u16"),
            database_url: env::var("BANK8_REPORTING_DATABASE_URL").unwrap_or_else(|_| env::var("DATABASE_URL").unwrap_or_else(|_| sentin8::config::Sentin8Config::default_database_url().to_string())),
            sentin8_auth_issuer: env::var("SENTIN8_AUTH_ISSUER").unwrap_or_else(|_| cfg.sentin8_auth_issuer()),
            sentin8_auth_jwks_url: env::var("SENTIN8_AUTH_JWKS_URL").unwrap_or_else(|_| cfg.sentin8_auth_jwks_url()),
            ledger_url: env::var("BANK8_LEDGER_URL").unwrap_or_else(|_| format!("https://{}", cfg.platform_domain("bank8:ledger"))),
            risk_url: env::var("BANK8_RISK_URL").unwrap_or_else(|_| format!("https://{}", cfg.platform_domain("bank8:risk"))),
            sentin8_bus_url: env::var("SENTIN8_BUS_URL").unwrap_or_else(|_| cfg.sentin8_bus_url()),
            stream_family: env::var("BANK8_REPORTING_STREAM_FAMILY").unwrap_or_else(|_| "reporting".into()),
            sentin8_flow_url: env::var("SENTIN8_FLOW_URL").unwrap_or_else(|_| cfg.sentin8_flow_url()),
            sentin8_storage_url: env::var("SENTIN8_STORAGE_URL").unwrap_or_else(|_| cfg.sentin8_storage_url()),
            sentin8_auth_client: AuthClient::new(ClientOptions {
                base_url: env::var("SENTIN8_AUTH_ISSUER").unwrap_or_else(|_| cfg.sentin8_auth_issuer()),
                ..Default::default()
            }),
        }
    }
}