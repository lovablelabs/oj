use std::io::Write;
use std::process::{Command, Stdio};

fn run_cli(args: &[&str], input: &str) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fru"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stylex-rs");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait for stylex-rs");
    (
        output.status.code().expect("exit code"),
        String::from_utf8(output.stdout).expect("utf8 stdout"),
        String::from_utf8(output.stderr).expect("utf8 stderr"),
    )
}

fn reprint(input: &str) -> String {
    let (code, stdout, stderr) = run_cli(&["reprint"], input);
    assert_eq!(code, 0, "reprint failed: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");
    stdout
}

const SAMPLE_JSX: &str = r#"
import * as React from "react";
export function Badge({ label, tone }: { label: string; tone: "info" | "warn" }) {
  return (
    <span className={tone} data-tone={tone} aria-label={label}>
      {label.toUpperCase()}
      <em>{tone === "warn" ? <b>!</b> : null}</em>
    </span>
  );
}
"#;

const SAMPLE_TS_TYPES: &str = r#"
export interface Style<T extends string = string> {
  readonly kind: T;
  props?: Partial<Record<"color" | "margin", string | number>>;
}
type Pair<A, B> = [A, B];
export const enum Axis { X = "x", Y = "y" }
export function pick<K extends keyof Style>(s: Style, k: K): Style[K] {
  return s[k];
}
const p: Pair<number, Axis> = [1, Axis.X] as const as unknown as Pair<number, Axis>;
export default p satisfies Pair<number, Axis>;
"#;

const SAMPLE_TEMPLATES: &str = r#"
const size = 12;
const unit = "px";
export const css = `margin: ${size}${unit}; content: "${`${size + 1}`}";`;
const tagged = String.raw`line1\n${size > 0 ? `pos-${size}` : "neg"}`;
export const multi = `a
b ${tagged} c`;
"#;

const SAMPLE_COMPUTED_KEYS: &str = r#"
const key = "dynamic";
const sym = Symbol("s");
export const styles = {
  [key]: { color: "red" },
  [`${key}-hover`]: { color: "blue" },
  [sym]: true,
};
export class Box {
  [key]: string = "v";
  static [`${key}Static`]() {
    return 1;
  }
}
"#;

const SAMPLE_DECORATORS: &str = r#"
function sealed(target: unknown) {}
function readonly(target: unknown, ctx: unknown) {}
@sealed
export class Widget {
  @readonly name: string = "widget";
  @readonly
  render(): string {
    return this.name;
  }
}
"#;

const SAMPLE_UNICODE: &str = r#"
export const greetings = {
  de: "Grüße",
  jp: "こんにちは世界",
  emoji: "🎨 ✨\u{1F680}",
  mixed: `Ångström — ${"π".repeat(2)}`,
};
export const rtl = "مرحبا";
"#;

const SAMPLES: [&str; 6] = [
    SAMPLE_JSX,
    SAMPLE_TS_TYPES,
    SAMPLE_TEMPLATES,
    SAMPLE_COMPUTED_KEYS,
    SAMPLE_DECORATORS,
    SAMPLE_UNICODE,
];

const LARGE_SAMPLE: &str = r##"
import * as React from "react";
import { useMemo, useState, useCallback } from "react";

export type Tone = "neutral" | "info" | "success" | "warning" | "danger";
export type Size = "sm" | "md" | "lg";

export interface TokenSet {
  readonly color: string;
  readonly background: string;
  readonly borderRadius: number;
  readonly padding: readonly [number, number];
}

export interface ChipProps {
  label: string;
  tone?: Tone;
  size?: Size;
  count?: number;
  disabled?: boolean;
  onSelect?: (label: string) => void;
}

const TONES: Record<Tone, TokenSet> = {
  neutral: { color: "#111", background: "#eee", borderRadius: 6, padding: [2, 8] },
  info: { color: "#03318c", background: "#dbe9ff", borderRadius: 6, padding: [2, 8] },
  success: { color: "#0a5a2a", background: "#d8f5e3", borderRadius: 6, padding: [2, 8] },
  warning: { color: "#7a4d05", background: "#fff0cc", borderRadius: 6, padding: [2, 8] },
  danger: { color: "#8c0313", background: "#ffdbe0", borderRadius: 6, padding: [2, 8] },
};

const SIZE_SCALE: Record<Size, number> = { sm: 0.85, md: 1, lg: 1.2 };

function px(n: number): string {
  return `${Math.round(n * 100) / 100}px`;
}

function paddingFor(tokens: TokenSet, size: Size): string {
  const [v, h] = tokens.padding;
  const k = SIZE_SCALE[size];
  return `${px(v * k)} ${px(h * k)}`;
}

