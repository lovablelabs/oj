# OJ upstream merge + Vite replacement rollout (leo-server)

## Goal

1. Merge `upstream/main` (lovablelabs/oj, v0.4.5, 260 commits ahead) into fork branch
   `solid-support` (david1gp/oj, 20 fork-only commits: SolidJS/Solid-Start support),
   preserving all Solid support. Push to `origin/solid-support`.
2. Answer with evidence: does OJ work as a Vite alternative for SolidJS incl. SSR?
   Do all our Vite plugins work? How much memory is saved per dev server?
3. Switch Vite dev servers on `leo@leo-server` to OJ: pilot intersolaris, then
   abikur, betaconsulting, akademie (devtest only; akademie preview/prod stay Vite).
4. Fix OJ bugs found along the way (incl. unrelated errors hit during testing),
   rebuild release binary, rsync to leo-server, retest. Repeat until green.

## Decisions (user, 2026-11-07)

- Merge into `solid-support`, push to origin. Fork `main` untouched.
- Pilot: intersolaris first (currently down, no service — zero downtime risk).
- Rollout: switch systemd units to OJ. Add a NEW OJ service definition file in each
  project's `ops/` dir (e.g. `ops/<project>-oj.service`) alongside the existing Vite
  one so switching back = reinstall the Vite unit from `ops/`. Do not delete Vite
  definitions. No separate backup of installed units needed.
- akademie: only the HMR dev server (`akademie-devtest`, port 3141) switches.
  `akademie.service` (3120) and `akademie-prod.service` (3122) use `vite preview`
  and stay on Vite.

## Context (verified 2026-11-07)

- OJ: Rust workspace, `cargo build -p adaptive-oj` (release for deploy), binary `oj`,
  `oj dev [root] [--port] [--host]`, default port 5199. Vite-plugin compat via Node
  plugin host; native TanStack Start (React+Solid) host; Solid SSR via
  `solid-transform.mjs` (babel-preset-solid). Lint gate: `make` targets run
  `cargo fmt --check` + `cargo clippy --workspace --all-targets -D warnings`.
- Merge risk HIGH: 131 files changed both sides; upstream restructured
  `crates/oj/src/build.rs` into `crates/oj/src/build/*`. Merge base `5a0272c5`.
- All 4 projects: TanStack Start (Solid) + Vite 8.3.5, `solid({ssr:true})`,
  prerender on. Plugins to verify per project: `@tailwindcss/vite`,
  `tanstackStart(...)`, `vite-plugin-solid`, `solidAiSrcPlugin()` (ai-src),
  local `llmsTxt()`, abikur `tailwindStandaloneCssStubPlugin()` + `visualizer()`,
  betaconsulting local `v9Api()` middleware.
- Server: node v26.12.1, bun 1.4.3, 117 GiB RAM (55 GiB available).
  Vite dev RSS baseline: abikur ~465 MB, betaconsulting ~478 MB, akademie-devtest
  ~520 MB. Ports: abikur 3103, intersolaris 3016, betaconsulting 3104,
  akademie-devtest 3141. All loopback-bound behind Caddy (preview.* domains).
- Units: `bun run dev` today. intersolaris unit exists only as `ops/` template.
- Existing libraries first: use what is already in each `package.json`; do not add
  new deps to the web projects. OJ fixes go in the OJ repo.

## Tasks

- [x] T1 Merge `upstream/main` into `solid-support` in an isolated worktree; (merge commit `40a2f400`, solid-start e2e dev+prod PASS; known follow-up: CF-worker bundle lacks solid-first conditions — no fixture, not needed for leo-server)
      resolve conflicts keeping Solid support; `cargo build -p adaptive-oj`
      + targeted tests (solid/start unit tests, `node e2e/solid-start.mjs`,
      `node e2e/solid.mjs`); report conflicts resolved. No push yet.
- [x] T2 Push merged `solid-support` to origin (after main-agent check of T1). (pushed `40a2f400`; release binary `oj 0.4.5` 188 MB rsynced to `leo@leo-server:~/.local/bin/oj`, x86_64 both sides, verified)
      Build release binary (`cargo build --release -p adaptive-oj`), rsync to
      `leo@leo-server:~/.local/bin/oj`, verify `oj --version`/help on server.
- [x] T3 Pilot intersolaris: GO. Bugs fixed in T6 (004d84cf, 8227a491). Re-crawl: 45/45 routes 200, SSR real content, tailwind/ai-src/llmsTxt/tanstackStart/vite-plugin-solid all PASS, headless-chromium hydration PASS (0 console errors), HMR ~11 s. `/sitemap.xml` 404 = Vite parity. RSS: oj-debug ~620 MB vs vite ~528 MB — re-measure with release binary in T4/T7 before any memory claim. Original scope: crawl all
      routes from route tree/sitemap/prerender pages with curl, compare HTTP
      status + SSR HTML sanity vs expectations; test HMR edit; measure RSS vs
      Vite baseline class (~470 MB). List per-plugin works/broken. Fix errors
      encountered (project-side config fixes allowed; OJ bugs → T6).
- [x] T4 Install intersolaris OJ unit from new `ops/intersolaris-oj.service` (live under leo on 3016, public via intersolaris.wonder1.de; preview.intersolaris.de = missing Tailscale dnsmasq record, root-owner fix documented)
      (port 3016), verify preview domain + all pages via curl/browser.
- [x] T5 Roll out GO: abikur (3103, 9/9 routes 200), betaconsulting (3104, 15/15 + /v9 API incl. GET-with-body, no desync), akademie-devtest (3141, 12/12 with Vite-dev parity) — all live under OJ systemd units, verified by T7 crawl. Original scope: abikur (3103), betaconsulting (3104), akademie-devtest (3141)
      in parallel where independent: spare-port page crawl first, then add
      `ops/<project>-oj.service`, switch unit, verify pages + RSS. betaconsulting
      must include `/v9*` API checks.
- [x] T6 OJ fix loop: 3 iterations done, all pushed — server-entry export + SSR stylesheet classify + --host parsing (004d84cf), virtual-importer client-bundle resolution (8227a491), plugin-middleware GET-body bridge desync (7478739b). Original scope: fix in OJ repo on
      `solid-support`, targeted test, release build, rsync, restart affected
      unit(s), re-verify failing pages. Commit + push each fix.
- [x] T7 Final verification GO all 4 projects: crawls 45+9+15+12 all 200 (akademie thin-SSR = Vite-dev parity, proven on spare port), hydration 0 errors, HMR ~8 s, public 200s; gates fmt clean, clippy 5 pre-existing, e2e PASS, server suite 385 pass + 2 order-dependent pre-existing failures. Original scope: all pages of all 4 projects, memory table
      before/after, plugin matrix, `cargo test` relevant subset + fmt/clippy on
      touched crates. Cleanup worktrees/temp branches/spare-port servers.

## Verification rules

- Subagents run only targeted tests named in their task; full e2e suite only in T7.
- Page checks hit the real HTTP interface (curl every route; browser check for
  hydration on at least the homepage + one nested route per project).
- Memory: report systemd Memory + process RSS before/after per project.
