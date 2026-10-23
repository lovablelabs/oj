# Upstream assertion mining report

Source: `facebook/stylex` tag 0.19.0, commit `5f51b24444abced04b213726977b9d67339bb26d` (MIT).
Each job carries upstream's own committed expectation in `assert`, not an
oracle-observed pin. Regenerate with `node src/mine-assertions.mjs`.

## Extracted assertions

| Test file | code | metadata | rules | throws | noThrow | Not extracted |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| evaluation-import-test.js | 10 | 0 | 8 | 0 | 0 | 12 |
| transform-import-export-test.js | 14 | 3 | 0 | 0 | 0 | 12 |
| transform-polyfills-test.js | 0 | 0 | 0 | 0 | 0 | 3 |
| transform-process-test.js | 5 | 0 | 0 | 0 | 0 | 22 |
| transform-stylex-create-test.js | 101 | 92 | 0 | 0 | 0 | 9 |
| transform-stylex-createTheme-test.js | 19 | 19 | 0 | 0 | 0 | 4 |
| transform-stylex-createThemeNested-test.js | 4 | 0 | 3 | 3 | 0 | 3 |
| transform-stylex-defineConsts-test.js | 21 | 7 | 6 | 0 | 0 | 7 |
| transform-stylex-defineConstsNested-test.js | 6 | 0 | 4 | 2 | 0 | 0 |
| transform-stylex-defineMarker-test.js | 2 | 1 | 0 | 0 | 0 | 0 |
| transform-stylex-defineVars-test.js | 27 | 27 | 0 | 0 | 0 | 0 |
| transform-stylex-defineVarsNested-test.js | 32 | 0 | 6 | 3 | 0 | 24 |
| transform-stylex-keyframes-test.js | 6 | 6 | 0 | 0 | 0 | 0 |
| transform-stylex-positionTry-test.js | 4 | 4 | 0 | 0 | 0 | 0 |
| transform-stylex-props-test.js | 93 | 0 | 0 | 0 | 0 | 0 |
| transform-stylex-viewTransitionClass-test.js | 4 | 4 | 0 | 0 | 0 | 2 |
| transform-stylex-when-test.js | 9 | 0 | 6 | 4 | 0 | 0 |
| transform-value-normalization-test.js | 17 | 0 | 0 | 0 | 0 | 0 |
| validation-import-export-test.js | 0 | 0 | 0 | 0 | 7 | 0 |
| validation-stylex-create-test.js | 0 | 0 | 0 | 86 | 28 | 7 |
| validation-stylex-createTheme-test.js | 0 | 0 | 0 | 9 | 3 | 0 |
| validation-stylex-defineConsts-test.js | 0 | 0 | 0 | 14 | 6 | 0 |
| validation-stylex-defineMarker-test.js | 0 | 0 | 0 | 6 | 2 | 0 |
| validation-stylex-defineVars-test.js | 0 | 0 | 0 | 19 | 8 | 0 |
| validation-stylex-keyframes-test.js | 0 | 0 | 0 | 2 | 4 | 1 |
| legacy/stylex-transform-call-test.js | 58 | 0 | 0 | 0 | 0 | 0 |
| legacy/stylex-transform-legacy-shorthands-test.js | 44 | 0 | 0 | 2 | 0 | 0 |
| legacy/transform-legacy-polyfills-test.js | 0 | 42 | 0 | 0 | 0 | 0 |
| legacy/transform-logical-properties-test.js | 90 | 0 | 0 | 0 | 0 | 0 |
| legacy/transform-logical-values-test.js | 29 | 0 | 0 | 0 | 0 | 0 |

Total assertion jobs: **1041** across 29 jobs files.
Assertion sites not extracted: 106.
Inputs also present in `corpus/upstream` (extraction cross-check): 1041/1041.

## Reasons an assertion site was not extracted

| Count | Reason |
| ---: | --- |
| 15 | toMatchInlineSnapshot: subject not traceable to a transform (stylexPlugin.processStylexRules(metadata) |
| 13 | toContain: subject not traceable to a transform (allLtr) |
| 11 | toEqual: toEqual argument is not a rules array |
| 6 | toMatchInlineSnapshot: subject not traceable to a transform (expectedVarName) |
| 5 | toContain: expectedVarName is not defined |
| 5 | toMatchInlineSnapshot: inline snapshot never written upstream |
| 5 | toBeGreaterThan: unsupported subject path "metadata.stylex.length" with toBeGreaterThan |
| 4 | input: option `options.env.colorMix` is a function, not JSON-serializable |
| 4 | helper skipped: aliases embed fixture-dir globs; <ROOT> substitution is exact-string only |
| 2 | toMatchInlineSnapshot: subject not traceable to a transform (output1) |
| 2 | toMatchInlineSnapshot: subject not traceable to a transform (output2) |
| 2 | toMatchInlineSnapshot: subject not traceable to a transform (output3) |
| 2 | input: option `options.env.shadow` is a function, not JSON-serializable |
| 2 | input: option `options.env.opacity` is a function, not JSON-serializable |
| 2 | input: option `options.env.override` is a function, not JSON-serializable |
| 2 | toEqual: negated toEqual on code |
| 2 | toEqual: negated rules assertion |
| 2 | toContain: subject not traceable to a transform (consumerCss) |
| 2 | toMatch: subject not traceable to a transform (allLtr) |
| 2 | toHaveBeenCalled: subject not traceable to a transform (consoleSpy) |
| 1 | toThrow: subject not traceable to a transform (transformation) |
| 1 | toMatchInlineSnapshot: subject not traceable to a transform (expectedImportTestMetadata) |
| 1 | toThrow: function subject with no helper call |
| 1 | toMatchInlineSnapshot: subject not traceable to a transform (rules) |
| 1 | toBeNull: subject not traceable to a transform (varHashMatch) |
| 1 | toBeDefined: subject not traceable to a transform (overrideCSS) |
| 1 | toContain: subject not traceable to a transform (overrideCSS[1].ltr) |
| 1 | toMatchInlineSnapshot: code snapshot is not a quoted string |
| 1 | toBe: subject not traceable to a transform (unique.size) |
| 1 | toBeGreaterThan: subject not traceable to a transform (producerVars.length) |
| 1 | toBeFalsy: subject not traceable to a transform (result) |
| 1 | toMatchInlineSnapshot: subject not traceable to a transform (result?.code) |
| 1 | toMatchInlineSnapshot: subject not traceable to a transform (result?.metadata?.stylex) |
| 1 | toThrow: negated toThrow with an argument |
| 1 | toHaveBeenCalledWith: subject not traceable to a transform (consoleSpy) |
| 1 | toThrow: subject not traceable to a transform (callTransform) |

## Assertion shapes

- `code` — `expect(code).toMatchInlineSnapshot()` on the transformed program.
- `metadata` / `rules` — snapshot or `toEqual` on `metadata` / `metadata.stylex`.
- `throws` — `toThrow()` (any), `toThrow(string)` (substring), `toThrow(/re/)`,
  or `toThrowErrorMatchingInlineSnapshot()` (exact message, code frame included).
- `noThrow` — `.not.toThrow()`.
