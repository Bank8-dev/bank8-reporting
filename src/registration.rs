// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! Service self-registration data for Bank8:Reporting.
//! Output via `--print-registration` flag. See `sentin8/design/sentin8-service-self-registration-design.md`.

use sentin8::registration::{
    AdminScope, AdminUi, AdminUiPage, AuditFinding, ErrorCode, M2mGrant, PgRole, Registration,
    PolicySetDecl, RelationDecl, ScheduleDecl, ServiceActionEntry, SqlMigration,
};
use utoipa::OpenApi;

const PG_ROLES: &[PgRole] = &[
    PgRole {
        scope: "bank8:reporting:read,bank8:reporting:write",
        role: "bank8_reporting_service",
        bypass_rls: false,
        // Grants live in the bank8 vertical database only (DBX §6.6
        // database-per-vertical, schema-per-service).
        database: "bank8",
        // Reporting owns all its reads — inputs arrive as events or
        // public-API reads, never cross-schema grants.
        cross_schema_grants: &[],
    },
];

/// M2M DCR scope grants (self-registration design §3.11). One entry per
/// workload this service registers via DCR; `vertical enroll` upserts
/// `certless:<workload>` rows into `sentin8.auth_m2m_scope_grants`.
/// - `bank8:reporting` — the service's own client (server.rs): bus
///   produce/consume (worker + outbox relay path).
/// - `reporting` — the bare short-form spelling the runtime DCR call
///   sites pass; the gate looks up the exact client_name.
/// - `reporting-audit` — shell audit-journal pusher workload (audit
///   crate §6.8; boot step 8c).
/// - `reporting-worker` — the embedded filing worker (worker.rs): SSE
///   consume + Flow resume/retry.
const M2M_GRANTS: &[M2mGrant] = &[
    M2mGrant {
        workload: "bank8:reporting",
        scopes: &["sentin8:bus:produce", "sentin8:bus:consume", "sentin8:flow:write", "sentin8:storage:write"],
    },
    M2mGrant {
        workload: "reporting",
        scopes: &["sentin8:bus:produce", "sentin8:bus:consume", "sentin8:flow:write", "sentin8:storage:write"],
    },
    // Shield hot-reload consumer (policy crate `policy_bus_client`, full
    // colon identity — BUSPOLICY-1: the boot-minted cert is authorized by
    // the enroll-published registry map, and enrollment only provisions
    // declared workloads; without this row every policy SSE connect 401s
    // "workload is not enrolled" and hot-reload is dead for this service).
    M2mGrant {
        workload: "bank8:reporting-policy",
        scopes: &["sentin8:bus:consume", "sentin8:stat:measure"],
    },
    M2mGrant {
        workload: "reporting-audit",
        scopes: &["sentin8:sys:audit:write"],
    },
    M2mGrant {
        workload: "reporting-worker",
        scopes: &["sentin8:bus:consume", "sentin8:flow:read", "sentin8:flow:write", "sentin8:cron:write"],
    },
    // Event-driven env-init consumer (event_consumer.rs; consumer group
    // `bank8:reporting:envinit` under the short-form `reporting-envinit`
    // workload identity — BUSPOLICY-1: every runtime-presented identity
    // must be declared for the enroll-published registry map to authorize
    // it).
    M2mGrant {
        workload: "reporting-envinit",
        scopes: &["sentin8:bus:consume"],
    },
    // The platform Cron workload fires the filing-calendar due-check
    // proxy-back; its token must carry the same Shield target every human
    // mutation passes (the ledger-storage precedent: a service's grants
    // may name platform workloads).
    M2mGrant {
        workload: "sentin8:cron",
        scopes: &["bank8:reporting:runs:write"],
    },
];

const ADMIN_SCOPES: &[AdminScope] = &[
    AdminScope::Typed { role: "bank8:reporting:admin", scope: "bank8:reporting:obligations:read" },
    AdminScope::Typed { role: "bank8:reporting:admin", scope: "bank8:reporting:obligations:write" },
    AdminScope::Typed { role: "bank8:reporting:admin", scope: "bank8:reporting:runs:read" },
    AdminScope::Typed { role: "bank8:reporting:admin", scope: "bank8:reporting:runs:write" },
    AdminScope::Typed { role: "bank8:reporting:admin", scope: "bank8:reporting:submissions:read" },
    AdminScope::Typed { role: "bank8:reporting:admin", scope: "bank8:reporting:submissions:write" },
    // Policy-admin surface (policy design §3.6; principles §6.0b per-service scope).
    AdminScope::Typed { role: "bank8:reporting:admin", scope: "bank8:reporting:policy:read" },
    AdminScope::Typed { role: "bank8:reporting:admin", scope: "bank8:reporting:policy:write" },
];

