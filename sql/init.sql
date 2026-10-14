-- SPDX-FileCopyrightText: Copyright Logically Magic Inc. <hello@logicallymagic.com>
-- SPDX-FileCopyrightText: Copyright Hamid Jahanzad <Hamid@Jahanzad.com>
-- SPDX-FileCopyrightText: Copyright Contributors to the Bank8:Reporting
-- SPDX-License-Identifier: Apache-2.0

-- Bank8:Reporting — the service's own schema `reporting`
-- (schema-per-service): the platform-scoped jurisdiction registry
-- (jurisdictions/regulators/return_templates — provisioned from
-- vertical-registry.json by `vertical enroll`, not tenant-authored) +
-- env-scoped filing tables + reporting.outbox / reporting.audit_journal
-- producer tables, per services/reporting/DESIGN.md §3.

CREATE SCHEMA IF NOT EXISTS reporting;

-- ── Jurisdiction registry (platform-scoped — no tenant RLS; the
-- rows are registry data, written by env-init seeding, never by
-- tenant requests) ─────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS reporting.jurisdictions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    country_code    TEXT NOT NULL UNIQUE,
    metadata        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS reporting.regulators (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    jurisdiction_id UUID NOT NULL REFERENCES reporting.jurisdictions(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    authority_kind  TEXT NOT NULL CHECK (authority_kind IN ('prudential', 'fiu', 'sanctions', 'conduct')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (jurisdiction_id, name)
);

CREATE INDEX IF NOT EXISTS idx_reporting_regulators_jurisdiction
    ON reporting.regulators (jurisdiction_id);

CREATE TABLE IF NOT EXISTS reporting.return_templates (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    regulator_id    UUID NOT NULL REFERENCES reporting.regulators(id) ON DELETE CASCADE,
    return_code     TEXT NOT NULL,
    output_format   TEXT NOT NULL CHECK (output_format IN ('XBRL', 'CSV')),
    cadence         TEXT NOT NULL CHECK (cadence IN ('daily', 'monthly', 'quarterly', 'annual')),
    -- Declarative data-source/mapping/validation definition — registry
    -- data; changing a regulator's format updates the registry, not the
    -- code (DESIGN.md §3).
    definition      JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (regulator_id, return_code)
);

CREATE INDEX IF NOT EXISTS idx_reporting_templates_regulator
    ON reporting.return_templates (regulator_id);

-- ── Env-scoped filing tables (FORCE RLS) ────────────────────────────────

-- Submission targets — credentials resolved by REFERENCE through the
-- platform secret store; plaintext never enters a row (DESIGN.md §8).
CREATE TABLE IF NOT EXISTS reporting.regulator_endpoints (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    regulator_id    UUID NOT NULL REFERENCES reporting.regulators(id),
    endpoint_kind   TEXT NOT NULL CHECK (endpoint_kind IN ('portal', 'sftp', 'api')),
    url             TEXT NOT NULL,
    -- Secret reference only (sentin8-secrets://) — never plaintext.
    credential_ref  TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_reporting_endpoints_regulator
    ON reporting.regulator_endpoints (regulator_id, env_id);

CREATE TABLE IF NOT EXISTS reporting.filing_obligations (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    template_id     UUID NOT NULL REFERENCES reporting.return_templates(id),
    period          TEXT NOT NULL,
    next_due        TIMESTAMPTZ NOT NULL,
    grace_period    TEXT,
    status          TEXT NOT NULL DEFAULT 'scheduled'
        CHECK (status IN ('scheduled', 'due', 'filed', 'overdue')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (env_id, template_id, period)
);

CREATE INDEX IF NOT EXISTS idx_reporting_obligations_env_due
    ON reporting.filing_obligations (env_id, next_due);

CREATE TABLE IF NOT EXISTS reporting.filing_runs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    obligation_id   UUID NOT NULL REFERENCES reporting.filing_obligations(id),
    period          TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'draft' CHECK (status IN
        ('draft', 'generated', 'validated', 'packaged', 'submitted', 'acknowledged', 'rejected', 'amended')),
    -- Idempotency per (obligation, period): a re-fired calendar can
    -- never double-submit (DESIGN.md §5/§8).
    idempotency_key TEXT NOT NULL,
    artifact_ref    TEXT,
    validation_errors JSONB,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (obligation_id, period)
);

CREATE INDEX IF NOT EXISTS idx_reporting_runs_env
    ON reporting.filing_runs (env_id, status);

CREATE TABLE IF NOT EXISTS reporting.submissions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    run_id          UUID NOT NULL REFERENCES reporting.filing_runs(id),
    endpoint_id     UUID NOT NULL REFERENCES reporting.regulator_endpoints(id),
    regulator_ref   TEXT,
    acknowledgment  TEXT CHECK (acknowledgment IN ('pending', 'acknowledged', 'rejected')),
    submitted_at    TIMESTAMPTZ,
    acknowledged_at TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_reporting_submissions_run
    ON reporting.submissions (run_id);

-- Corrections are amendments — terminal states never regress (§8).
CREATE TABLE IF NOT EXISTS reporting.amendments (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    submission_id   UUID NOT NULL REFERENCES reporting.submissions(id),
    reason          TEXT NOT NULL,
    new_run_id      UUID NOT NULL REFERENCES reporting.filing_runs(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ── Env-scoped RLS (obligations/runs/submissions/amendments/
-- endpoints) ─────────────────────────────────────────────────────────
ALTER TABLE reporting.regulator_endpoints ENABLE ROW LEVEL SECURITY;
ALTER TABLE reporting.regulator_endpoints FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS endpoints_dml ON reporting.regulator_endpoints;
CREATE POLICY endpoints_dml ON reporting.regulator_endpoints
    USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())
    WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id());

ALTER TABLE reporting.filing_obligations ENABLE ROW LEVEL SECURITY;
ALTER TABLE reporting.filing_obligations FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS obligations_dml ON reporting.filing_obligations;
CREATE POLICY obligations_dml ON reporting.filing_obligations
    USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())
    WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id());

ALTER TABLE reporting.filing_runs ENABLE ROW LEVEL SECURITY;
ALTER TABLE reporting.filing_runs FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS runs_dml ON reporting.filing_runs;
CREATE POLICY runs_dml ON reporting.filing_runs
    USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())
    WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id());

