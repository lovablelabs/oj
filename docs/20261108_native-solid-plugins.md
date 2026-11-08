# Native support for Solid-app Vite plugins in OJ

## Goal

Find out why OJ shows no memory/CPU improvement over Vite on SolidJS apps,
natively reimplement the Vite plugins where that measurably helps, and keep
OJ only if it wins. Reference app: `~/leo_customers/intersolaris`
(`vite.config.ts:69-92`): `tailwindcss()`, `tanstackStart({...})`,
`solidAiSrcPlugin()`, `solid({ssr:true})`, inline `llmsTxt()`.

## Decisions (user, 2026-11-08)

- Native = OJ reimplements the plugin and skips its JS. Measure first,
  reimplement only measured winners.
- Keep rule: **≥20% RSS saving vs Vite (median, like-for-like post-crawl)
  and no slower edits**. Below that: switch the apps back to Vite.
- Solid transform strategy, if measurement confirms it dominates:
  **SWC Solid transform** instead of `babel-preset-solid`.

## Current state (verified by source inspection, branch `solid-support`)

| Plugin | OJ today | Heavy work |
|---|---|---|
| `@tailwindcss/vite` | Hosted JS (`transform`), **plus** native `CssEngine::tailwind` (`crates/oj_server/src/css_engine.rs:102`, used `crates/oj/src/start_dev.rs:590`) — possible double work | oxide `Scanner` + `@tailwindcss/node compile().build()`, lightningcss at build |
| `tanstackStart` | Mostly native already (`tanstack[-:]` in `isReimplemented`, `native-plugins.mjs:55`); `import-protection` + route-splitting still hosted | route-tree codegen, server-fn compile, manifest |
| `vite-plugin-solid` | Hosted JS; native `solid-transform.mjs` only for Start bundles | `babel.transformAsync` + `babel-preset-solid` per `.tsx/.jsx`, both dom+ssr envs (2x) |
| `solidAiSrcPlugin` | Hosted JS, dev-only (`apply` serve+development) | `@babel/parser`+traverse+MagicString per `.tsx`, inserts `data-ai-src` |
| `llmsTxt()` | Hosted JS `configureServer` middleware | ~zero; app-local code (`renderLlmsTxt`) — cannot be generically reimplemented |

Plug-in host itself (`crates/oj_server/src/assets/plugin-host.mjs`) already
supports config/configResolved/configureServer/resolveId/load/transform/
transformIndexHtml/handleHotUpdate/generateBundle etc. Prior memory result
(prior thread, not re-measured): OJ at parity at best vs Vite ~465–520 MB.

## Hypothesis verdict (T1, measured on intersolaris, 45 routes)

"Most work happens in vite-plugin-solid": **refuted as stated.** Solid is
the largest single plugin (~30% of transform wall both sides) but TanStack
combined is bigger (Vite 56%, OJ ~62%: import-protection alone 38.5% on OJ).
Tailwind transform ≤0.5s. Baseline RSS at parity (OJ slightly worse:
warm 587–606 MB vs Vite 567–575 MB). OJ tsx edit 9.7–12.4s warm vs Vite
0.1–0.6s; OJ transform wall total 73.8s vs Vite 3.49s for boot+crawl, per-call
~10x. Dominant OJ costs are structural: per-call host overhead and full
re-transform of both envs per tsx edit. Native ports must therefore be
judged on bypassing host overhead + RSS effect, not plugin share alone.
T1c diagnoses RSS composition + per-call overhead before any port.

## Tasks

Work in a separate git worktree (`mktemp -d`, copy needed `.env`, install).
Each task: smallest correct change, targeted tests only (name them in the
task), no full e2e suite. Use libraries already in `package.json`/
`Cargo.toml` first; new deps need justification in the task return.

### T1 — Benchmark harness + baseline

- Add a reproducible bench (script under `e2e/` or `bench/`, following
  existing e2e conventions) that, for the intersolaris app (read-only use)
  and/or an OJ Solid fixture, measures for **Vite** and **OJ hosted**:
  cold-boot time to first 200, RSS after crawling all routes (settled,
  same procedure both sides), single `.tsx` edit → updated response
  latency, CSS-only edit latency.
