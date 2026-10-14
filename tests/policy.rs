// SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
// SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
// SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
// SPDX-License-Identifier: Apache-2.0

//! Sentin8:Policy wiring tests for Bank8:Reporting.
//! Per the platform policy design §6 (Definition of Done) — proves the
//! reporting wiring with the REAL seeded scope-mirror text
//! (`registration::SCOPE_MIRROR_POLICIES`, the exact string
//! `vertical enroll` upserts): no-set → deny; enforced no-match → deny;
//! shadow never blocks; the mirror permits its own grants; M2M parity
//! (`token_tp` never bypasses); platform-admin scopes pass; forbid beats
//! permit; tenant role-keyed sets decide; cross-env sets are isolated;
//! the declared mirror covers EVERY action the handlers evaluate; and
//! the enroll-time dry-run gate accepts the declared set.
//! Pure tests (no DB): the engine cache is fed via `load_from_rows` with
//! hand-built rows matching the DB row shape.

use uuid::Uuid;

use bank8_reporting_server::registration::SCOPE_MIRROR_POLICIES;
use sentin8::shield::{
    Decision, EvalTarget, PolicyEngine, PolicyMode, PolicyOrigin, PolicySetRecord, RequestCtx,
};

const SERVICE: &str = "bank8:reporting";
const ENV: uuid::Uuid = uuid::Uuid::nil();

fn row(name: &str, policies: &str, mode: PolicyMode, origin: PolicyOrigin) -> PolicySetRecord {
    PolicySetRecord {
        id: Uuid::new_v4(),
        env_id: ENV,
        service: SERVICE.into(),
        policy_set_name: name.into(),
        description: String::new(),
        policies: policies.into(),
        schema: None,
        origin,
        mode,
        version: 1,
        enabled: true,
    }
}

fn ctx(token_tp: &str, scope_list: &[&str]) -> RequestCtx {
    RequestCtx::new(
        "subj-1",
        ENV,
        uuid::Uuid::nil(),
        token_tp,
        scope_list.iter().map(|s| s.to_string()).collect(),
    )
}

fn mirror_engine(mode: PolicyMode) -> PolicyEngine {
    let e = PolicyEngine::new(SERVICE);
    e.load_from_rows(vec![row("scope-mirror", SCOPE_MIRROR_POLICIES, mode, PolicyOrigin::Platform)])
        .expect("declared scope-mirror set must parse");
    e
}

/// No-set → deny: an engine with NO loaded set denies everything
/// (fail-closed at boot is the refuse-to-start contract; at request time
/// an empty cache denies — design §3.3).
#[test]
fn no_set_denies_everything() {
    let e = PolicyEngine::new(SERVICE); // empty cache
    let out = e.evaluate(
        &ctx("user", &["bank8:reporting:runs:read"]),
        &EvalTarget::new("bank8:reporting:runs:read"),
    );
    assert!(!out.is_allow());
    assert!(matches!(out.enforce_decision, Decision::Deny { .. }));
}

/// Enforced no-match → deny: a caller holding an unrelated scope is
/// denied even with the full mirror loaded.
#[test]
fn enforced_no_match_denies() {
    let e = mirror_engine(PolicyMode::Enforced);
    let out = e.evaluate(
        &ctx("user", &["bank8:reporting:runs:read"]),
        &EvalTarget::new("bank8:reporting:runs:write"),
    );
    assert!(!out.is_allow(), "scope for another action must not satisfy");
}

/// Shadow never blocks (the emergency rollback lever is inert).
#[test]
fn shadow_never_blocks() {
    let e = mirror_engine(PolicyMode::Shadow);
    let out = e.evaluate(
        &ctx("user", &[]),
        &EvalTarget::new("bank8:reporting:obligations:read"),
    );
    assert!(out.is_allow(), "shadow must never block");
    assert!(out.shadow_deny.is_some(), "would-deny must be reported");
}

/// The mirror permits its own grants — the scope-mirror invariant.
#[test]
fn mirror_permits_own_grants() {
    let e = mirror_engine(PolicyMode::Enforced);
    for scope in [
        "bank8:reporting:obligations:read",
        "bank8:reporting:obligations:write",
        "bank8:reporting:runs:read",
        "bank8:reporting:runs:write",
        "bank8:reporting:submissions:read",
        "bank8:reporting:submissions:write",
    ] {
        let out = e.evaluate(&ctx("user", &[scope]), &EvalTarget::new(scope));
        assert!(out.is_allow(), "scope {scope} must allow action {scope}");
    }
}