export function chipStyle(tone: Tone, size: Size): Record<string, string> {
  const tokens = TONES[tone];
  return {
    color: tokens.color,
    background: tokens.background,
    borderRadius: px(tokens.borderRadius),
    padding: paddingFor(tokens, size),
  };
}

export function Chip({ label, tone = "neutral", size = "md", count, disabled, onSelect }: ChipProps) {
  const [hovered, setHovered] = useState(false);
  const style = useMemo(() => chipStyle(tone, size), [tone, size]);
  const handleClick = useCallback(() => {
    if (!disabled && onSelect) {
      onSelect(label);
    }
  }, [disabled, onSelect, label]);
  const suffix = count === undefined ? null : <span className="count">{count > 99 ? "99+" : count}</span>;
  return (
    <button
      type="button"
      style={style}
      disabled={disabled}
      data-tone={tone}
      data-size={size}
      data-hovered={hovered ? "true" : "false"}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      onClick={handleClick}
    >
      {label}
      {suffix}
    </button>
  );
}

export interface ChipGroupProps {
  items: ReadonlyArray<{ label: string; tone?: Tone }>;
  size?: Size;
  onSelect?: (label: string) => void;
}

export function ChipGroup({ items, size = "md", onSelect }: ChipGroupProps) {
  const summary = useMemo(() => {
    const byTone = new Map<Tone, number>();
    for (const item of items) {
      const tone = item.tone ?? "neutral";
      byTone.set(tone, (byTone.get(tone) ?? 0) + 1);
    }
    return [...byTone.entries()].map(([tone, n]) => `${tone}:${n}`).join(", ");
  }, [items]);
  return (
    <div role="group" aria-label={`chips (${summary})`}>
      {items.map((item, index) => (
        <Chip
          key={`${item.label}-${index}`}
          label={item.label}
          tone={item.tone}
          size={size}
          count={index}
          onSelect={onSelect}
        />
      ))}
    </div>
  );
}

export const enum Layer {
  Base = 0,
  Overlay = 10,
  Toast = 100,
}

const layerNames: Record<Layer, string> = {
  [Layer.Base]: "base",
  [Layer.Overlay]: "overlay",
  [Layer.Toast]: "toast",
};

export function layerName(layer: Layer): string {
  return layerNames[layer] ?? `layer-${layer}`;
}

export class ToneCycler {
  private index = 0;
  private readonly order: readonly Tone[] = ["neutral", "info", "success", "warning", "danger"];
  next(): Tone {
    const tone = this.order[this.index % this.order.length];
    this.index += 1;
    return tone;
  }
  reset(): void {
    this.index = 0;
  }
  get position(): number {
    return this.index;
  }
}

export function* tonePairs(): Generator<[Tone, Tone]> {
  const cycler = new ToneCycler();
  for (let i = 0; i < 4; i += 1) {
    yield [cycler.next(), cycler.next()];
  }
}

export async function loadThemeNames(fetcher: (url: string) => Promise<string[]>): Promise<string[]> {
  try {
    const names = await fetcher("/themes.json");
    return names.filter((name) => name.length > 0).map((name) => name.normalize("NFC"));
  } catch {
    return ["default"];
  }
}

export const I18N = {
  "zh-Hans": { chip: "标签", group: "标签组" },
  "ja-JP": { chip: "チップ", group: "チップグループ" },
  "ar-EG": { chip: "شريحة", group: "مجموعة" },
  emoji: { chip: "🏷️", group: "🗂️" },
} as const;

export type Locale = keyof typeof I18N;

export function localizedChipName(locale: Locale): string {
  const table = I18N[locale];
  return "chip" in table ? table.chip : `${String(locale)} (missing)`;
}

export type SelectionAction =
  | { type: "select"; label: string }
  | { type: "deselect"; label: string }
  | { type: "clear" }
  | { type: "toggle"; label: string; exclusive?: boolean };

export interface SelectionState {
  readonly selected: readonly string[];
  readonly lastAction: SelectionAction["type"] | null;
}

export const initialSelection: SelectionState = { selected: [], lastAction: null };

export function selectionReducer(state: SelectionState, action: SelectionAction): SelectionState {
  switch (action.type) {
    case "select": {
      if (state.selected.includes(action.label)) {
        return { ...state, lastAction: action.type };
      }
      return { selected: [...state.selected, action.label], lastAction: action.type };
    }
    case "deselect": {
      return {
        selected: state.selected.filter((label) => label !== action.label),
        lastAction: action.type,
      };
    }
    case "clear": {
      return { selected: [], lastAction: action.type };
    }
    case "toggle": {
      const has = state.selected.includes(action.label);
      if (action.exclusive) {
        return { selected: has ? [] : [action.label], lastAction: action.type };
      }
      return has
        ? { selected: state.selected.filter((label) => label !== action.label), lastAction: action.type }
        : { selected: [...state.selected, action.label], lastAction: action.type };
    }
    default: {
      const exhausted: never = action;
      throw new Error(`unknown action: ${JSON.stringify(exhausted)}`);
    }
  }
}
"##;