- Per-plugin transform wall-time + call counts (OJ host timing and/or Vite
  debug timing), split by plugin name.
- Output one JSON + short markdown table. No production code changes
  except opt-in timing instrumentation behind an env flag, off by default.
- Verify: harness runs end-to-end twice with stable (±10%) results.

### T2 — Tailwind: eliminate double work

- If OJ native tailwind engine is active for an app, skip `@tailwindcss/vite`
  in the plugin host (native-plugins classification, gated on the same
  condition as `app_uses_tailwind`); never run both for the same CSS.
- Test: generated CSS for a fixture (utilities incl. arbitrary values,
  `@apply`-free v4 syntax used by intersolaris) matches hosted-plugin
  output byte-for-byte or explains diffs; existing css_engine tests pass.

### T3 — `solidAiSrcPlugin` native (only if T1 shows non-trivial cost)

- Native OJ dev transform inserting `data-ai-src="rel:line:col"` using
  OJ's existing JSX parsing — no `@babel/parser`. Match the JS plugin's
  gating: dev/serve only, `.tsx/.jsx` only, skip node_modules.
- Skip the hosted plugin when the native transform is on.
- Test: attribute values match JS plugin output on fixtures; absent in build.

### T4 — `llmsTxt()` parity (no native port)

- App-local `configureServer` plugin stays hosted. Add/keep an e2e test:
  `GET /llms.txt` under OJ returns the app's rendered content in dev.
- No OJ code change expected; report if it already passes.

### T5 — Solid transform via SWC (only if T1 confirms babel dominance)

- Spike first: compile a fixture corpus (components, control flow,
  hydration/ssr variants from existing Solid tests) with the SWC Solid
  transform vs `babel-preset-solid`; diff output semantics via existing
  Solid SSR/hydration tests. Stop and report if parity fails.
- If parity holds: use SWC path in OJ's Solid transform (dev serving +
  `solid-transform.mjs` Start bundles), skip hosted `vite-plugin-solid`
  transform; keep its `config` contributions (resolve conditions,
  `solid`-first) natively. Preserve `/@solid-refresh` behavior or document
  its absence with reload latency measured.
- Tests: existing Solid adapter/SSR tests + new transform snapshot tests.

### T6 — TanStack remainder assessment

- From T1 numbers, decide on hosted `import-protection` + route-splitting.
  Default: leave hosted (low gain, high parity risk). Only port if T1
  shows >5% of total CPU/RSS. Add a regression test either way for
  server-fn import protection under OJ.

### T7 — Final measurement + keep/revert

- Re-run T1 harness: Vite vs OJ-with-native-winners, same procedure.
- Report table: RSS (post-crawl, settled), cold boot, `.tsx` edit, CSS edit.
- If median RSS saving ≥20% and edits not slower: keep OJ, proceed to
  commits/merge. If not: switch the four leo apps back to their Vite
  systemd units (definitions kept for rollback), verify all pages 200,
  and report which OJ changes (if any) are still worth merging for parity.

## Verification gates

- Targeted Rust tests for touched crates (`cargo test -p <crate> <filter>`),
  targeted `e2e/unit` node tests for host changes.
- Full repo gates + e2e once at the end (main agent coordinates), not per task.
- Every perf claim needs harness output from the same run; no claim from
  source inspection alone.

## Status