ALTER TABLE reporting.submissions ENABLE ROW LEVEL SECURITY;
ALTER TABLE reporting.submissions FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS submissions_dml ON reporting.submissions;
CREATE POLICY submissions_dml ON reporting.submissions
    USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())
    WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id());

ALTER TABLE reporting.amendments ENABLE ROW LEVEL SECURITY;
ALTER TABLE reporting.amendments FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS amendments_dml ON reporting.amendments;
CREATE POLICY amendments_dml ON reporting.amendments
    USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())
    WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id());

-- ── Local outbox (DBX, architecture-principles §6.7) ─────────────────────
-- Producer path: reporting rows are appended to the outbox in the SAME
-- transaction as the domain change (or via the pool helper on handlers
-- without an open transaction); the shell's post-commit relay pushes them
-- to Bus POST /v1/produce (idempotency_key = row id). Generated from
-- sentin8-outbox::outbox_table_sql("reporting") — the outbox lives in the
-- service's OWN leaf schema, matching the relay's notify channel
-- (`reporting_outbox_changed`) and the search_path-derived table
-- resolution.
CREATE TABLE IF NOT EXISTS reporting.outbox (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    env_id          UUID NOT NULL,
    event_type      TEXT NOT NULL,
    stream_family   TEXT NOT NULL,
    payload         JSONB NOT NULL,
    publisher_id_token  TEXT,
    publisher_subject   TEXT,
    relayed         BOOLEAN NOT NULL DEFAULT false,
    relayed_at      TIMESTAMPTZ,
    relay_attempts  INT NOT NULL DEFAULT 0,
    next_retry_at   TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_reporting_outbox_unrelayed
    ON reporting.outbox (created_at) WHERE relayed = false;
ALTER TABLE reporting.outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE reporting.outbox FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS outbox_select ON reporting.outbox;
CREATE POLICY outbox_select ON reporting.outbox FOR SELECT
    USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id());
