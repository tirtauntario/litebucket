# Documentation validation report

**Validation date:** October 4, 2026.

This report describes checks on the **specification bundle**, not tests of a storage application. No Rust application was built, no SDK interoperability suite was executed, and no crash, durability, or performance claim has been validated.

| Check | Result | Details |
|---|---|---|
| Reference schema executes | PASS | All 7 tables created using local SQLite 3.46.1; syntax/constraint checks only. |
| Byte-exact key ordering | PASS | 12 representative UTF-8 keys retain BLOB identity and match bytewise sort; includes Unicode composition differences, whitespace, and slashes. |
| Schema constraint checks | PASS | 14 negative cases rejected; foreign-key and integrity checks pass. Cross-table application invariants are not claimed as database-enforced. |
| TOML examples parse | PASS | Non-secret defaults/units agree with selected specification values; all three example credentials are disabled placeholders. |
| Standalone appendices | PASS | Embedded SQL, config, and credential examples exactly match their separate files. |
| Reference coverage | PASS | All numbered reference identifiers resolve within the 50-entry primary-source appendix. This check does not re-fetch URLs. |
| Document structure | PASS | Balanced code fences; unique headings/test IDs; 14 invariant IDs covered; AGENTS.md is under 32 KiB. |
| Acceptance plan | PASS | 60 named acceptance cases across seven implementation milestones; all application test statuses remain not run. |

## Important limits

The local SQLite version above was used only to execute the reference schema in an in-memory test database. It is **not** the recommended production dependency and was not used to test WAL durability. The implementation must resolve a patched bundled version and verify it at runtime as required by SPEC section 5.

The SQL deliberately relies on application transactions for cross-table blob ownership, lifecycle transitions, usage counters, and authorization. Passing the reference-schema checks does not prove those application guarantees.

TOML validation confirms syntax and selected template values, not parsing by a nonexistent Rust implementation. Source URLs were documented from primary-reference review; version pinning and compatibility validation remain milestone-0 and release responsibilities.
