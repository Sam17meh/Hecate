# Milestone 5 focused audit report

## Scope and baseline

This report records the initial review before remediation. The reviewed baseline is commit `a127b5388c87412a3379838ed33763c1ef0519cf` on branch `main`. The working tree was clean, with no untracked files. The repository has no `.github/workflows` CI configuration. The implementation and documentation cover repository analysis, GitHub discovery and source archives, workspace preparation, the native provider/runtime, read-only repository tools, and approved patch application.

## Findings before remediation

### Medium — GitHub response and issue data are unbounded

`src/github.rs` uses `Response::json()` for API payloads without a response-byte limit, and `request()` buffers other error response bodies completely before truncating them for display. `open_issues` can make up to 1,000 requests and retains every parsed issue and full body in memory. The page size requested from GitHub does not constrain bytes received or guarantee that a server returns at most 100 records. A large or malformed response, or a repository with extensive issue data, can consume excessive memory or processing time during `discover` and `plan`. The 30-second HTTP timeout applies per request and is not a total workflow deadline. This is in scope and should be fixed with bounded reads plus an aggregate issue-data budget.

### Medium — no cross-platform CI evidence

There is no CI workflow to run formatting, checking, and tests on Windows, Ubuntu, and macOS. The implementation has distinct Unix and Windows filesystem and patch-application paths (`src/workspace.rs`, `src/agent/patch_application.rs`), so a Windows-only local run cannot establish that the Unix branch remains buildable or that the macOS/Ubuntu behavior remains covered. Local tests will be reported separately from any configured matrix.

### Low — release metadata was incomplete at audit time

At the Milestone 5 review, `README.md` advertised the license as TBD and no license files existed. The project owner had not yet selected terms, so the engineering review did not choose a license. Milestone 6A subsequently records the owner-approved dual MIT OR Apache-2.0 choice and the owner's confirmation that they created the original project material, with the MIT notice `Copyright 2026 Samarth Mehrotra`.

### Informational — threat-model boundary remains material

Workspace preparation and final Windows application use path-based OS operations around retained handles. The threat model in `docs/SECURITY.md` explicitly excludes malicious same-user concurrent modification of the selected workspace parent, and describes multi-file application as non-atomic and not crash-safe. These are documented limitations, not findings against the stated bounded local-CLI threat model. The release assessment must retain those qualifications.

## Independent review and remediation

An independent reviewer inspected the source and report read-only, did not modify files, and did not run formatters, builds, or tests. The reviewer confirmed the Medium GitHub response finding, including unbounded error-body buffering, and found no Critical or High approval/application bypass in the inspected trust chain. The report's initial finding set above records the state before remediation.

The GitHub response finding was fixed in `src/github.rs`: response reads now stop at 8 MiB for JSON and 4 KiB for error responses, including close-delimited responses without a declared length. Issue pagination fails with an actionable error above 64 MiB aggregate response data or 10,000 non-pull-request issues. Three regression tests cover streaming limits and budget enforcement. README, architecture, and security documentation describe these limits.

The missing CI configuration was addressed by adding `.github/workflows/ci.yml`, which runs formatting, locked checking, and locked tests on Windows, Ubuntu, and macOS for pushes and pull requests. These remote jobs have not executed in this review; only Windows was run locally.

### Compiler warning review

The nine warning groups from the initial `cargo check` were inspected and resolved:

1. `FindingCategory` had production-dead `Bug`, `Security`, `Testing`, `Performance`, `Documentation`, `Maintainability`, `ContributionOpportunity`, and `Other` variants. `Testing` appeared only in a generic validation fixture, which now uses an emitted category. The internal enum now contains only categories emitted by the current finding builder: `Dependency`, `Architecture`, and `Reliability`.
2. `Impact` had unused `Low`, `High`, and `Critical` variants. The current builder emits only `Medium`, so the unused values were removed.
3. `Confidence::High` was never emitted; it was removed. Current findings use `Low` or `Medium`.
4. `Effort::Medium` and `Effort::Large` were never emitted; they were removed. Current findings use `Small`.
5. `ValidationStatus::Observed`, `Validated`, and `Rejected` were never emitted; they were removed. Current findings use `NeedsValidation`.
6. `save_plan` is now compiled only for tests because its remaining callers are test fixtures.
7. `load_plan` is now compiled only for tests for the same reason.
8. `write_json_new` had no callers and was removed.
9. `write_json_bytes_new` only supported the test-only plan writer, so it is now compiled only for tests. Its `OpenOptions` import is also test-only.