DROP POLICY IF EXISTS outbox_insert ON reporting.outbox;
CREATE POLICY outbox_insert ON reporting.outbox FOR INSERT
    WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id());
DROP POLICY IF EXISTS outbox_update ON reporting.outbox;
CREATE POLICY outbox_update ON reporting.outbox FOR UPDATE
    USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id());
CREATE OR REPLACE FUNCTION reporting.reporting_notify_outbox() RETURNS TRIGGER
LANGUAGE plpgsql AS $fn$
BEGIN
    PERFORM pg_notify('reporting_outbox_changed', '');
    RETURN NEW;
END;
$fn$;
DROP TRIGGER IF EXISTS reporting_outbox_notify_trigger ON reporting.outbox;
CREATE TRIGGER reporting_outbox_notify_trigger
    AFTER INSERT ON reporting.outbox
    FOR EACH ROW
    EXECUTE FUNCTION reporting.reporting_notify_outbox();

-- ── Local audit journal (DBX §6.8) ────────────────────────────────────────
-- Spill journal for the canonical audit push: rows written same-tx as the
-- domain change, pushed to Sys POST /v1/audit (sentin8:sys:audit:write),
-- then marked pushed. A Sys outage never drops audit.
-- Generated from sentin8-audit::audit_journal_sql("reporting").
CREATE TABLE IF NOT EXISTS reporting.audit_journal (
    id              UUID PRIMARY KEY,
    env_id          UUID NOT NULL,
    action          TEXT NOT NULL,
    target_resource_type TEXT,
    target_resource_id    TEXT,
    actor_user_id   TEXT,
    actor_subject_type TEXT,
    severity        TEXT NOT NULL DEFAULT 'info',
    correlation_id  TEXT,
    before_state    JSONB,
    after_state     JSONB,
    details         JSONB,
    pushed          BOOLEAN NOT NULL DEFAULT false,
    pushed_at       TIMESTAMPTZ,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_reporting_audit_journal_unpushed
    ON reporting.audit_journal (created_at) WHERE pushed = false;
ALTER TABLE reporting.audit_journal ENABLE ROW LEVEL SECURITY;
ALTER TABLE reporting.audit_journal FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS audit_journal_dml ON reporting.audit_journal;
CREATE POLICY audit_journal_dml ON reporting.audit_journal
    USING (sentin8_context_initialized() AND env_id = sentin8_current_env_id())
    WITH CHECK (sentin8_context_initialized() AND env_id = sentin8_current_env_id());

-- ── Jurisdiction registry seed (platform-scoped data — JUR-REG-1) ────────
-- The four target jurisdictions as registry ROWS, never code (zero
-- regulator names in services/*/src — principles §4.14 extended to
-- jurisdictions). Idempotent inserts: ON CONFLICT keeps the registry
-- stable across re-inits; metadata carries the threshold/cadence data
-- the consumers read. The AML/CTF thresholds drive compliance
-- monitoring_rules; the capital/liquidity floors drive risk risk_limits
-- defaults (both seeded by their own init.sql from the same values).

INSERT INTO reporting.jurisdictions (country_code, metadata) VALUES
    ('CA', '{"name": "Canada"}'::jsonb),
    ('US', '{"name": "United States"}'::jsonb),
    ('GB', '{"name": "United Kingdom"}'::jsonb),
    ('EU', '{"name": "European Union / EEA"}'::jsonb),
    ('CH', '{"name": "Switzerland"}'::jsonb),
    ('SG', '{"name": "Singapore"}'::jsonb),
    ('AU', '{"name": "Australia"}'::jsonb),
    ('HK', '{"name": "Hong Kong SAR"}'::jsonb),
    ('JP', '{"name": "Japan"}'::jsonb)
ON CONFLICT (country_code) DO NOTHING;

-- Prudential + FIU + sanctions + conduct authorities per jurisdiction.
INSERT INTO reporting.regulators (jurisdiction_id, name, authority_kind)
SELECT j.id, r.name, r.authority_kind
FROM (VALUES
    ('CA', 'Office of the Superintendent of Financial Institutions', 'prudential'),
    ('CA', 'Financial Transactions and Reports Analysis Centre', 'fiu'),
    ('CA', 'Canada Deposit Insurance Corporation', 'prudential'),
    ('US', 'Federal Reserve / OCC / FDIC', 'prudential'),
    ('US', 'Financial Crimes Enforcement Network', 'fiu'),
    ('US', 'Office of Foreign Assets Control', 'sanctions'),
    ('US', 'Consumer Financial Protection Bureau', 'conduct'),
    ('GB', 'Prudential Regulation Authority', 'prudential'),
    ('GB', 'Financial Conduct Authority', 'conduct'),
    ('GB', 'Office of Financial Sanctions Implementation', 'sanctions'),
    ('EU', 'European Central Bank / SSM', 'prudential'),
    ('EU', 'European Banking Authority', 'prudential'),
    ('EU', 'Council of the EU (CFSP)', 'sanctions')
) AS r(country_code, name, authority_kind)
JOIN reporting.jurisdictions j ON j.country_code = r.country_code
ON CONFLICT (jurisdiction_id, name) DO NOTHING;

-- REG-JUR-2 wave 2: the additional jurisdictions' authorities (same
-- idempotent pattern; values as registry data, never code).
INSERT INTO reporting.regulators (jurisdiction_id, name, authority_kind)
SELECT j.id, r.name, r.authority_kind
FROM (VALUES
    ('CH', 'Swiss Financial Market Supervisory Authority', 'prudential'),
    ('CH', 'Money Laundering Reporting Office Switzerland', 'fiu'),
    ('SG', 'Monetary Authority of Singapore', 'prudential'),
    ('SG', 'Suspicious Transaction Reporting Office', 'fiu'),
    ('AU', 'Australian Prudential Regulation Authority', 'prudential'),
    ('AU', 'Australian Transaction Reports and Analysis Centre', 'fiu'),
    ('HK', 'Hong Kong Monetary Authority', 'prudential'),
    ('HK', 'Joint Financial Intelligence Unit', 'fiu'),
    ('JP', 'Japanese Financial Services Agency', 'prudential'),
    ('JP', 'National Police Agency Financial Intelligence', 'fiu')
) AS r(country_code, name, authority_kind)
JOIN reporting.jurisdictions j ON j.country_code = r.country_code
ON CONFLICT (jurisdiction_id, name) DO NOTHING;

-- Returns required per prudential authority. `definition` carries the
-- declarative data-source/mapping/validation rules + the jurisdiction's
-- threshold metadata the consumers read.
INSERT INTO reporting.return_templates (regulator_id, return_code, output_format, cadence, definition)
SELECT reg.id, t.return_code, t.output_format, t.cadence, t.definition::jsonb
FROM (VALUES
    ('CA', 'Office of the Superintendent of Financial Institutions', 'capital-adequacy', 'XBRL', 'quarterly', '{"family": "capital", "floors": {"capital_ratio": "10.5", "lcr": "100", "nsfr": "100"}}'),
    ('US', 'Federal Reserve / OCC / FDIC', 'call-report', 'XBRL', 'quarterly', '{"family": "capital", "floors": {"capital_ratio": "10.5", "lcr": "100", "nsfr": "100"}}'),
    ('GB', 'Prudential Regulation Authority', 'corep', 'XBRL', 'quarterly', '{"family": "capital", "floors": {"capital_ratio": "10.5", "lcr": "100", "nsfr": "100"}}'),
    ('EU', 'European Central Bank / SSM', 'corep', 'XBRL', 'quarterly', '{"family": "capital", "floors": {"capital_ratio": "10.5", "lcr": "100", "nsfr": "100"}}'),
    ('EU', 'European Banking Authority', 'finrep', 'XBRL', 'quarterly', '{"family": "financial"}'),
    ('US', 'Financial Crimes Enforcement Network', 'ctr', 'CSV', 'daily', '{"family": "aml", "thresholds": {"cash": "10000.00"}}'),
    ('US', 'Financial Crimes Enforcement Network', 'sar', 'CSV', 'daily', '{"family": "aml"}'),
    ('CA', 'Financial Transactions and Reports Analysis Centre', 'lctr', 'CSV', 'daily', '{"family": "aml", "thresholds": {"cash": "10000.00"}}'),
    ('CA', 'Financial Transactions and Reports Analysis Centre', 'eftr', 'CSV', 'daily', '{"family": "aml", "thresholds": {"eft": "10000.00"}}'),
    ('CA', 'Financial Transactions and Reports Analysis Centre', 'str', 'CSV', 'daily', '{"family": "aml"}'),
    ('GB', 'Financial Conduct Authority', 'str', 'CSV', 'daily', '{"family": "aml"}'),
    ('EU', 'European Banking Authority', 'amlr', 'CSV', 'daily', '{"family": "aml"}'),
    ('CH', 'Money Laundering Reporting Office Switzerland', 'str', 'CSV', 'daily', '{"family": "aml"}'),
    ('SG', 'Suspicious Transaction Reporting Office', 'str', 'CSV', 'daily', '{"family": "aml", "thresholds": {"cash": "20000.00"}}'),
    ('AU', 'Australian Transaction Reports and Analysis Centre', 'smr', 'CSV', 'daily', '{"family": "aml", "thresholds": {"cash": "10000.00"}}'),
    ('HK', 'Joint Financial Intelligence Unit', 'str', 'CSV', 'daily', '{"family": "aml", "thresholds": {"cash": "120000.00"}}'),
    ('JP', 'National Police Agency Financial Intelligence', 'str', 'CSV', 'daily', '{"family": "aml"}')
) AS t(country_code, regulator_name, return_code, output_format, cadence, definition)
JOIN reporting.regulators reg
  ON reg.name = t.regulator_name
 AND reg.jurisdiction_id = (SELECT id FROM reporting.jurisdictions WHERE country_code = t.country_code)
ON CONFLICT (regulator_id, return_code) DO NOTHING;

-- ── Per-service role grants + search_path (F1 — §4.3e) ──────────────────
-- The reporting service connects via its own PostgreSQL role
-- (bank8_reporting_service, created + granted by
-- `sentin8-cli vertical init/enroll` from this service's
-- --print-registration --section=pg_roles). RLS only ENFORCES when the
-- service does NOT bypass it. These grants give the role table access;
-- the RLS policies scope the rows. Grants are scoped to the OWN schema
-- only — no blanket grants (SCHEMA-1 discipline).
-- The search_path puts the service's OWN schema FIRST (outbox/
-- audit_journal resolution for the shell's relay + journal pusher;
-- unqualified domain tables resolve here too), then public (RLS helpers).
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'bank8_reporting_service') THEN
        ALTER ROLE bank8_reporting_service SET search_path = reporting, public;
        GRANT USAGE ON SCHEMA reporting TO bank8_reporting_service;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA reporting TO bank8_reporting_service;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA reporting TO bank8_reporting_service;
    END IF;
END $$;