# Adaptive oj fork release

## Goal
Complete an independent crates.io release of the Solid-support fork as `adaptive-oj` while upstream integration is pending. Preserve the installed executable name `oj` and existing Rust import names. The user has authorized committing, pushing, publishing, creating the release tag, and verifying the install autonomously. Do not start development servers.

## Decisions
- Checkout: `/home/david/adaptive/oj`, branch `solid-support`.
- `adaptive-oj` was absent from crates.io when checked. Internal names must also be checked before release.
- Retain workspace version `0.2.23`; this is a separate package lineage.
- Use `adaptive-oj` / `adaptive-oj-*` for fork packages. Reuse compatible published upstream packages where actual archive/source and dependency-identity comparisons establish compatibility. Leave nonpublishable `oj_wasm` unchanged.
- Final release set: CLI, cache, compiler, resolver, server, env, config (seven fork packages). Use upstream `oj_graph`, `oj_js`, `oj_css`, `oj_diag`, `oj_deno_napi`, `oj_deno_process`, `oj_deno_runtime`, and `oj_deno_snapshots` at verified `0.2.22`. Keep the JS/runtime/NAPI/process dependency train consistently upstream; process hooks and NAPI exports must not mix fork identities.
- Exclude the six unused earlier adaptive publications from the CLI dependency graph and future release scripts. Their `0.2.23` versions are yanked; retain names/archives and do not republish or un-yank them. Permanent deletion was not accessible without a website login.
- Preserve internal dependency keys and actual existing library target names through explicit package aliases and library names. No Rust source import renaming.
- Point workspace repository metadata at `https://github.com/david1gp/oj`.
- Load `CARGO_REGISTRY_TOKEN` from `/home/david/leo_internal/dev-servers/david-server/env/env.conf` only for scoped release commands, without printing or persisting its value. Future CI may use crates.io Trusted Publishing configured for the fork and each new crate.
- Use existing project tooling. Do not add dependencies or run the full workspace test suite or e2e tests.

## Tasks
1. Complete — Rename publishable package identities and internal package aliases, preserve library/binary identities, update workspace metadata and lockfile. Audit resulting dependency graph using targeted metadata checks. Scope: manifests and Cargo.lock only.
2. Complete — Align existing CI/release configuration and document install, bootstrap publishing, and Trusted Publishing for the fork. Update affected Cargo package selectors in scripts/documentation and e2e harness commands; avoid unrelated rewrites. Scope: workflows, `tools/gen-snapshot-pin.sh`, README/development docs, e2e package selectors, and concise release documentation / SOLID.md installation guidance.
3. Complete — Independently review manifest/lockfile/workflow changes and perform targeted packaging checks feasible before the new dependencies are on crates.io. Validate no upstream internal package identities leak into registry dependencies. Report limitations without publishing or running broad builds.
4. Complete — Remove CLI compile-time reliance on sibling crate source files. Expose the exact existing embedded Start assets from their owning `oj_server` crate and consume them from the CLI without changing script names, bytes, iteration order, or runtime behavior. Verify via focused unit checks and targeted compilation; no full workspace tests or e2e.
5. Complete — Align the excluded fuzz manifest's internal dependency package aliases; preserve existing Rust import keys and verify lightweight fuzz metadata.
6. Complete — Perform release preflight on the existing changes, confirm publish authentication, and ensure packaged files are self-contained. Use targeted tests and checks, not broad e2e runs. Correct concrete release defects only.
7. Complete — Commit the Solid-support release changes in logical conventional commits and push `solid-support`. Use the commits skill. No PR is required for the fork release.
8. Complete — Bootstrap publish the required seven fork packages at `0.2.23` with the external scoped token, verifying registry results and resuming safely after partial success. Reuse compatible upstream `0.2.22` dependencies and keep credentials secret.
9. Complete with workflow limitation — Push the matching `v0.2.23` tag and create the GitHub release from the exact changelog notes. The tag produced no Actions run (the Actions API listed zero runs/workflows; Actions are enabled), so there are no job statuses to report. Publishing was already complete; no duplicate publish was attempted. The snapshot staleness check was evaluated directly and returned `stale=false`, so no harvest or PR was needed. Registry install smoke, all seven Trusted Publishing configurations, and all seven public registry statuses are verified. No upstream merge or `main` changes.
10. Complete — Replace compatible dependency edges with the verified upstream registry packages, align CLI direct/build NAPI and server process hooks, update release selectors/documentation, and rerun focused compilation/tests before continuing publication.
11. Complete — Determine crates.io deletion eligibility for the six unused earlier publications and perform supported cleanup: yank their `0.2.23` versions, retaining names and existing locked consumers. No required fork or upstream packages affected; keep unused local members nonpublishable.

## Verification approach
- Locate Cargo through existing development tooling if it is not on PATH.
- Run metadata with `--locked --no-deps` and validate package identities, preserved targets, aliases, publish flags, and dependency versions/paths.
- Use package listing and other lightweight package checks rather than full compilation. A dry-run publishing command may depend on unpublished registry dependencies; assess that before running it and do not confuse this with a complete release verification.
- Keep credentials secret; report presence only.

## Current context
The published CLI has passed a genuine Solid project build using the installed registry binary with `vite-plugin-solid`; the output contains Solid runtime transforms without React transforms, and the homepage checkout remains unchanged. The GitHub release exists at `https://github.com/david1gp/oj/releases/tag/v0.2.23`. Future Actions registration remains an external account prerequisite: authenticated fork activation enables all existing workflows, so the inherited scheduled nightly must be protected before activation. No supported release-only CLI registration route was established. Do not move the release tag, alter published crate contents, or change `main`.
Workspace contains 16 local members, with exactly seven publishable fork packages. Eight unused local adaptive packages and `oj_wasm` are nonpublishable. The CLI release graph consumes the eight verified upstream registry packages instead of their unused local counterparts.
The release workflow explicitly selects the seven required fork packages and uses OIDC. Keep selectors for the unchanged `oj_wasm` package intact. Initial scoped-token bootstrap is complete; future automated releases depend on fork workflow registration.
All seven required fork packages are published at `0.2.23`, unyanked and owned by `david1gp`, with their Trusted Publishing configurations verified. The release graph reuses eight verified upstream packages at `0.2.22`. All six earlier unused versions are yanked and publicly verified. Genuine registry installation and executable smoke checks are complete. The annotated `v0.2.23` tag is on `solid-support`, and the GitHub release was created manually from the exact changelog section because the tag produced no Actions run. The publish job was not run; the snapshot staleness check was evaluated directly (`stale=false`). Do not alter `main` or upstream.
