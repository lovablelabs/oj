# Upstream mining report

Source: `facebook/stylex` tag 0.19.0, commit `5f51b24444abced04b213726977b9d67339bb26d` (MIT).
Mined inputs are differential test data only — never shipped.
Regenerate with `node src/mine-upstream.mjs` then `node src/mine-values.mjs`.

## Babel-plugin test files

| Test file | Mined | Skipped | Dedup | Oracle ok/error | Notes |
| --- | ---: | ---: | ---: | --- | --- |
| evaluation-import-test.js | 10 | 0 | 0 | 8/2 | — |
| shared-stylex-nested-transforms-test.js | 0 | all | — | — | skipped: calls src/shared transform functions directly — no plugin transforms to mine |
| stylex-nested-utils-test.js | 0 | all | — | — | skipped: tests internal flatten/unflatten utils directly — no plugin transforms to mine |
| transform-import-export-test.js | 15 | 0 | 1 | 15/0 | — |
| transform-polyfills-test.js | 3 | 0 | 0 | 3/0 | — |
| transform-process-test.js | 7 | 0 | 9 | 7/0 | — |
| transform-stylex-create-test.js | 100 | 0 | 7 | 96/4 | — |
| transform-stylex-createTheme-test.js | 19 | 0 | 2 | 18/1 | — |
| transform-stylex-createThemeNested-test.js | 10 | 0 | 0 | 7/3 | — |
| transform-stylex-defineConsts-test.js | 23 | 2 | 1 | 22/1 | 2× aliases embed fixture-dir globs; <ROOT> substitution is exact-string only |
| transform-stylex-defineConstsNested-test.js | 8 | 0 | 0 | 6/2 | — |
| transform-stylex-defineMarker-test.js | 2 | 0 | 0 | 2/0 | — |
| transform-stylex-defineVars-test.js | 27 | 0 | 0 | 27/0 | — |
| transform-stylex-defineVarsNested-test.js | 29 | 0 | 0 | 23/6 | — |
| transform-stylex-keyframes-test.js | 5 | 0 | 1 | 5/0 | — |
| transform-stylex-positionTry-test.js | 4 | 0 | 0 | 4/0 | — |
| transform-stylex-props-test.js | 93 | 0 | 0 | 93/0 | — |
| transform-stylex-viewTransitionClass-test.js | 5 | 0 | 0 | 5/0 | — |
| transform-stylex-when-test.js | 13 | 0 | 0 | 9/4 | — |
| transform-value-normalization-test.js | 17 | 0 | 0 | 17/0 | — |
| validation-import-export-test.js | 7 | 0 | 0 | 7/0 | — |
| validation-stylex-create-test.js | 114 | 0 | 1 | 86/28 | — |
| validation-stylex-createTheme-test.js | 12 | 0 | 0 | 3/9 | — |
| validation-stylex-defineConsts-test.js | 20 | 0 | 0 | 5/15 | — |
| validation-stylex-defineMarker-test.js | 8 | 0 | 0 | 2/6 | — |
| validation-stylex-defineVars-test.js | 27 | 0 | 0 | 8/19 | — |
| validation-stylex-keyframes-test.js | 7 | 0 | 0 | 4/3 | — |
| legacy/stylex-transform-call-test.js | 58 | 0 | 0 | 58/0 | — |
| legacy/stylex-transform-legacy-shorthands-test.js | 46 | 0 | 0 | 44/2 | — |
| legacy/transform-legacy-polyfills-test.js | 42 | 0 | 0 | 42/0 | — |
| legacy/transform-logical-properties-test.js | 90 | 0 | 0 | 90/0 | — |
| legacy/transform-logical-values-test.js | 29 | 0 | 0 | 29/0 | — |
| legacy/transform-pre-plugin-test.js | 0 | all | — | — | skipped: runs a custom pre-plugin in the babel pipeline — not representable in the single-plugin job schema |

Total mined jobs: **850** across 30 jobs files.
Skipped call sites: 2. Deduplicated repeats: 22.
Template-literal sources resolved via static scope evaluation: 75.
Flow-stripped sources (raw kept in raw-sources.json): 0.
Oracle-error pins whose message classified as `unknown`: 8 (differ compares class, so unknown-vs-unknown still gates on outcome).

## Adapter table

Each upstream file wraps `transformSync` in a local helper with its own option
defaults; the `ADAPTERS` table in `src/mine-upstream.mjs` mirrors those defaults
(defaults spread under call-site opts, forced overrides spread over them,
per-helper default filenames). Multi-stage helpers are not statically mined:
the miner extracts them verbatim and runs them in a vm over the real
0.19.0 plugin, capturing the final transform invocation:

- `transform-process-test.js` `transform` — compiles two token files, splices
  their output above the test source, recompiles the composite.
