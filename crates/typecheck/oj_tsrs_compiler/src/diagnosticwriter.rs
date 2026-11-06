// Port of Go's `diagnosticwriter` package for ast diagnostics. Go's `Diagnostic` interface also covers LSP
// diagnostics; tsc only formats `ASTDiagnostic`s, so the functions here take a `P<Diagnostic>` and read it through
// `ASTDiagnostic`'s methods (`diagnostic_file`, `diagnostic_pos`, `diagnostic_message_chain`, ...), which map a
// diagnostic on a content-mapped file back to its original text.

use std::io::Write;

use tsrs_ast::{Diagnostic, SourceFile, SourceFileLike, new_compiler_diagnostic};
use tsrs_core::tspath::{self, ComparePathsOptions};
use tsrs_core::{P, TextRange};
use tsrs_diagnostics::{self as diagnostics, Category};

// Go `FileLike`: the file a diagnostic renders against. Go's ASTDiagnostic.File returns the source file itself, or
// a fresh `renamedFile` / `originalTextFile` on every call; `key` keeps that identity (getErrorSummary groups by it).
// diagnosticwriter.go:21
pub enum FileLike {
    // The diagnostic's source file.
    SourceFile(P<SourceFile>),
    // diagnosticwriter.go:135 renamedFile
    Renamed { file: P<SourceFile>, file_name: &'static str },
    // diagnosticwriter.go:116 originalTextFile
    OriginalText(OriginalTextFile),
}

impl FileLike {
    fn file_name(&self) -> &str {
        match self {
            FileLike::SourceFile(file) => file.get().file_name(),
            FileLike::Renamed { file_name, .. } => file_name,
            FileLike::OriginalText(file) => file.file_name,
        }
    }

    // The map key Go's interface value is: the file for the file itself, nothing (a new key) for a wrapper.
    fn key(&self) -> Option<P<SourceFile>> {
        match self {
            FileLike::SourceFile(file) => Some(*file),
            _ => None,
        }
    }
}

impl SourceFileLike for FileLike {
    fn text(&self) -> &str {
        match self {
            FileLike::SourceFile(file) | FileLike::Renamed { file, .. } => file.get().text(),
            FileLike::OriginalText(file) => file.text,
        }
    }

