# Bank8:Reporting - Business Requirements & Functionality

> Audience: product owners, business analysts, and anyone evaluating what
> Bank8:Reporting **does for the business**. For architecture read
> `README.md`; for wire-level detail read `DESIGN.md`.

## 1. Business Purpose

- **Regulatory filings on schedule, in the right format** - the bank
  meets every regulator's prudential/statistical return obligations
  (capital, liquidity, financial, large exposures, granular credit)
  without bespoke per-jurisdiction code.
- **One filing executor for the whole vertical** - AML/CTF reports
  (STR/SAR/CTR/EFTR/LCTR) and sanctions reports are *detected* by
  Bank8:Compliance and *filed* here, so there is exactly one mechanism
  that submits to a regulator.
- **Portable across jurisdictions** - Canada (OSFI), US (Fed/OCC/FDIC,
  FinCEN), EU (ECB/EBA/NCAs), UK (PRA) are runtime registry data, not
  hardcoded; adding a jurisdiction is configuration, not engineering.

## 2. Business Scope (What the Service Provides)

| # | Capability area | What the business gets |
|---|---|---|
| 1 | Filing calendar | Jurisdiction-aware schedule of due returns; next-due and grace-period computation |
| 2 | Return templates | Declarative return definitions (data sources, mappings, validation, format) resolved from the jurisdiction registry |
| 3 | Return generation | Idempotent per (return, period) generation of capital/liquidity/financial/exposure returns |
| 4 | Packaging | XBRL instance documents and CSV extracts in the regulator's required format |
| 5 | Submission & acknowledgment | Submit to the configured regulator endpoint; track acknowledgment/rejection |
| 6 | Amendments & resubmission | Correction workflow; terminal states never regress |
| 7 | AML/CTF filing | File STR/SAR/CTR/EFTR/LCTR detected by Bank8:Compliance |
| 8 | Admin cockpit | RFW filing cockpit: calendar, run status, submission history, DLQ |
| 9 | Domain events | `bank8:reporting:*` family (return/filing/submission lifecycle) |
| 10 | Health & ops | Health endpoint, metrics, audit journal |

**What the service does NOT own (business boundary):**

| Out of scope | Owned by |
|---|---|
| Schedule timing | Sentin8:Cron |
| Approval/amendment workflow execution | Sentin8:Flow |
| Artifact byte storage | Sentin8:Storage |
| Balances/postings | Bank8:Ledger (read via public API) |
| Detection of reportable events | Bank8:Compliance / Bank8:Risk |
| Regulator endpoint credential custody | Sentin8 (reference-only) |

## 3. Business Capabilities

| Capability | Business rule |
|---|---|
| Calendar | A return is generated only when its obligation is due; re-firing never double-submits. |
| Templates | A template is declarative data; changing a regulator's format updates the registry, not the code. |
| Generation | Every return is validated before packaging; invalid returns never reach a regulator. |
| Submission | Submission is idempotent per (return, period); acknowledgment is recorded, rejection triggers amendment. |
| Amendments | Corrections are amendments, never mutations of a submitted filing. |
| AML filing | A detected STR/SAR/CTR is filed exactly once, or lands in the DLQ for manual replay. |

## 4. Business Events (Outbound)

| Event | Fires when |
|---|---|
| `bank8:reporting:return:created` | A return obligation is scheduled |
| `bank8:reporting:return:generated` | A return has been generated + validated |
| `bank8:reporting:filing:submitted` | A filing was submitted to a regulator |
| `bank8:reporting:filing:amended` | A filing was corrected and resubmitted |

## 5. Business Rules & Invariants

1. A filing is idempotent per (return, period).
2. Terminal submission states never regress; corrections are amendments.
3. Regulator credentials are reference-only.
4. No reportable event is silently dropped - DLQ is the floor.

## 6. Target-User Roles & Use Cases

| Role | Use case |
|---|---|
| Compliance officer | Reviews generated returns, approves amendments |
| Risk officer | Triggers ad-hoc returns, reviews exposure inputs |
| Regulator | Receives filings via the configured endpoint |

## 7. KPIs / Success Criteria

- 100% of due returns generated on schedule (no missed calendar obligations).
- Zero duplicate submissions to any regulator.
- All filings in the regulator's required format (XBRL/CSV).

## 8. Dependencies & Reliance on Other Services

- Sentin8:Cron (timing), Sentin8:Flow (workflows), Sentin8:Storage (artifacts).
- Bank8:Ledger (balances), Bank8:Risk (exposure inputs), Bank8:Compliance (detection).

## 9. Acceptance Criteria

1. A due return generates, validates, packages, and submits without manual steps.
2. A rejected submission triggers an amendment workflow.
3. A detected STR/SAR/CTR files through this service, not Compliance.
4. Adding a jurisdiction is registry data only - no code change.

## 10. Open Business Questions

- Portal-only (non-machine-readable) regulator submissions.
- Multi-version XBRL taxonomy support.

---

**Design attribution:** bank8:reporting and its underlying concepts were designed by Hamid Jahanzad <Hamid@Jahanzad.com>.
