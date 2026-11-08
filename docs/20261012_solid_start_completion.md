# Complete Solid Start support

## Goal

Complete the existing SolidJS fork (`david1gp/oj`, branch `solid-support`). Both plain Solid and TanStack Solid Start must work in development and production. Verify the real application at `/home/david/personal/homepage` without changing its tracked files.

## Current context

- Existing implementation and roadmap: `SOLID.md`.
- Solid Start production renders correctly; development currently serves head and an empty body that client rendering masks.
- Confirmed development SSR cause: StartHost drops `?tsr-split=component` before plugin load/transform hooks, producing the reference module instead of the split component export. Preserve queried plugin identity while keeping clean filesystem/compiler paths.
- Plain Solid has browser coverage; Solid Start needs a dedicated regression fixture that asserts raw SSR content as well as browser hydration.
- The homepage reference behavior includes complete SSR content and its route-specific title. Development acceptance must check both, rather than accepting a visible page after client rendering.
- Solid-first export conditions are already preserved; a focused merge/resolver regression now pins that behavior.
- Framework-caught route errors render a meaningful Solid Router error boundary, potentially with HTTP 200. Do not impose 500 on handled errors; separately verify genuinely uncaught handler errors.
- The existing Start forwarding path already converts truly uncaught handler/body-read errors to HTTP 500. No error-status implementation change is needed.
- The new fixture's reference-Vite hydration correction is being verified before it becomes an acceptance gate.
- Query-preservation restores complete development SSR. Development hydration fails because client bundling skips TanStack route-splitting transforms while SSR keeps lazy wrappers, changing Solid hydration context topology. Preserve router reference/split/shared transforms before Solid compilation.
- Development SSR freshness fails because the unversioned external TanStack runtime caches the previous router entries inside the isolate. Reset the Start isolate on effective reload/revalidation without modifying framework packages.
- Persistent Start client bundle keys must include embedded builder/bridge/transform inputs; otherwise an existing homepage cache can restore pre-fix output across binary rebuilds. Add content-based invalidation rather than deleting user caches.
- Client split chunks and the HTML must import the same emitted `client-entry.js`; enforce output naming rather than copying a differently named entry and executing Solid twice.
- Tailwind CSS is correctly expanded and byte-identical to Vite; failed hydration removes its stylesheet. No client CSS implementation fix is warranted.
- Production acceptance passes, including all nine homepage routes and the dedicated fixture's hydration, reactivity, SPA navigation, and render-error fallback.

## Decisions and scope

- Keep the existing Rust/embedded-engine development architecture. Choose the smallest correct fix based on reproduction, not a replacement pipeline.
- Use existing dependencies first. Preserve React Start and Svelte behavior.
- Do not stash, reset, clean, or delete unrelated files. Preserve homepage tracked-file cleanliness; use scratch copies for deliberate source edits.
- Pin raw body rendering, hydration/reactivity, route/head behavior, edit/rebuild behavior, production rendering, and SSR error handling with targeted tests.
- Confirm Solid export conditions with a narrow regression if coverage is missing.
- Run narrow checks during implementation. Run browser/e2e verification after the relevant increment is ready, not after every minor edit.

## Tasks and status

1. **Complete:** reproduce missing development SSR and identify query loss before plugin transforms as its cause.
2. **Complete:** queried IDs, Solid router lifecycle/transforms, canonical client chunk identity, safe isolate reset, and embedded-script cache fingerprints are implemented with focused regressions.
3. **Complete:** dedicated dev and production acceptance passes for raw SSR/head, hydration/reactivity, SPA identity, loader/default-render errors, and route/imported-component edit restoration. CI installation and explicit execution are wired.
4. **Complete:** pin Solid condition propagation and actual native resolver selection, including queried package imports.
5. **Complete:** real homepage development/production, desktop/mobile visual review, targeted cross-framework acceptance, documentation, and independent diff review are complete. The reviewed runtime, regression/CI, and documentation commits are prepared for publication to the fork branch.

## Acceptance

- `oj dev` renders real route content in the initial HTTP response, not just after client JavaScript executes.
- Hydration is clean and interactions and development updates work.
- `oj build` plus `node server.mjs` serves complete SSR content and correct head ordering with clean hydration.
- A throwing render is observable as an error rather than a silent empty successful response, consistent with the framework's error behavior.
- Healthy-route assertions require actual body elements after stripping script payloads. A default global render-error boundary may legitimately return a fragment without a document shell; test its meaningful fallback markup separately from loader-error boundaries.
- Development SPA navigation retains document identity, and watched edits update both raw SSR and the hydrated browser.
- Homepage development styles match the reference Vite output, including Tailwind utilities.
- Solid-specific regression coverage fails for an empty SSR body.
- Existing targeted React Start, plain Solid, and Svelte checks remain passing.
- Homepage tracked files remain unchanged and the fork contains no unintended deletions.