    fn ecma_line_map(&self) -> &[u32] {
        match self {
            FileLike::SourceFile(file) | FileLike::Renamed { file, .. } => {
                file.get().ecma_line_map()
            }
            FileLike::OriginalText(file) => &file.line_map,
        }
    }
}

// diagnosticwriter.go:58 (ASTDiagnostic).File
fn diagnostic_file(d: P<Diagnostic>) -> Option<FileLike> {
    d.loc()?;
    let file = d.file()?;
    let mut file_name = file.get().file_name();
    if let Some(canonical) = file.canonical_source_file() {
        file_name = canonical.get().file_name();
    }
    if resolve(d).use_original {
        // The mapper's own diagnostics (Source != "") already carry original ranges; compiler
        // diagnostics have their transformed ranges mapped back. Both render against the original,
        // untransformed text. Diagnostics in synthesized code (see resolve) keep the virtual text.
        return Some(FileLike::OriginalText(new_original_text_file(file, file_name)));
    }
    if file_name != file.file_name() {
        return Some(FileLike::Renamed { file, file_name });
    }
    Some(FileLike::SourceFile(file))
}

// diagnosticwriter.go:83
fn diagnostic_pos(d: P<Diagnostic>) -> u32 {
    resolve(d).loc.pos().expect_source("rendered diagnostics have source positions")
}

// diagnosticwriter.go:85
fn diagnostic_len(d: P<Diagnostic>) -> u32 {
    resolve(d).loc.len().expect("rendered diagnostics have source positions")
}

// resolvedLocation describes how a diagnostic on a content-mapped file should be reported.
// diagnosticwriter.go:88
struct ResolvedLocation {
    loc: TextRange,
    use_original: bool, // render against the file's original, untransformed text
    synthesized: bool,  // the range is in virtual code with no corresponding original location
}

// resolve determines where and against which text a diagnostic should be reported. A content mapper's
// own diagnostics already carry original ranges. A compiler diagnostic on a content-mapped file has its
// virtual range mapped back to the original; if it falls entirely within synthesized code, there is no
// original location, so it is shown against the virtual text and flagged as synthesized.
// diagnosticwriter.go:98
fn resolve(d: P<Diagnostic>) -> ResolvedLocation {
    let loc = d.loc().unwrap_or_else(TextRange::synthetic);
    let Some(file) = d.file() else {
        return ResolvedLocation { loc, use_original: false, synthesized: false };
    };
    if !d.source().is_empty() {
        return ResolvedLocation { loc, use_original: true, synthesized: false };
    }
    if let Some(span_map) = file.span_map() {
        let (mapped, fidelity) = span_map.virtual_to_original_span(loc);
        if fidelity == tsrs_spanmap::Fidelity::None {
            return ResolvedLocation { loc, use_original: false, synthesized: true };
        }
        return ResolvedLocation { loc: mapped, use_original: true, synthesized: false };
    }
    ResolvedLocation { loc, use_original: false, synthesized: false }
}

// originalTextFile presents a source file's original (untransformed) text as a FileLike, so that
// diagnostics whose ranges point into that text render at the correct locations.
// diagnosticwriter.go:116
pub struct OriginalTextFile {
    file_name: &'static str,
    text: &'static str,
    line_map: Vec<u32>,
}

// diagnosticwriter.go:122
fn new_original_text_file(file: P<SourceFile>, file_name: &'static str) -> OriginalTextFile {
    let text = file.original_text();
    OriginalTextFile { file_name, text, line_map: tsrs_core::compute_ecma_line_starts(text) }
}

// diagnosticwriter.go:144 (ASTDiagnostic).MessageChain
fn diagnostic_message_chain(d: P<Diagnostic>) -> Vec<P<Diagnostic>> {
    let chain = d.message_chain();
    let mut result = Vec::with_capacity(chain.len() + 1);
    result.extend_from_slice(chain);
    if resolve(d).synthesized {
        // The diagnostic points into synthesized virtual code; make clear the shown location is not in the
        // original file, and which content mapper produced it.
        let note = new_compiler_diagnostic(
            &diagnostics::THIS_LOCATION_IS_IN_VIRTUAL_CODE_PRODUCED_BY_THE_CONTENT_MAPPER_0_AND_HAS_NO_CORRESPONDING_LOCATION_IN_THE_ORIGINAL_FILE,
            &[&d.file().unwrap().content_mapper()],
        );
        result.push(note);
    }
    result
}

pub struct FormattingOptions {
    pub compare_paths_options: ComparePathsOptions,
    pub new_line: String,
}

const FOREGROUND_COLOR_ESCAPE_GREY: &str = "\u{001b}[90m";
const FOREGROUND_COLOR_ESCAPE_RED: &str = "\u{001b}[91m";
const FOREGROUND_COLOR_ESCAPE_YELLOW: &str = "\u{001b}[93m";
const FOREGROUND_COLOR_ESCAPE_BLUE: &str = "\u{001b}[94m";
const FOREGROUND_COLOR_ESCAPE_CYAN: &str = "\u{001b}[96m";

const GUTTER_STYLE_SEQUENCE: &str = "\u{001b}[7m";
const GUTTER_SEPARATOR: &str = " ";
const RESET_ESCAPE_SEQUENCE: &str = "\u{001b}[0m";
const ELLIPSIS: &str = "...";

// Go writes through an io.Writer and ignores write errors; so do we.
macro_rules! w {
    ($out:expr_2021, $($arg:tt)*) => {{
        let _ = write!($out, $($arg)*);
    }};
}

pub fn format_diagnostic_with_color_and_context(
    output: &mut dyn Write,
    diagnostic: P<Diagnostic>,
    format_opts: &FormattingOptions,
) {
    if let Some(file) = diagnostic_file(diagnostic) {
        let pos = diagnostic_pos(diagnostic);
        write_location(output, &file, pos, Some(format_opts), &write_with_style_and_reset);
        w!(output, " - ");
    }

    write_with_style_and_reset(
        output,
        diagnostic.category().name(),
        get_category_format(diagnostic.category()),
    );
    w!(
        output,
        "{} {}{}: {}",
        FOREGROUND_COLOR_ESCAPE_GREY,
        diagnostic_prefix(diagnostic),
        diagnostic.code(),
        RESET_ESCAPE_SEQUENCE
    );
    write_flattened_diagnostic_message(output, diagnostic, &format_opts.new_line);

    if let Some(file) = diagnostic_file(diagnostic)
        && diagnostic.code() != diagnostics::FILE_APPEARS_TO_BE_BINARY.code()
    {
        w!(output, "{}", format_opts.new_line);
        write_code_snippet(
            output,
            &file,
            diagnostic_pos(diagnostic),
            diagnostic_len(diagnostic),
            get_category_format(diagnostic.category()),
            "",
            format_opts,
        );
        w!(output, "{}", format_opts.new_line);
    }

    let related = diagnostic.related_information();
    if !related.is_empty() {
        for &related_information in related {
            if let Some(file) = diagnostic_file(related_information) {
                w!(output, "{}", format_opts.new_line);
                w!(output, "  ");
                let pos = diagnostic_pos(related_information);
                write_location(output, &file, pos, Some(format_opts), &write_with_style_and_reset);
                w!(output, " - ");
                write_flattened_diagnostic_message(
                    output,
                    related_information,
                    &format_opts.new_line,
                );
                write_code_snippet(
                    output,
                    &file,
                    pos,
                    diagnostic_len(related_information),
                    FOREGROUND_COLOR_ESCAPE_CYAN,
                    "    ",
                    format_opts,
                );
            }
            w!(output, "{}", format_opts.new_line);
        }
    }
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

fn write_code_snippet(
    writer: &mut dyn Write,
    source_file: &FileLike,
    start: u32,
    length: u32,
    squiggle_color: &str,
    indent: &str,
    format_opts: &FormattingOptions,
) {
    let (first_line, first_line_char) =
        tsrs_scanner::get_ecma_line_and_utf16_character_of_position(source_file, start);
    let (last_line, mut last_line_char) =
        tsrs_scanner::get_ecma_line_and_utf16_character_of_position(
            source_file,
            start.checked_add(length).expect("diagnostic range overflow"),
        );
    if length == 0 {
        last_line_char += 1; // When length is zero, squiggle the character right after the start position.
    }

    let text = source_file.text();
    let last_line_of_file = tsrs_scanner::get_ecma_line_of_position(source_file, text.len() as u32);

    let has_more_than_five_lines = last_line - first_line >= 4;
    let mut gutter_width = (last_line + 1).to_string().len();
    if has_more_than_five_lines {
        gutter_width = gutter_width.max(ELLIPSIS.len());
    }

    let mut i = first_line;
    while i <= last_line {
        w!(writer, "{}", format_opts.new_line);

        // If the error spans over 5 lines, we'll only show the first 2 and last 2 lines,
        // so we'll skip ahead to the second-to-last line.
        if has_more_than_five_lines && first_line + 1 < i && i < last_line - 1 {
            w!(writer, "{}", indent);
            w!(writer, "{}", GUTTER_STYLE_SEQUENCE);
            w!(writer, "{:>width$}", ELLIPSIS, width = gutter_width);
            w!(writer, "{}", RESET_ESCAPE_SEQUENCE);
            w!(writer, "{}", GUTTER_SEPARATOR);
            w!(writer, "{}", format_opts.new_line);
            i = last_line - 1;
        }

        let line_start =
            tsrs_scanner::get_ecma_position_of_line_and_byte_offset(source_file, i, 0) as usize;
        let line_end = if i < last_line_of_file {
            tsrs_scanner::get_ecma_position_of_line_and_byte_offset(source_file, i + 1, 0) as usize
        } else {
            text.len()
        };

        let line_content = text[line_start..line_end].trim_end(); // trim from end
        let line_content = line_content.replace('\t', " "); // convert tabs to single spaces

        // Output the gutter and the actual contents of the line.
        w!(writer, "{}", indent);
        w!(writer, "{}", GUTTER_STYLE_SEQUENCE);
        w!(writer, "{:>width$}", i + 1, width = gutter_width);
        w!(writer, "{}", RESET_ESCAPE_SEQUENCE);
        w!(writer, "{}", GUTTER_SEPARATOR);
        w!(writer, "{}", line_content);
        w!(writer, "{}", format_opts.new_line);

        // Output the gutter and the error span for the line using tildes.
        w!(writer, "{}", indent);
        w!(writer, "{}", GUTTER_STYLE_SEQUENCE);
        w!(writer, "{:>width$}", "", width = gutter_width);
        w!(writer, "{}", RESET_ESCAPE_SEQUENCE);
        w!(writer, "{}", GUTTER_SEPARATOR);
        w!(writer, "{}", squiggle_color);
        if i == first_line {
            // If we're on the last line, then limit it to the last character of the last line.
            // Otherwise, we'll just squiggle the rest of the line, giving 'slice' no end position.
            let last_char_for_line = if i == last_line {
                last_line_char as i64
            } else {
                utf16_len(&line_content) as i64
            };

            // Fill with spaces until the first character,
            // then squiggle the remainder of the line.
            w!(writer, "{}", " ".repeat(first_line_char as usize));
            w!(
                writer,
                "{}",
                "~".repeat(go_repeat_count(last_char_for_line - first_line_char as i64))
            );
        } else if i == last_line {
            // Squiggle until the final character.
            w!(writer, "{}", "~".repeat(go_repeat_count(last_line_char as i64)));
        } else {
            // Squiggle the entire line.
            w!(writer, "{}", "~".repeat(utf16_len(&line_content)));
        }

        w!(writer, "{}", RESET_ESCAPE_SEQUENCE);
        i += 1;
    }
}

// strings.Repeat panics on a negative count; the Go code never passes one for valid diagnostics.
fn go_repeat_count(n: i64) -> usize {
    assert!(n >= 0, "strings: negative Repeat count");
    n as usize
}

pub fn write_flattened_diagnostic_message(
    writer: &mut dyn Write,
    diagnostic: P<Diagnostic>,
    newline: &str,
) {
    w!(writer, "{}", diagnostic.localize());

    for chain in diagnostic_message_chain(diagnostic) {
        flatten_diagnostic_message_chain(writer, chain, newline, 1 /*level*/);
    }
}

fn flatten_diagnostic_message_chain(
    writer: &mut dyn Write,
    chain: P<Diagnostic>,
    new_line: &str,
    level: usize,
) {
    w!(writer, "{}", new_line);
    for _ in 0..level {
        w!(writer, "  ");
    }

    w!(writer, "{}", chain.localize());
    for child in diagnostic_message_chain(chain) {
        flatten_diagnostic_message_chain(writer, child, new_line, level + 1);
    }
}

// diagnosticPrefix returns the prefix shown before a diagnostic's code, e.g. "TS" for compiler
// diagnostics or an external source's custom prefix.
fn diagnostic_prefix(diagnostic: P<Diagnostic>) -> &'static str {
    let source = diagnostic.source();
    if !source.is_empty() {
        return source;
    }
    "TS"
}

fn get_category_format(category: Category) -> &'static str {
    match category {
        Category::Error => FOREGROUND_COLOR_ESCAPE_RED,
        Category::Warning => FOREGROUND_COLOR_ESCAPE_YELLOW,
        Category::Suggestion => FOREGROUND_COLOR_ESCAPE_GREY,
        Category::Message => FOREGROUND_COLOR_ESCAPE_BLUE,
    }
}