const ERROR_CODES: &[ErrorCode] = &[
    ErrorCode { enum_name: "ReportingValidationFailed", error_code: "reporting_validation_failed", status_code: 422 },
    ErrorCode { enum_name: "ReportingAlreadySubmitted", error_code: "reporting_already_submitted", status_code: 409 },
];

const ADMIN_UI_PAGES: &[AdminUiPage] = &[
    AdminUiPage { route: "overview", label: "Overview", ui_endpoint: "/v1/admin/ui/overview", data_endpoint: "/v1/admin/overview", icon: "0xe1b1" },
    AdminUiPage { route: "calendar", label: "Calendar", ui_endpoint: "/v1/admin/ui/calendar", data_endpoint: "/v1/admin/overview", icon: "0xe3c9" },
    AdminUiPage { route: "runs", label: "Runs", ui_endpoint: "/v1/admin/ui/runs", data_endpoint: "/v1/admin/overview", icon: "0xf06be" },
    // Sentin8:Policy admin surface (§3.6, post pattern).
    AdminUiPage { route: "policy-sets", label: "Policy Sets", ui_endpoint: "/v1/admin/ui/policy-sets", data_endpoint: "/v1/admin/policy-sets", icon: "0xe8b8" },
    AdminUiPage { route: "policy-decisions", label: "Policy Decisions", ui_endpoint: "/v1/admin/ui/policy-decisions", data_endpoint: "/v1/admin/policy-decisions", icon: "0xf070" },
    AdminUiPage { route: "relations", label: "Relations", ui_endpoint: "/v1/admin/ui/relations", data_endpoint: "/v1/admin/relations", icon: "0xe572" },
    AdminUiPage { route: "attributes", label: "Attributes", ui_endpoint: "/v1/admin/ui/attributes", data_endpoint: "/v1/admin/attributes", icon: "0xe8d3" },
];

/// Default declarative policy set (Sentin8:Policy design §3.2/§3.4,
/// seeded by `vertical enroll` as `origin='platform'`, `mode='enforced'`):
/// action-granular scope permits over the LIVE effective scope set — the
/// exact mirror of the gates this service's handlers evaluate in step 5
/// (policy design §4.3). Step 5 is the only gate.
pub const SCOPE_MIRROR_POLICIES: &str = concat!(
    "permit(principal, action == Action::\"bank8:reporting:obligations:read\", resource) when { principal.scopes.contains(\"bank8:reporting:obligations:read\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:obligations:read\", resource) when { principal.scopes.contains(\"bank8:reporting:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:obligations:read\", resource) when { principal.scopes.contains(\"sentin8:sys:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:obligations:read\", resource) when { principal.scopes.contains(\"sentin8:sys:org:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:obligations:read\", resource) when { principal.scopes.contains(\"sentin8:sys:env:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:obligations:write\", resource) when { principal.scopes.contains(\"bank8:reporting:obligations:write\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:obligations:write\", resource) when { principal.scopes.contains(\"bank8:reporting:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:obligations:write\", resource) when { principal.scopes.contains(\"sentin8:sys:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:obligations:write\", resource) when { principal.scopes.contains(\"sentin8:sys:org:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:obligations:write\", resource) when { principal.scopes.contains(\"sentin8:sys:env:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:read\", resource) when { principal.scopes.contains(\"bank8:reporting:runs:read\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:read\", resource) when { principal.scopes.contains(\"bank8:reporting:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:read\", resource) when { principal.scopes.contains(\"sentin8:sys:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:read\", resource) when { principal.scopes.contains(\"sentin8:sys:org:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:read\", resource) when { principal.scopes.contains(\"sentin8:sys:env:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:write\", resource) when { principal.scopes.contains(\"bank8:reporting:runs:write\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:write\", resource) when { principal.scopes.contains(\"bank8:reporting:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:write\", resource) when { principal.scopes.contains(\"sentin8:sys:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:write\", resource) when { principal.scopes.contains(\"sentin8:sys:org:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:runs:write\", resource) when { principal.scopes.contains(\"sentin8:sys:env:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:read\", resource) when { principal.scopes.contains(\"bank8:reporting:submissions:read\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:read\", resource) when { principal.scopes.contains(\"bank8:reporting:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:read\", resource) when { principal.scopes.contains(\"sentin8:sys:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:read\", resource) when { principal.scopes.contains(\"sentin8:sys:org:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:read\", resource) when { principal.scopes.contains(\"sentin8:sys:env:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:write\", resource) when { principal.scopes.contains(\"bank8:reporting:submissions:write\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:write\", resource) when { principal.scopes.contains(\"bank8:reporting:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:write\", resource) when { principal.scopes.contains(\"sentin8:sys:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:write\", resource) when { principal.scopes.contains(\"sentin8:sys:org:admin\") };\n",
    "permit(principal, action == Action::\"bank8:reporting:submissions:write\", resource) when { principal.scopes.contains(\"sentin8:sys:env:admin\") };\n",
);