- [x] T1 benchmark harness + baseline (worktree `/tmp/tmp.sPkckODBNe/oj-native`; harness `e2e/native-solid-bench.mjs`, fix cache state = warm for T7)
- [x] T1c diagnosis: OJ retained RSS ~346 MB + ~260 MB uncollected V8 garbage (4 GB plugin-host heap cap); per-call 10x was a call-count artifact (babel at parity in OJ isolate); real excess = client resolve amplification (`"{}"` resolved map → import-protection 112 ms/call), SSR `engine.reload()` full re-import per edit, client settle full-build fallback; tailwind does NOT double-compile (hosted `pre` wins, native isolate idle). Native-port RSS ceilings: solid+ai-src together 25–50 MB, TanStack remainder 5–15 MB.
- [x] T-A heap cap default 4096→512 MB per isolate (`plugins/host.rs:436`): settled RSS 602→453 MB vs Vite 578 MB (−21.6%), boot/css/tsx timings unchanged; `OJ_PLUGIN_MEMORY_MB`/`NODE_OPTIONS` overrides kept, near-limit failure is graceful + revive
- [x] T-B SSR plain edits → `revalidate_for` (+ versioned Start runtime seam so router-entry cache invalidates in-isolate, mirroring Vite noExternal): tsx edit 8583→5554 ms, SSR transforms/write ~91→~19; server-fn + regen changes still full `reload()`
- [x] T-C bundle pre-resolved import map (files-only, `bundlerResolve` fallback on miss; importer guard added): import-protection client 85.6→45.5 ms/call (share 35.7%→16.4%), tsx edit 5554→4512 ms, RSS unchanged. Remainder: non-file resolves still RPC + GC pressure. New flag: solid/ai-src/code-splitter transforms 13–18x Vite on both envs, not GC/resolve — needs own diagnosis before T5
- [x] T-D root cause was cold `HotSession` after warm-cache boot (settle itself healthy; T1c suspects disproven). Background warmup on cache restore: bench tsx edit 4512→1180 ms, connected-page first edit hot Patch ~1.3s (1–3 client transforms). Costs: boot 4.5→8.9s (warmup contention), post-crawl RSS 455→537 MB (engine resident; like-for-like post-edit RSS 558→~530 MB — T7 must sample post-edit both sides). Residual: css edits still `trigger_full_build` (globals.css unknown to engine, background only) → T-E scope
- [x] T-Dx babel diagnosis: true per-call CPU at parity (0.9–1.0x Vite, byte-identical in/out). 13–18x was wall-clock attribution under up-to-47 concurrent transforms on one isolate + dual-env volume + 145 duplicate same-module re-runs (no singleflight). OJ throughput actually better (20-parallel sweep 419 vs 1037 ms wall)
- [x] T-F singleflight in `plugin-host.mjs` transform dispatch (in-flight only, key = container+env+ssr+chain+phase+sha256(id,code,resolved,prior); errors propagate to all waiters): 63 duplicate executions eliminated in bench run, tests 3/3 new + 24/24 existing; single-run bench deltas within machine noise, RSS flat 535 MB
- [x] T-E lazy `LazyCssHost` (spawn on first fallback compile only; relink check non-spawning), style unknowns no longer force engine full build (verdict/reload unchanged), `native-css` rows in OJ_PLUGIN_TIMING: edit-phase client calls 665→12, native css 0 calls at boot/crawl; suites green (css_engine 8/8, start_bundle 88, start_dev 54)
- [ ] T4 llmsTxt parity test (hosted, no port) — folded into T7 verification
- [-] T5 SWC Solid — cancelled: T-Dx shows gap is scheduling, not babel CPU (true babel ~15–20% of wall; inflation is plugin-agnostic)
- [-] T3 ai-src native — cancelled with T5 (~0 standalone)
- [ ] T6 TanStack remainder — do not port (T-C addresses its cost)
- [x] T7 final measurement (browser-driven both sides, warm caches, 3 runs, medians): post-edit settled RSS OJ 527 MB vs Vite 475 MB (**+11% — RSS criterion FAILS**, needed ≤380); tsx browser-visible OJ 293 vs 572 ms (pass) but served-SSR 1857 vs 317 ms; css browser 806 vs 648 ms (fail); boot 9.0 vs 6.3s. Gates: 1605 cargo tests pass (1 known flaky), node unit failures all pre-existing, clippy all pre-existing. **Decision per keep rule: switch apps back to Vite; merge OJ fixes into `solid-support` for the record** (they are real OJ improvements: tsx edit ~10s→1.9s served / 0.3s browser, post-crawl RSS 602→455 under curl procedure)
- [x] T8 merged into `solid-support` (`5a444903`, pushed; worktree removed, `backup-nsp` kept). All 4 leo apps reverted to Vite units and verified (routes 200, units active/enabled, NRestarts=0; akademie-devtest got a new Vite unit modeled on its OJ one; OJ units/binary left in place)