type FormattedWriter = dyn Fn(&mut dyn Write, &str, &str);

fn write_with_style_and_reset(output: &mut dyn Write, text: &str, format_style: &str) {
    w!(output, "{}", format_style);
    w!(output, "{}", text);
    w!(output, "{}", RESET_ESCAPE_SEQUENCE);
}

fn write_location(
    output: &mut dyn Write,
    file: &FileLike,
    pos: u32,
    format_opts: Option<&FormattingOptions>,
    write_with_style_and_reset: &FormattedWriter,
) {
    let (first_line, first_char) =
        tsrs_scanner::get_ecma_line_and_utf16_character_of_position(file, pos);
    let relative_file_name = match format_opts {
        Some(opts) => {
            tspath::convert_to_relative_path(file.file_name(), &opts.compare_paths_options)
        }
        None => file.file_name().to_string(),
    };

    write_with_style_and_reset(output, &relative_file_name, FOREGROUND_COLOR_ESCAPE_CYAN);
    w!(output, ":");
    write_with_style_and_reset(
        output,
        &(first_line + 1).to_string(),
        FOREGROUND_COLOR_ESCAPE_YELLOW,
    );
    w!(output, ":");
    write_with_style_and_reset(
        output,
        &(first_char + 1).to_string(),
        FOREGROUND_COLOR_ESCAPE_YELLOW,
    );
}