const POLICIES: &[PolicySetDecl] = &[PolicySetDecl {
    policy_set_name: "scope-mirror",
    description: "Action-granular scope permits for bank8:reporting over the LIVE effective scope set (design §3.1d). Step 5 is the only gate.",
    // Shield text lives in SCOPE_MIRROR_POLICIES (single source).
    policies: SCOPE_MIRROR_POLICIES,
    schema: None,
    mode: "enforced",
    default_enabled: true,
}];

/// Default ReBAC relation tuples (design §3.1c) — none declared by this
/// service today; env admins author their own graph via the relations
/// admin API.
const RELATIONS: &[RelationDecl] = &[];

const SQL_1_INIT: &str = include_str!("../sql/init.sql");

const AUDIT_FINDINGS: &[AuditFinding] = &[
    AuditFinding {
        finding: "Regulator credentials stored without a secret reference",
        recommendation: "Regulator endpoint credentials must be stored as a sentin8-secrets:// reference. The sentin8-secrets crate resolves the reference to the actual value at runtime.",
    },
];

const SERVICE_ACTIONS: &[ServiceActionEntry] = &[
    ServiceActionEntry {
        action: "reporting_run_regenerate",
        service_type: "bank8:reporting",
        platform_domain_key: "bank8:reporting",
        method: "POST",
        endpoint: "/v1/runs",
        scope: "bank8:reporting:runs:write",
    },
];

fn openapi_spec() -> serde_json::Value {
    serde_json::to_value(crate::openapi::ReportingOpenApi::openapi()).unwrap_or(serde_json::Value::Null)
}

/// Print the registration JSON to stdout.
pub fn print(section: Option<String>) -> anyhow::Result<()> {
    let reg = Registration {
        service_type: "bank8:reporting",
        subdomain: "bank8-reporting",
        health_path: "/v1/health",
        admin_scopes: ADMIN_SCOPES,
        error_codes: ERROR_CODES,
        consent_providers: &[],
        consent_flows: &[],
        policies: POLICIES,
        relations: RELATIONS,
        event_triggers: &[],
        // Filing-calendar due-check (SDD §2.4): daily 00:05 sweep — Cron
        // owns timing, reporting owns correctness. Provisioned at enroll;
        // fires the internal due-check route.
        schedules: &[
            ScheduleDecl {
                schedule_type: "filing-calendar-due-check",
                cron_expression: "5 0 * * *",
                timezone: "UTC",
                target_config: r#"{"internalPath": "/v1/internal/calendar", "scheduleType": "filing-calendar-due-check"}"#,
                max_retries: 3,
            },
        ],
        admin_ui: Some(AdminUi {
            label: "Reporting",
            icon: "0xe3c9",
            service_flag: "reporting",
            pages: ADMIN_UI_PAGES,
        }),
        sql_migrations: vec![
            SqlMigration { version: 1, name: "init", sql: SQL_1_INIT },
        ],
        audit_findings: Some(AUDIT_FINDINGS),
        service_actions: Some(SERVICE_ACTIONS),
        m2m_grants: Some(M2M_GRANTS),
        pg_roles: PG_ROLES,
        openapi_url: "/openapi.json",
        openapi: openapi_spec,
    };
    sentin8::registration::print_registration(section, &reg, openapi_spec)
}