No warnings are suppressed. The final `cargo check --locked` completed without warnings.

## Final release-readiness assessment

### Findings by severity

- **Critical:** None identified in this review.
- **High:** None identified in the independent review of the approval/application trust chain.
- **Medium:** Unbounded GitHub response and issue accumulation — fixed with per-response, aggregate-byte, and issue-count limits and regression coverage.
- **Medium:** No cross-platform CI configuration — addressed by the new three-platform workflow; remote CI results remain unverified.
- **Low, at the time of this assessment:** No license had been selected or included. Milestone 6A subsequently records the owner-approved license selection, attribution, texts, and metadata. This historical finding is resolved by that later work.
- **Informational, remaining:** The bounded local-CLI threat model excludes malicious same-user concurrent modification of the selected workspace parent. Preparation is not race-free or an OS sandbox; multi-file patch application is not atomic as a group or crash-safe. The limits and recovery behavior remain documented in `SECURITY.md`.

### Verified security boundaries

Review confirmed the model has only the registered read-only repository tools; tool dispatch uses registry, permission, argument, and runtime-budget checks. Generated proposals remain untrusted and use the same validator as supplied proposals. A private, process-local, one-use approval binds the exact patch and workspace; application revalidates, stages, verifies, and reports rollback/recovery outcomes. No model-controlled approval, shell, arbitrary process execution, repository test runner, automatic commit, push, or pull request was found. Provider redirects are disabled, remote endpoints require HTTPS, transport sizes/time/retries are bounded, and provider data disclosure is documented. Filesystem/workspace limits remain those stated in `SECURITY.md`; this review does not claim OS isolation or production-grade security.

### Platforms and validation

- Local platform: Windows.
- CI configured: Windows, Ubuntu, macOS, using stable Rust; no remote job result was available during this review.
- `cargo fmt --check`: passed.
- `cargo check --locked`: passed with zero compiler warnings after the warning cleanup.
- `cargo test --locked`: passed — 187 unit tests, 7 contribution CLI integration tests, and 3 planning CLI integration tests (197 total; 0 failed).
- CLI help: `cargo run --locked -- --help`, `cargo run --locked -- contribute --help`, and `cargo run --locked -- inspect --help` all passed.
- `git diff --check`: passed (Git emitted line-ending normalization notices for edited text files).

### Dependencies and remaining limitations

No dependency or lockfile changes were needed. Direct dependencies were reviewed against source usage and `cargo tree --locked --depth 1`; the lockfile was used by all validation commands. `cargo-audit` is not installed, so known advisory database scanning was not performed; no network installation or advisory lookup was attempted. `Cargo.toml` does not declare an MSRV, and CI checks stable Rust only. The license selection was undecided at the time of this assessment; Milestone 6A records the owner's later choice and attribution. Dependency license metadata is descriptive input for review, not a legal compatibility determination. Local validation covers Windows only, despite the newly configured cross-platform CI.

The Milestone 6A Cargo metadata inventory found license metadata for the reviewed dependency packages; this does not establish that every license is compatible with HECATE's dual license. Expressions requiring particular care include `ring` (Apache-2.0 AND ISC), `webpki-root-certs` (CDLA-Permissive-2.0), and ICU Unicode-3.0 packages. The inventory also includes packages with combined license expressions such as AWS-LC. These metadata strings warrant manual legal review; no compatibility conclusion is made here.

### Decision

**At the time of the Milestone 5 assessment, HECATE was not ready for an open-source public release.** The implementation was a release candidate under its documented bounded local-CLI threat model, and the identified actionable Medium resource issue was fixed. Milestone 6A later records the selected license terms and confirmed attribution; configured Ubuntu/macOS CI jobs still need results before claiming those platforms pass. This assessment makes no claim of complete sandbox isolation or production-grade security.