// Some of these lived in watch.ts, but they're not specific to the watch API.

struct ErrorSummary {
    total_error_count: usize,
    global_errors: Vec<P<Diagnostic>>,
    // Go's ErrorsByFile in SortedFiles order.
    errors_by_file: Vec<(FileLike, Vec<P<Diagnostic>>)>,
}

pub fn write_error_summary_text(
    output: &mut dyn Write,
    all_diagnostics: &[P<Diagnostic>],
    format_opts: &FormattingOptions,
) {
    // Roughly corresponds to 'getErrorSummaryText' from watch.ts

    let error_summary = get_error_summary(all_diagnostics);
    let total_error_count = error_summary.total_error_count;
    if total_error_count == 0 {
        return;
    }

    let first_file = error_summary.errors_by_file.first();
    let first_file_name = match first_file {
        Some((file, errors)) => pretty_path_for_file_error(Some(file), errors, format_opts),
        None => String::new(),
    };
    let num_erroring_files = error_summary.errors_by_file.len();

    let message = if total_error_count == 1 {
        // Special-case a single error.
        if !error_summary.global_errors.is_empty() || first_file_name.is_empty() {
            diagnostics::FOUND_1_ERROR.localize(&[])
        } else {
            diagnostics::FOUND_1_ERROR_IN_0.localize(&[&first_file_name])
        }
    } else {
        match num_erroring_files {
            // No file-specific errors.
            0 => diagnostics::FOUND_0_ERRORS.localize(&[&total_error_count]),
            // One file with errors.
            1 => diagnostics::FOUND_0_ERRORS_IN_THE_SAME_FILE_STARTING_AT_COLON_1
                .localize(&[&total_error_count, &first_file_name]),
            // Multiple files with errors.
            _ => diagnostics::FOUND_0_ERRORS_IN_1_FILES
                .localize(&[&total_error_count, &num_erroring_files]),
        }
    };
    w!(output, "{}", format_opts.new_line);
    w!(output, "{}", message);
    w!(output, "{}", format_opts.new_line);
    w!(output, "{}", format_opts.new_line);
    if num_erroring_files > 1 {
        write_tabular_errors_display(output, &error_summary, format_opts);
        w!(output, "{}", format_opts.new_line);
    }
}