#[test]
fn valid_tsx_reprints_deterministically() {
    let first = reprint(SAMPLE_JSX);
    let second = reprint(SAMPLE_JSX);
    let third = reprint(SAMPLE_JSX);
    assert!(!first.is_empty());
    assert_eq!(first, second, "reprint output differs run-to-run");
    assert_eq!(first, third, "reprint output differs run-to-run");
}

#[test]
fn reprint_is_idempotent_over_samples() {
    for (index, sample) in SAMPLES.iter().enumerate() {
        let once = reprint(sample);
        let twice = reprint(&once);
        assert_eq!(once, twice, "reprint not idempotent for sample {index}");
    }
}

#[test]
fn parse_check_accepts_valid_source_silently() {
    for sample in SAMPLES {
        let (code, stdout, stderr) = run_cli(&["parse-check"], sample);
        assert_eq!(code, 0, "parse-check rejected valid sample: {stderr}");
        assert!(stdout.is_empty(), "parse-check wrote to stdout: {stdout}");
        assert!(stderr.is_empty(), "parse-check wrote to stderr: {stderr}");
    }
}

#[test]
fn parse_check_rejects_broken_source_with_json() {
    let (code, stdout, stderr) = run_cli(&["parse-check"], "const = ;;; <<<");
    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "failure path wrote to stdout: {stdout}");
    assert_error_json(&stderr);
}

#[test]
fn reprint_rejects_broken_source_with_json() {
    let (code, stdout, stderr) = run_cli(&["reprint"], "export default = {;");
    assert_eq!(code, 1);
    assert!(stdout.is_empty(), "failure path wrote to stdout: {stdout}");
    assert_error_json(&stderr);
}

fn assert_error_json(stderr: &str) {
    let line = stderr.trim_end();
    assert!(!line.contains('\n'), "stderr not a single line: {stderr}");
    let parsed: serde_json::Value = serde_json::from_str(line).expect("stderr is JSON");
    assert_eq!(parsed["error"], "PARSE");
    let diagnostics = parsed["diagnostics"].as_array().expect("diagnostics array");
    assert!(!diagnostics.is_empty());
    for diagnostic in diagnostics {
        assert!(
            diagnostic["message"].is_string(),
            "diagnostic missing message: {diagnostic}"
        );
    }
}

#[test]
fn filename_flag_selects_dialect() {
    // Generic arrows are valid in .ts but parse as JSX (and fail) under the tsx default.
    let generic_arrow = "const id = <T>(x: T): T => x;\n";
    let (ts_code, _, ts_err) = run_cli(&["parse-check", "--filename", "a.ts"], generic_arrow);
    assert_eq!(ts_code, 0, "expected .ts parse to pass: {ts_err}");
    let (tsx_code, _, tsx_err) = run_cli(&["parse-check"], generic_arrow);
    assert_eq!(tsx_code, 1);
    assert_error_json(&tsx_err);
}

#[test]
fn unknown_extension_falls_back_to_tsx() {
    let source = "const x = <div a=\"b\">hi</div>;\nlet y: number = 1;\n";
    let (code, _, stderr) = run_cli(&["parse-check", "--filename", "styles.weird"], source);
    assert_eq!(code, 0, "tsx fallback did not apply: {stderr}");
}

#[test]
fn reprint_with_filename_flag_matches_default_for_tsx() {
    let flagged = {
        let (code, stdout, stderr) = run_cli(&["reprint", "--filename", "sample.tsx"], SAMPLE_JSX);
        assert_eq!(code, 0, "reprint --filename failed: {stderr}");
        stdout
    };
    assert_eq!(flagged, reprint(SAMPLE_JSX));
}

#[test]
fn large_sample_round_trips_idempotently() {
    assert!(LARGE_SAMPLE.lines().count() >= 190, "large sample shrank");
    let once = reprint(LARGE_SAMPLE);
    let twice = reprint(&once);
    assert_eq!(once, twice, "large sample reprint not idempotent");
    let again = reprint(LARGE_SAMPLE);
    assert_eq!(once, again, "large sample reprint differs run-to-run");
    for marker in ["ChipGroup", "标签", "شريحة", "tonePairs"] {
        assert!(once.contains(marker), "marker {marker} lost in reprint");
    }
}