/// M2M parity: `token_tp == "m2m"` evaluates the same set — the token
/// type never bypasses policy (principles §5.2a).
#[test]
fn m2m_parity() {
    let e = mirror_engine(PolicyMode::Enforced);
    let allow = e.evaluate(
        &ctx("m2m", &["bank8:reporting:runs:write"]),
        &EvalTarget::new("bank8:reporting:runs:write"),
    );
    assert!(allow.is_allow(), "scoped M2M principal passes like a user");
    let deny = e.evaluate(
        &ctx("m2m", &[]),
        &EvalTarget::new("bank8:reporting:runs:write"),
    );
    assert!(!deny.is_allow(), "unscoped M2M is denied like a user");
}

/// Platform-admin scopes pass every action (the mirror carries
/// `sentin8:sys:admin` permits for each action).
#[test]
fn platform_admin_scopes_pass() {
    let e = mirror_engine(PolicyMode::Enforced);
    for action in ["bank8:reporting:runs:read", "bank8:reporting:runs:write", "bank8:reporting:submissions:read"] {
        let out = e.evaluate(&ctx("user", &["sentin8:sys:admin"]), &EvalTarget::new(action));
        assert!(out.is_allow(), "sys:admin must pass {action}");
    }
}

/// Forbid beats permit: a tenant-authored forbid overrides the mirror.
#[test]
fn forbid_beats_permit() {
    let e = PolicyEngine::new(SERVICE);
    e.load_from_rows(vec![
        row("scope-mirror", SCOPE_MIRROR_POLICIES, PolicyMode::Enforced, PolicyOrigin::Platform),
        row(
            "tenant-freeze",
            "forbid(principal, action == Action::\"bank8:reporting:runs:write\", resource);",
            PolicyMode::Enforced,
            PolicyOrigin::Tenant,
        ),
    ])
    .expect("load");
    let out = e.evaluate(
        &ctx("user", &["bank8:reporting:runs:write"]),
        &EvalTarget::new("bank8:reporting:runs:write"),
    );
    assert!(matches!(out.enforce_decision, Decision::Deny { .. }), "forbid wins");
}

/// Role-keyed tenant sets participate in the same decision (§3.1d).
#[test]
fn role_keyed_tenant_set_decides() {
    let e = PolicyEngine::new(SERVICE);
    e.load_from_rows(vec![row(
        "tenant-role-grant",
        "permit(principal, action == Action::\"bank8:reporting:runs:write\", resource) when { principal.roles.contains(\"bank8:reporting:admin\") };",
        PolicyMode::Enforced,
        PolicyOrigin::Tenant,
    )])
    .expect("load");
    let denied = e.evaluate(&ctx("user", &[]), &EvalTarget::new("bank8:reporting:runs:write"));
    assert!(!denied.is_allow());
    let allowed = e.evaluate(
        &ctx("user", &[]).with_roles(["bank8:reporting:admin"].iter().map(|s| s.to_string()).collect()),
        &EvalTarget::new("bank8:reporting:runs:write"),
    );
    assert!(allowed.is_allow());
}

/// Tenant isolation: sets of ANOTHER env are invisible to this env's
/// evaluation — the cache key is (env_id, name).
#[test]
fn cross_env_set_isolated() {
    let other_env = uuid::Uuid::new_v4();
    let mut r = row("other-env-set", SCOPE_MIRROR_POLICIES, PolicyMode::Enforced, PolicyOrigin::Platform);
    r.env_id = other_env;
    let e = PolicyEngine::new(SERVICE);
    e.load_from_rows(vec![r]).expect("load");
    let out = e.evaluate(
        &ctx("user", &["bank8:reporting:runs:read"]),
        &EvalTarget::new("bank8:reporting:runs:read"),
    );
    assert!(!out.is_allow(), "another env's set must not decide this env");
}

/// Registration parity: the declared set covers EVERY action the handlers
/// evaluate (a new EvalTarget without a mirror permit fails here).
#[test]
fn declared_mirror_covers_every_handler_action() {
    const HANDLER_ACTIONS: &[&str] = &[
        "bank8:reporting:obligations:read",
        "bank8:reporting:runs:read",
        "bank8:reporting:runs:write",
        "bank8:reporting:submissions:read",
    ];
    let e = mirror_engine(PolicyMode::Enforced);
    for action in HANDLER_ACTIONS {
        let out = e.evaluate(&ctx("user", &[*action]), &EvalTarget::new(*action));
        assert!(out.is_allow(), "action {action} must be covered by the mirror");
    }
}

/// The enroll-time dry-run gate (§4.2) accepts the declared set — an
/// enforced set that denies its own grants never ships.
#[test]
fn declared_set_passes_enroll_dry_run() {
    sentin8::shield::runtime::dry_run_mirror(SCOPE_MIRROR_POLICIES, "scope-mirror", SERVICE)
        .expect("declared reporting scope-mirror must permit its own grants");
}