fn get_error_summary(diags: &[P<Diagnostic>]) -> ErrorSummary {
    let mut total_error_count = 0;
    let mut global_errors = Vec::new();
    let mut errors_by_file: Vec<(FileLike, Vec<P<Diagnostic>>)> = Vec::new();
    let mut index_by_file: rustc_hash::FxHashMap<P<SourceFile>, usize> =
        rustc_hash::FxHashMap::default();

    for &diagnostic in diags {
        if diagnostic.category() != Category::Error {
            continue;
        }

        total_error_count += 1;
        match diagnostic_file(diagnostic) {
            None => global_errors.push(diagnostic),
            Some(file) => {
                // A wrapper (`FileLike::key` is None) is a new map key each time, as in Go.
                let idx = if let Some(key) = file.key() {
                    *index_by_file.entry(key).or_insert_with(|| {
                        errors_by_file.push((file, Vec::new()));
                        errors_by_file.len() - 1
                    })
                } else {
                    errors_by_file.push((file, Vec::new()));
                    errors_by_file.len() - 1
                };
                errors_by_file[idx].1.push(diagnostic);
            }
        }
    }

    // !!!
    // Need an ordered map here, but sorting for consistency.
    errors_by_file.sort_by(|a, b| a.0.file_name().cmp(b.0.file_name()));

    ErrorSummary { total_error_count, global_errors, errors_by_file }
}