- `transform-stylex-createTheme{,Nested}-test.js` `transformWithFixture` —
  inlines compiled `defineVars` output above the fixture.
- `transform-import-export-test.js` `createStylesFixture` — string factory
  that embeds compiled `defineConsts`/`defineVars` output.

## Known gaps

- Upstream `filename`/`rootDir` pairs are relativized into the fixture root;
  the exact-string `<ROOT>` placeholder cannot express alias globs like
  `~fixture/* → <dir>/*`, so `transformWithAliasedInlineConsts` sites are skipped.
- Tests that mock `@dual-bundle/import-meta-resolve` (evaluation-import,
  defineVarsNested cross-package) replay against real resolution; their pins
  record the resolution failure the oracle produces without the mock.
- `expect` pins are the observed oracle outcome, not the upstream assertion;
  `babel-runner` warns when a pin goes stale against a newer oracle.
- Upstream plugin options received the helper `filename` key via spread; the
  plugin ignores it, and the miner drops it from job options.
- 59 `describe.skip`/`test.skip` inputs assert
  toThrow but the live oracle accepts them (or vice versa) — upstream skipped
  them for that reason; pins follow the oracle and jobs carry the
  `upstream-skipped` tag.

<!-- values-section-start -->
## style-value-parser value/media-query mining

Raw strings harvested from `packages/style-value-parser/src/**/__tests__`
(args of `parse`/`parseToEnd`/`validateMediaQuery` plus `@media` string
literals) into `values.json` / `media-queries.json`, then wrapped as
`stylex.create()` jobs. Each media query is emitted twice: default options
(enableMediaQueryOrder defaults true) and `enableMediaQueryOrder: false`.

Non-static skips are parse args that are identifiers or templates; in the
at-queries files those reference `const input = '@media …'` locals whose
strings the `@media` literal harvest already captured.

| Test file | Harvested | Non-static skipped | Notes |
| --- | ---: | ---: | --- |
| __tests__/token-parser-test.js | 38 | 0 | regex fallback (Flow `implies` unparseable) |
| at-queries/__tests__/media-query-transform-test.js | 140 | 0 | — |
| at-queries/__tests__/parse-media-query-test.js | 258 | 86 | — |
| at-queries/__tests__/validation-media-query-test.js | 39 | 0 | — |
| css-types/__tests__/alpha-value-test.js | 24 | 0 | — |
| css-types/__tests__/angle-percentage-test.js | 11 | 0 | — |
| css-types/__tests__/angle-test.js | 13 | 0 | — |
| css-types/__tests__/blend-mode-test.js | 19 | 0 | — |
| css-types/__tests__/calc-test.js | 34 | 1 | — |
| css-types/__tests__/color-test.js | 27 | 0 | — |
| css-types/__tests__/custom-ident-test.js | 18 | 0 | — |
| css-types/__tests__/dashed-ident-test.js | 8 | 0 | — |
| css-types/__tests__/easing-function-test.js | 21 | 0 | — |
| css-types/__tests__/filter-function-test.js | 12 | 0 | — |
| css-types/__tests__/flex-test.js | 6 | 0 | — |
| css-types/__tests__/frequency-test.js | 11 | 0 | — |
| css-types/__tests__/length-percentage-test.js | 11 | 0 | — |
| css-types/__tests__/length-test.js | 29 | 0 | — |
| css-types/__tests__/position-test.js | 24 | 0 | — |
| css-types/__tests__/resolution-test.js | 6 | 0 | — |
| css-types/__tests__/time-test.js | 11 | 0 | — |
| css-types/__tests__/transform-function-test.js | 150 | 0 | — |

Distinct values: 455 → 455 jobs (values.jobs.json, oracle ok/error 454/1).
Distinct media queries: 248 → 496 jobs (media-queries.jobs.json, oracle ok/error 447/49).

Error pins whose oracle message classified as `unknown` (mostly inputs that
crash the plugin internally, e.g. an empty-string value): upstream/value-token-parser-026.

Wrapper property per category (miscategorization is harmless — the gate is
differential, not CSS validity):

```json
{
  "token-parser": "width",
  "alpha-value": "opacity",
  "angle": "rotate",
  "angle-percentage": "rotate",
  "blend-mode": "mixBlendMode",
  "calc": "width",
  "color": "color",
  "custom-ident": "willChange",
  "dashed-ident": "transitionProperty",
  "easing-function": "transitionTimingFunction",
  "filter-function": "filter",
  "flex": "flex",
  "frequency": "width",
  "length": "width",
  "length-percentage": "width",
  "position": "backgroundPosition",
  "resolution": "width",
  "time": "transitionDuration",
  "transform-function": "transform",
  "media-query-transform": "width",
  "parse-media-query": "width",
  "validation-media-query": "width"
}
```
<!-- values-section-end -->