fn write_tabular_errors_display(
    output: &mut dyn Write,
    error_summary: &ErrorSummary,
    format_opts: &FormattingOptions,
) {
    let max_errors = error_summary.errors_by_file.iter().map(|(_, e)| e.len()).max().unwrap_or(0);

    // !!!
    // TODO (drosen): This was never localized.
    // Should make this better.
    let header_row = diagnostics::ERRORS_FILES.localize(&[]);
    let left_column_heading_length = header_row.split(' ').next().unwrap_or("").len();
    let length_of_biggest_error_count = max_errors.to_string().len();
    let left_padding_goal = left_column_heading_length.max(length_of_biggest_error_count);
    let header_padding = length_of_biggest_error_count.saturating_sub(left_column_heading_length);

    w!(output, "{}", " ".repeat(header_padding));
    w!(output, "{}", header_row);
    w!(output, "{}", format_opts.new_line);

    for (file, file_errors) in &error_summary.errors_by_file {
        let error_count = file_errors.len();

        w!(output, "{:>width$}  ", error_count, width = left_padding_goal);
        w!(output, "{}", pretty_path_for_file_error(Some(file), file_errors, format_opts));
        w!(output, "{}", format_opts.new_line);
    }
}

fn pretty_path_for_file_error(
    file: Option<&FileLike>,
    file_errors: &[P<Diagnostic>],
    format_opts: &FormattingOptions,
) -> String {
    let Some(file) = file else {
        return String::new();
    };
    if file_errors.is_empty() {
        return String::new();
    }
    let line = tsrs_scanner::get_ecma_line_of_position(file, diagnostic_pos(file_errors[0]));
    let mut file_name = file.file_name().to_string();
    if tspath::path_is_absolute(&file_name)
        && tspath::path_is_absolute(&format_opts.compare_paths_options.current_directory)
    {
        file_name =
            tspath::convert_to_relative_path(file.file_name(), &format_opts.compare_paths_options);
    }
    format!("{}{}:{}{}", file_name, FOREGROUND_COLOR_ESCAPE_GREY, line + 1, RESET_ESCAPE_SEQUENCE)
}

pub fn write_format_diagnostic(
    output: &mut dyn Write,
    diagnostic: P<Diagnostic>,
    format_opts: &FormattingOptions,
) {
    if let Some(file) = diagnostic_file(diagnostic) {
        let (line, character) = tsrs_scanner::get_ecma_line_and_utf16_character_of_position(
            &file,
            diagnostic_pos(diagnostic),
        );
        let file_name = file.file_name();
        let relative_file_name =
            tspath::convert_to_relative_path(file_name, &format_opts.compare_paths_options);
        w!(output, "{}({},{}): ", relative_file_name, line + 1, character + 1);
    }

    w!(
        output,
        "{} {}{}: ",
        diagnostic.category().name(),
        diagnostic_prefix(diagnostic),
        diagnostic.code()
    );
    write_flattened_diagnostic_message(output, diagnostic, &format_opts.new_line);
    w!(output, "{}", format_opts.new_line);
}

/// Fewer diagnostics than this are formatted on the calling thread (`format_diagnostics_in_order`).
const PARALLEL_FORMAT_MIN: usize = 256;
/// Diagnostics formatted per parallel round: bounds the formatted text held before it is written.
const PARALLEL_FORMAT_CHUNK: usize = 2048;

/// tsrs-only: `format` of each diagnostic passed to `write` in order, as a loop calling both would.
///
/// Many diagnostics are formatted on the worker pool, a chunk at a time, and written in order after each chunk;
/// `single_threaded` keeps the loop. The CLI reports after the parallel check, so formatting thousands of diagnostics
/// (line maps, relative file names, messages) one at a time on the main thread was the serial tail of the run.
/// `format` only reads the diagnostics and their files (a line map is built once, under its file's region lock).
pub fn format_diagnostics_in_order(
    diagnostics: &[P<Diagnostic>],
    single_threaded: bool,
    format: &(dyn Fn(&mut Vec<u8>, P<Diagnostic>) + Sync),
    write: &mut dyn FnMut(&[u8]),
) {
    use rayon::prelude::*;

    if single_threaded || diagnostics.len() < PARALLEL_FORMAT_MIN {
        let mut out = Vec::new();
        for &diagnostic in diagnostics {
            out.clear();
            format(&mut out, diagnostic);
            write(&out);
        }
        return;
    }
    let pool = crate::program::worker_pool();
    for chunk in diagnostics.chunks(PARALLEL_FORMAT_CHUNK) {
        let formatted: Vec<Vec<u8>> = pool.install(|| {
            chunk
                .par_iter()
                .map(|&diagnostic| {
                    let mut out = Vec::new();
                    format(&mut out, diagnostic);
                    out
                })
                .collect()
        });
        for out in &formatted {
            write(out);
        }
    }
}

// diagnosticwriter.go:584
pub fn format_diagnostics_status_with_color_and_time(
    output: &mut dyn Write,
    time: &str,
    diag: P<Diagnostic>,
    format_opts: &FormattingOptions,
) {
    w!(output, "[");
    write_with_style_and_reset(output, time, FOREGROUND_COLOR_ESCAPE_GREY);
    w!(output, "] ");
    write_flattened_diagnostic_message(output, diag, &format_opts.new_line);
}

// diagnosticwriter.go:591
pub fn format_diagnostics_status_and_time(
    output: &mut dyn Write,
    time: &str,
    diag: P<Diagnostic>,
    format_opts: &FormattingOptions,
) {
    w!(output, "{} - ", time);
    write_flattened_diagnostic_message(output, diag, &format_opts.new_line);
}

// diagnosticwriter.go:596 ScreenStartingCodes
fn is_screen_starting_code(code: i32) -> bool {
    code == diagnostics::STARTING_COMPILATION_IN_WATCH_MODE.code()
        || code == diagnostics::FILE_CHANGE_DETECTED_STARTING_INCREMENTAL_COMPILATION.code()
}

// diagnosticwriter.go:601
pub fn try_clear_screen(
    output: &mut dyn Write,
    diag: P<Diagnostic>,
    options: &tsrs_core::CompilerOptions,
) -> bool {
    if !options.preserve_watch_output.is_true()
        && !options.extended_diagnostics.is_true()
        && !options.diagnostics.is_true()
        && is_screen_starting_code(diag.code())
    {
        // Clear screen and move cursor to home position
        w!(output, "\x1B[2J\x1B[3J\x1B[H");
        return true;
    }
    false
}
