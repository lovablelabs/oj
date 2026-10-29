use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt::Display;

use rustc_hash::FxHashSet;
use tsrs_core::{ScriptTarget, stringutil, stringutil::Rune};
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

use crate::scanner::{
    EscapeSequenceScanningFlags, IdentifierVariant, RUNE_ERROR, Scanner, decode_rune,
    is_identifier_part, is_word_character, rune_string,
};
use crate::unicodeproperties::{
    BINARY_UNICODE_PROPERTIES, BINARY_UNICODE_PROPERTIES_OF_STRINGS, GENERAL_CATEGORY_VALUES,
    NON_BINARY_UNICODE_PROPERTIES, binary_unicode_properties, binary_unicode_properties_of_strings,
    general_category_values, non_binary_unicode_properties,
    values_of_non_binary_unicode_properties,
};

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub(crate) struct RegularExpressionFlags: u32 {
        const None = 0;
        const HasIndices = 1 << 0; // d
        const Global = 1 << 1; // g
        const IgnoreCase = 1 << 2; // i
        const Multiline = 1 << 3; // m
        const DotAll = 1 << 4; // s
        const Unicode = 1 << 5; // u
        const UnicodeSets = 1 << 6; // v
        const Sticky = 1 << 7; // y
        const AnyUnicodeMode = Self::Unicode.bits() | Self::UnicodeSets.bits();
        const Modifiers = Self::IgnoreCase.bits() | Self::Multiline.bits() | Self::DotAll.bits();
    }
}

/// Go `charCodeToRegExpFlag[ch]` ("comma ok").
pub fn char_code_to_reg_exp_flag(ch: Rune) -> Option<RegularExpressionFlags> {
    Some(match ch {
        0x64 => RegularExpressionFlags::HasIndices,
        0x67 => RegularExpressionFlags::Global,
        0x69 => RegularExpressionFlags::IgnoreCase,
        0x6D => RegularExpressionFlags::Multiline,
        0x73 => RegularExpressionFlags::DotAll,
        0x75 => RegularExpressionFlags::Unicode,
        0x76 => RegularExpressionFlags::UnicodeSets,
        0x79 => RegularExpressionFlags::Sticky,
        _ => return None,
    })
}

fn reg_exp_flag_to_first_available_language_version(
    flag: RegularExpressionFlags,
) -> Option<ScriptTarget> {
    if flag == RegularExpressionFlags::HasIndices {
        Some(ScriptTarget::ES2022)
    } else if flag == RegularExpressionFlags::DotAll {
        Some(ScriptTarget::ES2018)
    } else if flag == RegularExpressionFlags::UnicodeSets {
        Some(ScriptTarget::ES2024)
    } else {
        None
    }
}

impl Scanner {
    pub(crate) fn check_regular_expression_flag_availability(
        &mut self,
        flag: RegularExpressionFlags,
        pos: u32,
        size: u32,
    ) {
        if let Some(available_from) = reg_exp_flag_to_first_available_language_version(flag)
            && self.language_version() < available_from
        {
            let name = available_from.string().to_lowercase();
            self.error_at(&diagnostics::THIS_REGULAR_EXPRESSION_FLAG_IS_ONLY_AVAILABLE_WHEN_TARGETING_0_OR_LATER, pos, size, &[&name]);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ClassSetExpressionType {
    ClassIntersection,
    ClassSubtraction,
}

struct GroupNameReference {
    pos: u32,
    end: u32,
    name: &'static str,
}

struct DecimalEscapeValue {
    pos: u32,
    end: u32,
    value: i64,
}

/// Go `regExpParser`. Go holds a `*Scanner`; to avoid a borrowing struct this one owns the
/// scanner for the duration of the parse (see `Scanner::re_scan_slash_token`).
pub struct RegExpParser {
    scanner: Scanner,
    end: u32,
    any_unicode_mode: bool,
    unicode_sets_mode: bool,
    annex_b: bool,

    any_unicode_mode_or_non_annex_b: bool,
    named_capture_groups: bool,

    // See scanClassSetExpression.
    may_contain_strings: bool,
    // The number of all (named and unnamed) capturing groups defined in the regex.
    number_of_capturing_groups: u32,
    // All named capturing groups defined in the regex.
    group_specifiers: FxHashSet<&'static str>,
    // Go map insertion order, for deterministic spelling suggestions.
    group_specifiers_order: Vec<&'static str>,
    // All references to named capturing groups in the regex.
    group_name_references: Vec<GroupNameReference>,
    // All numeric backreferences within the regex.
    decimal_escapes: Vec<DecimalEscapeValue>,
    // A stack of scopes for named capturing groups. See scanGroupName.
    named_capturing_groups: Vec<Vec<&'static str>>,

    // pendingLowSurrogate holds the low surrogate to emit on the next
    // scanSourceCharacter call when Corsa has to split a non-BMP rune into
    // UTF-16 surrogate code units in non-unicode mode. Strada did not need
    // this bookkeeping because its source text was already indexed as UTF-16.
    pending_low_surrogate: Option<u32>,
}

fn compare_decimal_strings(a: &str, b: &str) -> Ordering {
    let mut a = a.trim_start_matches('0');
    let mut b = b.trim_start_matches('0');
    if a.is_empty() {
        a = "0";
    }
    if b.is_empty() {
        b = "0";
    }
    if a.len() != b.len() {
        if a.len() < b.len() {
            return Ordering::Less;
        }
        return Ordering::Greater;
    }
    a.cmp(b)
}

fn add_unique(set: &mut Vec<&'static str>, name: &'static str) {
    if !set.contains(&name) {
        set.push(name);
    }
}

impl RegExpParser {
    #[expect(
        clippy::fn_params_excessive_bools,
        reason = "The boolean parameters preserve the upstream Go function signature."
    )]
    pub(crate) fn new(
        scanner: Scanner,
        end: u32,
        any_unicode_mode: bool,
        unicode_sets_mode: bool,
        annex_b: bool,
        named_capture_groups: bool,
    ) -> RegExpParser {
        RegExpParser {
            scanner,
            end,
            any_unicode_mode,
            unicode_sets_mode,
            annex_b,
            any_unicode_mode_or_non_annex_b: false,
            named_capture_groups,
            may_contain_strings: false,
            number_of_capturing_groups: 0,
            group_specifiers: FxHashSet::default(),
            group_specifiers_order: Vec::new(),
            group_name_references: Vec::new(),
            decimal_escapes: Vec::new(),
            named_capturing_groups: Vec::new(),
            pending_low_surrogate: None,
        }
    }

    pub(crate) fn into_scanner(self) -> Scanner {
        self.scanner
    }

    #[inline]
    fn pos(&self) -> u32 {
        self.scanner.state.pos
    }

    #[inline]
    fn inc_pos(&mut self, n: u32) {
        self.scanner.state.pos += n;
    }

    #[inline]
    fn dec_pos(&mut self, n: u32) {
        self.scanner.state.pos -= n;
    }

    #[inline]
    fn char(&self) -> Option<u8> {
        self.scanner.char()
    }

    #[inline]
    fn char_at(&self, pos: u32) -> Option<u8> {
        self.scanner.char_at(pos - self.pos())
    }

    fn error(&mut self, msg: &'static Message, pos: u32, length: u32, args: &[&dyn Display]) {
        self.scanner.error_at(msg, pos, length, args);
    }

    #[inline]
    fn text(&self) -> &'static str {
        self.scanner.text
    }

    #[inline]
    fn text_byte(&self, i: u32) -> u8 {
        self.scanner.text.as_bytes()[i as usize]
    }

    // Disjunction ::= Alternative ('|' Alternative)*
    fn scan_disjunction(&mut self, is_in_group: bool) {
        // Names defined by any of this disjunction's alternatives. Since exactly one
        // alternative is chosen at runtime, these names are unioned together (rather
        // than intersected) and, when this disjunction is nested inside a group,
        // bubbled up into the enclosing alternative's scope once the group closes.
        // This ensures a name defined inside a nested group (e.g. `(?:(?<a>x))`) is
        // still visible to a duplicate check for a sibling group later in the same
        // enclosing alternative (e.g. `(?:(?<a>x))(?<a>z)`).
        let mut disjunction_names: Vec<&'static str> = Vec::new();
        loop {
            self.named_capturing_groups.push(Vec::new());
            self.scan_alternative(is_in_group);
            let alternative_names = self.named_capturing_groups.pop().unwrap();
            for name in alternative_names {
                add_unique(&mut disjunction_names, name);
            }
            if self.char() != Some(b'|') {
                break;
            }
            self.inc_pos(1);
        }
        if is_in_group && !self.named_capturing_groups.is_empty() {
            let parent_scope = self.named_capturing_groups.last_mut().unwrap();
            for name in disjunction_names {
                add_unique(parent_scope, name);
            }
        }
    }

    // Alternative ::= Term*
    // Term ::=
    //
    //	| Assertion
    //	| Atom Quantifier?
    //
    // Assertion ::=
    //
    //	| '^'
    //	| '$'
    //	| '\b'
    //	| '\B'
    //	| '(?=' Disjunction ')'
    //	| '(?!' Disjunction ')'
    //	| '(?<=' Disjunction ')'
    //	| '(?<!' Disjunction ')'
    //
    // Quantifier ::= QuantifierPrefix '?'?
    // QuantifierPrefix ::=
    //
    //	| '*'
    //	| '+'
    //	| '?'
    //	| '{' DecimalDigits (',' DecimalDigits?)? '}'
    //
    // Atom ::=
    //
    //	| PatternCharacter
    //	| '.'
    //	| '\' AtomEscape
    //	| CharacterClass
    //	| '(?<' RegExpIdentifierName '>' Disjunction ')'
    //	| '(?' RegularExpressionFlags ('-' RegularExpressionFlags)? ':' Disjunction ')'
    //
    // CharacterClass ::= unicodeMode
    //
    //	? '[' ClassRanges ']'
    //	: '[' ClassSetExpression ']'
    fn scan_alternative(&mut self, is_in_group: bool) {
        let mut is_previous_term_quantifiable = false;
        while self.pos() < self.end {
            let start = self.pos();
            let ch = self.char().expect("position is before regexp end");
            match ch {
                0x5E | 0x24 /* ^ $ */ => {
                    self.inc_pos(1);
                    is_previous_term_quantifiable = false;
                }
                0x5C /* \ */ => {
                    self.inc_pos(1);
                    if let Some(0x62 | 0x42) /* b B */ = self.char() {
                        self.inc_pos(1);
                        is_previous_term_quantifiable = false;
                    } else {
                        self.scan_atom_escape();
                        is_previous_term_quantifiable = true;
                    }
                }
                0x28 /* ( */ => {
                    self.inc_pos(1);
                    if self.char() == Some(b'?') {
                        self.inc_pos(1);
                        match self.char() {
                            Some(0x3D | 0x21) /* = ! */ => {
                                self.inc_pos(1);
                                // In Annex B, `(?=Disjunction)` and `(?!Disjunction)` are quantifiable
                                is_previous_term_quantifiable = !self.any_unicode_mode_or_non_annex_b;
                            }
                            Some(0x3C) /* < */ => {
                                let group_name_start = self.pos();
                                self.inc_pos(1);
                                if let Some(0x3D | 0x21) /* = ! */ = self.char() {
                                    self.inc_pos(1);
                                    is_previous_term_quantifiable = false;
                                } else {
                                    self.scan_group_name(false /*isReference*/);
                                    self.scan_expected_char(b'>');
                                    if self.scanner.language_version() < ScriptTarget::ES2018 {
                                        self.error(
                                            &diagnostics::NAMED_CAPTURING_GROUPS_ARE_ONLY_AVAILABLE_WHEN_TARGETING_ES2018_OR_LATER,
                                            group_name_start,
                                            self.pos() - group_name_start,
                                            &[],
                                        );
                                    }
                                    self.number_of_capturing_groups += 1;
                                    is_previous_term_quantifiable = true;
                                }
                            }
                            _ => {
                                let flags_start = self.pos();
                                let set_flags = self.scan_pattern_modifiers(RegularExpressionFlags::None);
                                if self.char() == Some(b'-') {
                                    self.inc_pos(1);
                                    self.scan_pattern_modifiers(set_flags);
                                    if self.pos() == flags_start + 1 {
                                        self.error(
                                            &diagnostics::SUBPATTERN_FLAGS_MUST_BE_PRESENT_WHEN_THERE_IS_A_MINUS_SIGN,
                                            flags_start,
                                            self.pos() - flags_start,
                                            &[],
                                        );
                                    }
                                }
                                // Modifier characters were consumed, so this is `(?flags:` rather than a plain `(?:` group.
                                if self.pos() != flags_start && self.scanner.language_version() < ScriptTarget::ES2025 {
                                    let name = ScriptTarget::ES2025.string().to_lowercase();
                                    self.error(
                                        &diagnostics::REGULAR_EXPRESSION_PATTERN_MODIFIERS_ARE_ONLY_AVAILABLE_WHEN_TARGETING_0_OR_LATER,
                                        flags_start,
                                        self.pos() - flags_start,
                                        &[&name],
                                    );
                                }
                                self.scan_expected_char(b':');
                                is_previous_term_quantifiable = true;
                            }
                        }
                    } else {
                        self.number_of_capturing_groups += 1;
                        is_previous_term_quantifiable = true;
                    }
                    self.scan_disjunction(true /*isInGroup*/);
                    self.scan_expected_char(b')');
                }
                0x7B | 0x2A | 0x2B | 0x3F /* { * + ? */ => {
                    if ch == b'{' {
                        self.inc_pos(1);
                        let digits_start = self.pos();
                        self.scan_digits();
                        let min_str = self.scanner.state.token_value;
                        if !self.any_unicode_mode_or_non_annex_b && min_str.is_empty() {
                            is_previous_term_quantifiable = true;
                            continue;
                        }
                        if self.char() == Some(b',') {
                            self.inc_pos(1);
                            self.scan_digits();
                            let max_str = self.scanner.state.token_value;
                            if min_str.is_empty() {
                                if !max_str.is_empty() || self.char() == Some(b'}') {
                                    self.error(&diagnostics::INCOMPLETE_QUANTIFIER_DIGIT_EXPECTED, digits_start, 0, &[]);
                                } else {
                                    let s = rune_string(u32::from(ch));
                                    self.error(&diagnostics::UNEXPECTED_0_DID_YOU_MEAN_TO_ESCAPE_IT_WITH_BACKSLASH, start, 1, &[&s]);
                                    is_previous_term_quantifiable = true;
                                    continue;
                                }
                            } else if !max_str.is_empty()
                                && compare_decimal_strings(min_str, max_str).is_gt()
                                    && (self.any_unicode_mode_or_non_annex_b || self.char() == Some(b'}'))
                                {
                                    self.error(&diagnostics::NUMBERS_OUT_OF_ORDER_IN_QUANTIFIER, digits_start, self.pos() - digits_start, &[]);
                                }
                        } else if min_str.is_empty() {
                            if self.any_unicode_mode_or_non_annex_b {
                                let s = rune_string(u32::from(ch));
                                self.error(&diagnostics::UNEXPECTED_0_DID_YOU_MEAN_TO_ESCAPE_IT_WITH_BACKSLASH, start, 1, &[&s]);
                            }
                            is_previous_term_quantifiable = true;
                            continue;
                        }
                        if self.char() != Some(b'}') {
                            if self.any_unicode_mode_or_non_annex_b {
                                self.error(&diagnostics::X_0_EXPECTED, self.pos(), 0, &[&"}"]);
                                self.dec_pos(1);
                            } else {
                                is_previous_term_quantifiable = true;
                                continue;
                            }
                        }
                    }
                    self.inc_pos(1);
                    if self.char() == Some(b'?') {
                        // Non-greedy
                        self.inc_pos(1);
                    }
                    if !is_previous_term_quantifiable {
                        self.error(&diagnostics::THERE_IS_NOTHING_AVAILABLE_FOR_REPETITION, start, self.pos() - start, &[]);
                    }
                    is_previous_term_quantifiable = false;
                }
                0x2E /* . */ => {
                    self.inc_pos(1);
                    is_previous_term_quantifiable = true;
                }
                0x5B /* [ */ => {
                    self.inc_pos(1);
                    if self.unicode_sets_mode {
                        self.scan_class_set_expression();
                    } else {
                        self.scan_class_ranges();
                        self.pending_low_surrogate = None;
                    }
                    self.scan_expected_char(b']');
                    is_previous_term_quantifiable = true;
                }
                0x29 | 0x5D | 0x7D /* ) ] } */ => {
                    if ch == b')' && is_in_group {
                        return;
                    }
                    if self.any_unicode_mode_or_non_annex_b || ch == b')' {
                        let s = rune_string(u32::from(ch));
                        self.error(&diagnostics::UNEXPECTED_0_DID_YOU_MEAN_TO_ESCAPE_IT_WITH_BACKSLASH, self.pos(), 1, &[&s]);
                    }
                    self.inc_pos(1);
                    is_previous_term_quantifiable = true;
                }
                0x2F | 0x7C /* / | */ => {
                    return;
                }
                _ => {
                    self.scan_source_character();
                    is_previous_term_quantifiable = true;
                }
            }
        }
    }

    fn scan_pattern_modifiers(
        &mut self,
        curr_flags: RegularExpressionFlags,
    ) -> RegularExpressionFlags {
        let mut curr_flags = curr_flags;
        while self.pos() < self.end {
            let (ch, size) = decode_rune(&self.text().as_bytes()[self.pos() as usize..]);
            let size = size as u32;
            if ch == RUNE_ERROR || !is_identifier_part(ch) {
                break;
            }
            match char_code_to_reg_exp_flag(ch) {
                None => {
                    self.error(
                        &diagnostics::UNKNOWN_REGULAR_EXPRESSION_FLAG,
                        self.pos(),
                        size,
                        &[],
                    );
                }
                Some(flag) if curr_flags.intersects(flag) => {
                    self.error(
                        &diagnostics::DUPLICATE_REGULAR_EXPRESSION_FLAG,
                        self.pos(),
                        size,
                        &[],
                    );
                }
                Some(flag) if !flag.intersects(RegularExpressionFlags::Modifiers) => {
                    self.error(&diagnostics::THIS_REGULAR_EXPRESSION_FLAG_CANNOT_BE_TOGGLED_WITHIN_A_SUBPATTERN, self.pos(), size, &[]);
                }
                Some(flag) => {
                    // Modifier syntax itself requires ES2025, which is later than any flag that can appear
                    // here, so the group's own diagnostic already covers availability.
                    curr_flags |= flag;
                }
            }
            self.inc_pos(size);
        }
        curr_flags
    }

    // AtomEscape ::=
    //
    //	| DecimalEscape
    //	| CharacterClassEscape
    //	| CharacterEscape
    //	| 'k<' RegExpIdentifierName '>'
    fn scan_atom_escape(&mut self) {
        assert!(self.pos() > 0 && self.text_byte(self.pos() - 1) == b'\\');
        let ch = self.char();
        if ch == Some(b'k') {
            self.inc_pos(1);
            if self.char() == Some(b'<') {
                self.inc_pos(1);
                self.scan_group_name(true /*isReference*/);
                self.scan_expected_char(b'>');
            } else if self.any_unicode_mode_or_non_annex_b || self.named_capture_groups {
                self.error(
                    &diagnostics::X_K_MUST_BE_FOLLOWED_BY_A_CAPTURING_GROUP_NAME_ENCLOSED_IN_ANGLE_BRACKETS,
                    self.pos() - 2,
                    2,
                    &[],
                );
            }
            return;
        }
        if ch == Some(b'q') && self.unicode_sets_mode {
            self.inc_pos(1);
            self.error(
                &diagnostics::X_Q_IS_ONLY_AVAILABLE_INSIDE_CHARACTER_CLASS,
                self.pos() - 2,
                2,
                &[],
            );
            return;
        }
        if !self.scan_character_class_escape() && !self.scan_decimal_escape() {
            // Regex literals cannot contain line breaks here, so a character escape must consume something.
            let escaped = self.scan_character_escape(true /*atomEscape*/);
            assert!(!escaped.is_empty());
        }
    }

    // DecimalEscape ::= [1-9] [0-9]*
    fn scan_decimal_escape(&mut self) -> bool {
        assert!(self.pos() > 0 && self.text_byte(self.pos() - 1) == b'\\');
        let ch = self.char();
        if ch.is_some_and(|ch| ch.is_ascii_digit() && ch != b'0') {
            let start = self.pos();
            self.scan_digits();
            let val = self.scanner.state.token_value.parse::<i64>().unwrap_or(i64::MAX);
            self.decimal_escapes.push(DecimalEscapeValue {
                pos: start,
                end: self.pos(),
                value: val,
            });
            return true;
        }
        false
    }

    // CharacterEscape ::=
    //
    //	| `c` ControlLetter
    //	| IdentityEscape
    //	| (Other sequences handled by `scanEscapeSequence`)
    //
    // IdentityEscape ::=
    //
    //	| '^' | '$' | '/' | '\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
    //	| [~AnyUnicodeMode] (any other non-identifier characters)
    fn scan_character_escape(&mut self, atom_escape: bool) -> Cow<'static, str> {
        assert!(self.pos() > 0 && self.text_byte(self.pos() - 1) == b'\\');
        let ch = self.char();
        match ch {
            None => {
                self.error(&diagnostics::UNDETERMINED_CHARACTER_ESCAPE, self.pos() - 1, 1, &[]);
                Cow::Borrowed("\\")
            }
            Some(0x63) /* c */ => {
                self.inc_pos(1);
                let ch = self.char();
                if let Some(ch) = ch.filter(|&ch| stringutil::is_ascii_letter(u32::from(ch))) {
                    self.inc_pos(1);
                    return Cow::Owned(rune_string(u32::from(ch & 0x1f)));
                }
                if self.any_unicode_mode_or_non_annex_b {
                    self.error(&diagnostics::X_C_MUST_BE_FOLLOWED_BY_AN_ASCII_LETTER, self.pos() - 2, 2, &[]);
                } else if atom_escape {
                    self.dec_pos(1);
                    return Cow::Borrowed("\\");
                }
                Cow::Owned(ch.map_or_else(|| rune_string(RUNE_ERROR), |ch| rune_string(u32::from(ch))))
            }
            Some(ch @ (0x5E | 0x24 | 0x2F | 0x5C | 0x2E | 0x2A | 0x2B | 0x3F | 0x28 | 0x29 | 0x5B | 0x5D | 0x7B | 0x7D | 0x7C)) => {
                self.inc_pos(1);
                Cow::Owned(rune_string(u32::from(ch)))
            }
            _ => {
                self.dec_pos(1); // back up to include the backslash for scanEscapeSequence
                let mut flags = EscapeSequenceScanningFlags::RegularExpression;
                if self.annex_b {
                    flags |= EscapeSequenceScanningFlags::AnnexB;
                }
                if self.any_unicode_mode {
                    flags |= EscapeSequenceScanningFlags::AnyUnicodeMode;
                }
                if atom_escape {
                    flags |= EscapeSequenceScanningFlags::AtomEscape;
                }
                self.scanner.scan_escape_sequence(flags)
            }
        }
    }

    fn scan_group_name(&mut self, is_reference: bool) {
        assert!(self.pos() > 0 && self.text_byte(self.pos() - 1) == b'<');
        self.scanner.state.token_start = self.pos();
        if !self.scanner.scan_identifier(0, IdentifierVariant::RegExpGroupName) {
            self.error(&diagnostics::EXPECTED_A_CAPTURING_GROUP_NAME, self.pos(), 0, &[]);
        } else if is_reference {
            self.group_name_references.push(GroupNameReference {
                pos: self.scanner.state.token_start,
                end: self.pos(),
                name: self.scanner.state.token_value,
            });
        } else if self.named_capturing_groups_contains(self.scanner.state.token_value) {
            self.error(
                &diagnostics::NAMED_CAPTURING_GROUPS_WITH_THE_SAME_NAME_MUST_BE_MUTUALLY_EXCLUSIVE_TO_EACH_OTHER,
                self.scanner.state.token_start,
                self.pos() - self.scanner.state.token_start,
                &[],
            );
        } else {
            let token_value = self.scanner.state.token_value;
            // A previous definition can only have come from a mutually exclusive alternative.
            // Below ES2018 the group itself is already reported, so don't stack a second error on it.
            if self.group_specifiers.contains(token_value)
                && self.scanner.language_version() >= ScriptTarget::ES2018
                && self.scanner.language_version() < ScriptTarget::ES2025
            {
                let name = ScriptTarget::ES2025.string().to_lowercase();
                self.error(
                    &diagnostics::DUPLICATE_NAMED_CAPTURING_GROUPS_ARE_ONLY_AVAILABLE_WHEN_TARGETING_0_OR_LATER,
                    self.scanner.state.token_start,
                    self.pos() - self.scanner.state.token_start,
                    &[&name],
                );
            }
            if let Some(scope) = self.named_capturing_groups.last_mut() {
                add_unique(scope, token_value);
            }
            if self.group_specifiers.insert(token_value) {
                self.group_specifiers_order.push(token_value);
            }
        }
    }

    fn named_capturing_groups_contains(&self, name: &str) -> bool {
        self.named_capturing_groups.iter().any(|group| group.contains(&name))
    }

    fn is_class_content_exit(&self, ch: Option<u8>) -> bool {
        ch == Some(b']') || self.pos() >= self.end
    }

    // ClassRanges ::= '^'? (ClassAtom ('-' ClassAtom)?)*
    fn scan_class_ranges(&mut self) {
        assert!(self.pos() > 0 && self.text_byte(self.pos() - 1) == b'[');
        self.pending_low_surrogate = None;
        if self.char() == Some(b'^') {
            self.inc_pos(1);
        }
        while self.pos() < self.end {
            let mut ch = self.char();
            if self.is_class_content_exit(ch) {
                return;
            }
            let min_start = self.pos();
            let min_character = self.scan_class_atom();
            if self.char() == Some(b'-') {
                self.inc_pos(1);
                ch = self.char();
                if self.is_class_content_exit(ch) {
                    return;
                }
                if min_character.is_empty() && self.any_unicode_mode_or_non_annex_b {
                    self.error(
                        &diagnostics::A_CHARACTER_CLASS_RANGE_MUST_NOT_BE_BOUNDED_BY_ANOTHER_CHARACTER_CLASS,
                        min_start,
                        self.pos() - 1 - min_start,
                        &[],
                    );
                }
                let max_start = self.pos();
                let max_character = self.scan_class_atom();
                if max_character.is_empty() && self.any_unicode_mode_or_non_annex_b {
                    self.error(
                        &diagnostics::A_CHARACTER_CLASS_RANGE_MUST_NOT_BE_BOUNDED_BY_ANOTHER_CHARACTER_CLASS,
                        max_start,
                        self.pos() - max_start,
                        &[],
                    );
                    continue;
                }
                if min_character.is_empty() {
                    continue;
                }
                let (min_character_value, min_size) =
                    stringutil::decode_js_string_rune(&min_character);
                let (max_character_value, max_size) =
                    stringutil::decode_js_string_rune(&max_character);
                if min_character.len() == min_size
                    && max_character.len() == max_size
                    && min_character_value > max_character_value
                {
                    self.error(
                        &diagnostics::RANGE_OUT_OF_ORDER_IN_CHARACTER_CLASS,
                        min_start,
                        self.pos() - min_start,
                        &[],
                    );
                }
            }
        }
    }

    // Static Semantics: MayContainStrings
    //     ClassUnion: ClassSetOperands.some(ClassSetOperand => ClassSetOperand.MayContainStrings)
    //     ClassIntersection: ClassSetOperands.every(ClassSetOperand => ClassSetOperand.MayContainStrings)
    //     ClassSubtraction: ClassSetOperands[0].MayContainStrings
    //     ClassSetOperand:
    //         || ClassStringDisjunctionContents.MayContainStrings
    //         || CharacterClassEscape.UnicodePropertyValueExpression.LoneUnicodePropertyNameOrValue.MayContainStrings
    //     ClassStringDisjunctionContents: ClassStrings.some(ClassString => ClassString.ClassSetCharacters.length !== 1)
    //     LoneUnicodePropertyNameOrValue: isBinaryUnicodePropertyOfStrings(LoneUnicodePropertyNameOrValue)

    // ClassSetExpression ::= '^'? (ClassUnion | ClassIntersection | ClassSubtraction)
    // ClassUnion ::= (ClassSetRange | ClassSetOperand)*
    // ClassIntersection ::= ClassSetOperand ('&&' ClassSetOperand)+
    // ClassSubtraction ::= ClassSetOperand ('--' ClassSetOperand)+
    // ClassSetRange ::= ClassSetCharacter '-' ClassSetCharacter
    fn scan_class_set_expression(&mut self) {
        assert!(self.pos() > 0 && self.text_byte(self.pos() - 1) == b'[');
        let mut is_character_complement = false;
        if self.char() == Some(b'^') {
            self.inc_pos(1);
            is_character_complement = true;
        }
        let mut expression_may_contain_strings = false;
        let mut ch = self.char();
        if self.is_class_content_exit(ch) {
            return;
        }
        let mut start = self.pos();
        let mut operand: Cow<'static, str> = Cow::Borrowed("");
        let mut two_chars: &[u8] = b"";
        if self.pos() + 1 < self.end {
            two_chars = &self.text().as_bytes()[self.pos() as usize..(self.pos() + 2) as usize];
        }
        match two_chars {
            b"--" | b"&&" => {
                self.error(&diagnostics::EXPECTED_A_CLASS_SET_OPERAND, self.pos(), 0, &[]);
                self.may_contain_strings = false;
            }
            _ => {
                operand = self.scan_class_set_operand();
            }
        }
        match self.char() {
            Some(0x2D) /* - */ => {
                if self.pos() + 1 < self.end && self.char_at(self.pos() + 1) == Some(b'-') {
                    if is_character_complement && self.may_contain_strings {
                        self.error(
                            &diagnostics::ANYTHING_THAT_WOULD_POSSIBLY_MATCH_MORE_THAN_A_SINGLE_CHARACTER_IS_INVALID_INSIDE_A_NEGATED_CHARACTER_CLASS,
                            start,
                            self.pos() - start,
                            &[],
                        );
                    }
                    expression_may_contain_strings = self.may_contain_strings;
                    self.scan_class_set_sub_expression(ClassSetExpressionType::ClassSubtraction);
                    self.may_contain_strings = !is_character_complement && expression_may_contain_strings;
                    return;
                }
            }
            Some(0x26) /* & */ => {
                if self.pos() + 1 < self.end && self.char_at(self.pos() + 1) == Some(b'&') {
                    self.scan_class_set_sub_expression(ClassSetExpressionType::ClassIntersection);
                    if is_character_complement && self.may_contain_strings {
                        self.error(
                            &diagnostics::ANYTHING_THAT_WOULD_POSSIBLY_MATCH_MORE_THAN_A_SINGLE_CHARACTER_IS_INVALID_INSIDE_A_NEGATED_CHARACTER_CLASS,
                            start,
                            self.pos() - start,
                            &[],
                        );
                    }
                    expression_may_contain_strings = self.may_contain_strings;
                    self.may_contain_strings = !is_character_complement && expression_may_contain_strings;
                    return;
                }
            }
            _ => {
                if is_character_complement && self.may_contain_strings {
                    self.error(
                        &diagnostics::ANYTHING_THAT_WOULD_POSSIBLY_MATCH_MORE_THAN_A_SINGLE_CHARACTER_IS_INVALID_INSIDE_A_NEGATED_CHARACTER_CLASS,
                        start,
                        self.pos() - start,
                        &[],
                    );
                }
                expression_may_contain_strings = self.may_contain_strings;
            }
        }
        while self.pos() < self.end {
            ch = self.char();
            if ch == Some(b'-') {
                self.inc_pos(1);
                ch = self.char();
                if self.is_class_content_exit(ch) {
                    self.may_contain_strings =
                        !is_character_complement && expression_may_contain_strings;
                    return;
                }
                if ch == Some(b'-') {
                    self.inc_pos(1);
                    self.error(
                        &diagnostics::OPERATORS_MUST_NOT_BE_MIXED_WITHIN_A_CHARACTER_CLASS_WRAP_IT_IN_A_NESTED_CLASS_INSTEAD,
                        self.pos() - 2,
                        2,
                        &[],
                    );
                    start = self.pos() - 2;
                    operand = Cow::Borrowed(&self.text()[start as usize..self.pos() as usize]);
                    continue;
                }
                if operand.is_empty() {
                    self.error(
                        &diagnostics::A_CHARACTER_CLASS_RANGE_MUST_NOT_BE_BOUNDED_BY_ANOTHER_CHARACTER_CLASS,
                        start,
                        self.pos() - 1 - start,
                        &[],
                    );
                }
                let second_start = self.pos();
                let second_operand = self.scan_class_set_operand();
                if is_character_complement && self.may_contain_strings {
                    self.error(
                        &diagnostics::ANYTHING_THAT_WOULD_POSSIBLY_MATCH_MORE_THAN_A_SINGLE_CHARACTER_IS_INVALID_INSIDE_A_NEGATED_CHARACTER_CLASS,
                        second_start,
                        self.pos() - second_start,
                        &[],
                    );
                }
                expression_may_contain_strings =
                    expression_may_contain_strings || self.may_contain_strings;
                if second_operand.is_empty() {
                    self.error(
                        &diagnostics::A_CHARACTER_CLASS_RANGE_MUST_NOT_BE_BOUNDED_BY_ANOTHER_CHARACTER_CLASS,
                        second_start,
                        self.pos() - second_start,
                        &[],
                    );
                } else if !operand.is_empty() {
                    let (min_character_value, min_size) =
                        stringutil::decode_js_string_rune(&operand);
                    let (max_character_value, max_size) =
                        stringutil::decode_js_string_rune(&second_operand);
                    if operand.len() == min_size
                        && second_operand.len() == max_size
                        && min_character_value > max_character_value
                    {
                        self.error(
                            &diagnostics::RANGE_OUT_OF_ORDER_IN_CHARACTER_CLASS,
                            start,
                            self.pos() - start,
                            &[],
                        );
                    }
                }
            } else if ch == Some(b'&')
                && self.pos() + 1 < self.end
                && self.char_at(self.pos() + 1) == Some(b'&')
            {
                start = self.pos();
                self.inc_pos(2);
                self.error(
                        &diagnostics::OPERATORS_MUST_NOT_BE_MIXED_WITHIN_A_CHARACTER_CLASS_WRAP_IT_IN_A_NESTED_CLASS_INSTEAD,
                        self.pos() - 2,
                        2,
                        &[],
                    );
                if self.char() == Some(b'&') {
                    let s = rune_string(u32::from(ch.expect("class content is not at end")));
                    self.error(
                        &diagnostics::UNEXPECTED_0_DID_YOU_MEAN_TO_ESCAPE_IT_WITH_BACKSLASH,
                        self.pos(),
                        1,
                        &[&s],
                    );
                    self.inc_pos(1);
                }
                operand = Cow::Borrowed(&self.text()[start as usize..self.pos() as usize]);
                continue;
            }
            if self.is_class_content_exit(self.char()) {
                break;
            }
            start = self.pos();
            two_chars = b"";
            if self.pos() + 1 < self.end {
                two_chars = &self.text().as_bytes()[self.pos() as usize..(self.pos() + 2) as usize];
            }
            match two_chars {
                b"--" | b"&&" => {
                    self.error(
                        &diagnostics::OPERATORS_MUST_NOT_BE_MIXED_WITHIN_A_CHARACTER_CLASS_WRAP_IT_IN_A_NESTED_CLASS_INSTEAD,
                        self.pos(),
                        2,
                        &[],
                    );
                    self.inc_pos(2);
                    operand = Cow::Borrowed(&self.text()[start as usize..self.pos() as usize]);
                }
                _ => {
                    operand = self.scan_class_set_operand();
                    if is_character_complement && self.may_contain_strings {
                        self.error(
                            &diagnostics::ANYTHING_THAT_WOULD_POSSIBLY_MATCH_MORE_THAN_A_SINGLE_CHARACTER_IS_INVALID_INSIDE_A_NEGATED_CHARACTER_CLASS,
                            start,
                            self.pos() - start,
                            &[],
                        );
                    }
                    expression_may_contain_strings =
                        expression_may_contain_strings || self.may_contain_strings;
                }
            }
        }
        self.may_contain_strings = !is_character_complement && expression_may_contain_strings;
    }

    fn scan_class_set_sub_expression(&mut self, expression_type: ClassSetExpressionType) {
        let mut expression_may_contain_strings = self.may_contain_strings;
        while self.pos() < self.end {
            let mut ch = self.char();
            if self.is_class_content_exit(ch) {
                break;
            }
            match ch {
                Some(0x2D) /* - */ => {
                    self.inc_pos(1);
                    if self.char() == Some(b'-') {
                        self.inc_pos(1);
                        if expression_type != ClassSetExpressionType::ClassSubtraction {
                            self.error(
                                &diagnostics::OPERATORS_MUST_NOT_BE_MIXED_WITHIN_A_CHARACTER_CLASS_WRAP_IT_IN_A_NESTED_CLASS_INSTEAD,
                                self.pos() - 2,
                                2,
                                &[],
                            );
                        }
                    } else {
                        self.error(
                            &diagnostics::OPERATORS_MUST_NOT_BE_MIXED_WITHIN_A_CHARACTER_CLASS_WRAP_IT_IN_A_NESTED_CLASS_INSTEAD,
                            self.pos() - 1,
                            1,
                            &[],
                        );
                    }
                }
                Some(0x26) /* & */ => {
                    self.inc_pos(1);
                    if self.char() == Some(b'&') {
                        self.inc_pos(1);
                        if expression_type != ClassSetExpressionType::ClassIntersection {
                            self.error(
                                &diagnostics::OPERATORS_MUST_NOT_BE_MIXED_WITHIN_A_CHARACTER_CLASS_WRAP_IT_IN_A_NESTED_CLASS_INSTEAD,
                                self.pos() - 2,
                                2,
                                &[],
                            );
                        }
                        if self.char() == Some(b'&') {
                            let s = rune_string(u32::from(ch.expect("class content is not at end")));
                            self.error(&diagnostics::UNEXPECTED_0_DID_YOU_MEAN_TO_ESCAPE_IT_WITH_BACKSLASH, self.pos(), 1, &[&s]);
                            self.inc_pos(1);
                        }
                    } else {
                        let s = rune_string(u32::from(ch.expect("class content is not at end")));
                        self.error(&diagnostics::UNEXPECTED_0_DID_YOU_MEAN_TO_ESCAPE_IT_WITH_BACKSLASH, self.pos() - 1, 1, &[&s]);
                    }
                }
                _ => match expression_type {
                    ClassSetExpressionType::ClassSubtraction => {
                        self.error(&diagnostics::X_0_EXPECTED, self.pos(), 0, &[&"--"]);
                    }
                    ClassSetExpressionType::ClassIntersection => {
                        self.error(&diagnostics::X_0_EXPECTED, self.pos(), 0, &[&"&&"]);
                    }
                },
            }
            ch = self.char();
            if self.is_class_content_exit(ch) {
                self.error(&diagnostics::EXPECTED_A_CLASS_SET_OPERAND, self.pos(), 0, &[]);
                break;
            }
            self.scan_class_set_operand();
            if expression_type == ClassSetExpressionType::ClassIntersection {
                expression_may_contain_strings =
                    expression_may_contain_strings && self.may_contain_strings;
            }
        }
        self.may_contain_strings = expression_may_contain_strings;
    }

    // ClassSetOperand ::=
    //
    //	| '[' ClassSetExpression ']'
    //	| '\' CharacterClassEscape
    //	| '\q{' ClassStringDisjunctionContents '}'
    //	| ClassSetCharacter
    fn scan_class_set_operand(&mut self) -> Cow<'static, str> {
        self.may_contain_strings = false;
        match self.char() {
            Some(0x5B) /* [ */ => {
                self.inc_pos(1);
                self.scan_class_set_expression();
                self.scan_expected_char(b']');
                Cow::Borrowed("")
            }
            Some(0x5C) /* \ */ => {
                self.inc_pos(1);
                if self.scan_character_class_escape() {
                    return Cow::Borrowed("");
                } else if self.char() == Some(b'q') {
                    self.inc_pos(1);
                    if self.char() == Some(b'{') {
                        self.inc_pos(1);
                        self.scan_class_string_disjunction_contents();
                        self.scan_expected_char(b'}');
                        return Cow::Borrowed("");
                    }
                    self.error(&diagnostics::X_Q_MUST_BE_FOLLOWED_BY_STRING_ALTERNATIVES_ENCLOSED_IN_BRACES, self.pos() - 2, 2, &[]);
                    return Cow::Borrowed("q");
                }
                self.dec_pos(1);
                self.scan_class_set_character()
            }
            _ => self.scan_class_set_character(),
        }
    }

    // ClassStringDisjunctionContents ::= ClassSetCharacter* ('|' ClassSetCharacter*)*
    fn scan_class_string_disjunction_contents(&mut self) {
        assert!(self.pos() > 0 && self.text_byte(self.pos() - 1) == b'{');
        let mut character_count = 0;
        while self.pos() < self.end {
            let ch = self.char().expect("position is before regexp end");
            match ch {
                0x7D /* } */ => {
                    if character_count != 1 {
                        self.may_contain_strings = true;
                    }
                    return;
                }
                0x7C /* | */ => {
                    if character_count != 1 {
                        self.may_contain_strings = true;
                    }
                    self.inc_pos(1);
                    character_count = 0;
                }
                _ => {
                    self.scan_class_set_character();
                    character_count += 1;
                }
            }
        }
    }

    // ClassSetCharacter ::=
    //
    //	| SourceCharacter -- ClassSetSyntaxCharacter -- ClassSetReservedDoublePunctuator
    //	| '\' (CharacterEscape | ClassSetReservedPunctuator | 'b')
    fn scan_class_set_character(&mut self) -> Cow<'static, str> {
        let ch = self.char();
        if ch == Some(b'\\') {
            self.inc_pos(1);
            let inner_ch = self.char();
            match inner_ch {
                Some(0x62) /* b */ => {
                    self.inc_pos(1);
                    return Cow::Borrowed("\u{8}");
                }
                Some(inner_ch @ (0x26 | 0x2D | 0x21 | 0x23 | 0x25 | 0x2C | 0x3A | 0x3B | 0x3C | 0x3D | 0x3E | 0x40 | 0x60 | 0x7E)) => {
                    self.inc_pos(1);
                    return Cow::Owned(rune_string(u32::from(inner_ch)));
                }
                _ => return self.scan_character_escape(false /*atomEscape*/),
            }
        } else if self.pos() + 1 < self.end
            && ch == self.char_at(self.pos() + 1)
            && let Some(
                0x26 | 0x21 | 0x23 | 0x25 | 0x2A | 0x2B | 0x2C | 0x2E | 0x3A | 0x3B | 0x3C | 0x3D
                | 0x3E | 0x3F | 0x40 | 0x60 | 0x7E,
            ) = ch
        {
            self.error(
                    &diagnostics::A_CHARACTER_CLASS_MUST_NOT_CONTAIN_A_RESERVED_DOUBLE_PUNCTUATOR_DID_YOU_MEAN_TO_ESCAPE_IT_WITH_BACKSLASH,
                    self.pos(),
                    2,
                    &[],
                );
            self.inc_pos(2);
            return Cow::Borrowed(&self.text()[(self.pos() - 2) as usize..self.pos() as usize]);
        }
        if let Some(ch @ (0x2F | 0x28 | 0x29 | 0x5B | 0x5D | 0x7B | 0x7D | 0x2D | 0x7C)) = ch {
            let s = rune_string(u32::from(ch));
            self.error(
                &diagnostics::UNEXPECTED_0_DID_YOU_MEAN_TO_ESCAPE_IT_WITH_BACKSLASH,
                self.pos(),
                1,
                &[&s],
            );
            self.inc_pos(1);
            return Cow::Owned(s);
        }
        self.scan_source_character()
    }

    // ClassAtom ::=
    //
    //	| SourceCharacter but not one of '\' or ']'
    //	| '\' ClassEscape
    //
    // ClassEscape ::=
    //
    //	| 'b'
    //	| '-'
    //	| CharacterClassEscape
    //	| CharacterEscape
    fn scan_class_atom(&mut self) -> Cow<'static, str> {
        if self.char() == Some(b'\\') {
            self.inc_pos(1);
            let ch = self.char().expect("position is before regexp end");
            match ch {
                0x62 /* b */ => {
                    self.inc_pos(1);
                    Cow::Borrowed("\u{8}")
                }
                0x2D /* - */ => {
                    self.inc_pos(1);
                    Cow::Owned(rune_string(u32::from(b'-')))
                }
                _ => {
                    if self.scan_character_class_escape() {
                        return Cow::Borrowed("");
                    }
                    self.scan_character_escape(false /*atomEscape*/)
                }
            }
        } else {
            self.scan_source_character()
        }
    }

    // CharacterClassEscape ::=
    //
    //	| 'd' | 'D' | 's' | 'S' | 'w' | 'W'
    //	| [+AnyUnicodeMode] ('P' | 'p') '{' UnicodePropertyValueExpression '}'
    fn scan_character_class_escape(&mut self) -> bool {
        assert!(self.pos() > 0 && self.text_byte(self.pos() - 1) == b'\\');
        let mut is_character_complement = false;
        let start = self.pos() - 1;
        let ch = self.char();
        match ch {
            Some(0x64 | 0x44 | 0x73 | 0x53 | 0x77 | 0x57) /* d D s S w W */ => {
                self.inc_pos(1);
                true
            }
            Some(ch @ (0x50 | 0x70)) /* P p */ => {
                if ch == b'P' {
                    is_character_complement = true;
                }
                self.inc_pos(1);
                if self.char() == Some(b'{') {
                    self.inc_pos(1);
                    let property_name_or_value_start = self.pos();
                    let property_name_or_value = self.scan_word_characters();
                    if self.char() == Some(b'=') {
                        let property_name = non_binary_unicode_properties(property_name_or_value).unwrap_or("");
                        if self.pos() == property_name_or_value_start {
                            self.error(&diagnostics::EXPECTED_A_UNICODE_PROPERTY_NAME, self.pos(), 0, &[]);
                        } else if property_name.is_empty() {
                            self.error(
                                &diagnostics::UNKNOWN_UNICODE_PROPERTY_NAME,
                                property_name_or_value_start,
                                self.pos() - property_name_or_value_start,
                                &[],
                            );
                            let suggestion = Self::get_spelling_suggestion_for_unicode_property_name(property_name_or_value);
                            if !suggestion.is_empty() {
                                self.error(
                                    &diagnostics::DID_YOU_MEAN_0,
                                    property_name_or_value_start,
                                    self.pos() - property_name_or_value_start,
                                    &[&suggestion],
                                );
                            }
                        }
                        self.inc_pos(1);
                        let property_value_start = self.pos();
                        let property_value = self.scan_word_characters();
                        if self.pos() == property_value_start {
                            self.error(&diagnostics::EXPECTED_A_UNICODE_PROPERTY_VALUE, self.pos(), 0, &[]);
                        } else if !property_name.is_empty()
                            && let Some(values) = values_of_non_binary_unicode_properties(property_name)
                                && !values.contains(property_value) {
                                    self.error(
                                        &diagnostics::UNKNOWN_UNICODE_PROPERTY_VALUE,
                                        property_value_start,
                                        self.pos() - property_value_start,
                                        &[],
                                    );
                                    let suggestion = Self::get_spelling_suggestion_for_unicode_property_value(property_name, property_value);
                                    if !suggestion.is_empty() {
                                        self.error(
                                            &diagnostics::DID_YOU_MEAN_0,
                                            property_value_start,
                                            self.pos() - property_value_start,
                                            &[&suggestion],
                                        );
                                    }
                                }
                    } else if self.pos() == property_name_or_value_start {
                        self.error(&diagnostics::EXPECTED_A_UNICODE_PROPERTY_NAME_OR_VALUE, self.pos(), 0, &[]);
                    } else if binary_unicode_properties_of_strings().contains(property_name_or_value) {
                        if !self.unicode_sets_mode {
                            self.error(
                                &diagnostics::ANY_UNICODE_PROPERTY_THAT_WOULD_POSSIBLY_MATCH_MORE_THAN_A_SINGLE_CHARACTER_IS_ONLY_AVAILABLE_WHEN_THE_UNICODE_SETS_V_FLAG_IS_SET,
                                property_name_or_value_start,
                                self.pos() - property_name_or_value_start,
                                &[],
                            );
                        } else if is_character_complement {
                            self.error(
                                &diagnostics::ANYTHING_THAT_WOULD_POSSIBLY_MATCH_MORE_THAN_A_SINGLE_CHARACTER_IS_INVALID_INSIDE_A_NEGATED_CHARACTER_CLASS,
                                property_name_or_value_start,
                                self.pos() - property_name_or_value_start,
                                &[],
                            );
                        } else {
                            self.may_contain_strings = true;
                        }
                    } else if !general_category_values().contains(property_name_or_value) && !binary_unicode_properties().contains(property_name_or_value) {
                        self.error(
                            &diagnostics::UNKNOWN_UNICODE_PROPERTY_NAME_OR_VALUE,
                            property_name_or_value_start,
                            self.pos() - property_name_or_value_start,
                            &[],
                        );
                        let suggestion = Self::get_spelling_suggestion_for_unicode_property_name_or_value(property_name_or_value);
                        if !suggestion.is_empty() {
                            self.error(
                                &diagnostics::DID_YOU_MEAN_0,
                                property_name_or_value_start,
                                self.pos() - property_name_or_value_start,
                                &[&suggestion],
                            );
                        }
                    }
                    self.scan_expected_char(b'}');
                    if !self.any_unicode_mode {
                        self.error(
                            &diagnostics::UNICODE_PROPERTY_VALUE_EXPRESSIONS_ARE_ONLY_AVAILABLE_WHEN_THE_UNICODE_U_FLAG_OR_THE_UNICODE_SETS_V_FLAG_IS_SET,
                            start,
                            self.pos() - start,
                            &[],
                        );
                    }
                } else if self.any_unicode_mode_or_non_annex_b {
                    let s = rune_string(u32::from(ch));
                    self.error(
                        &diagnostics::X_0_MUST_BE_FOLLOWED_BY_A_UNICODE_PROPERTY_VALUE_EXPRESSION_ENCLOSED_IN_BRACES,
                        self.pos() - 2,
                        2,
                        &[&s],
                    );
                } else {
                    self.dec_pos(1);
                    return false;
                }
                true
            }
            _ => false,
        }
    }

    fn get_spelling_suggestion_for_unicode_property_name(name: &str) -> String {
        tsrs_core::get_spelling_suggestion_for_strings(
            name,
            NON_BINARY_UNICODE_PROPERTIES.iter().map(|&(k, _)| k),
        )
        .map(std::string::ToString::to_string)
        .unwrap_or_default()
    }

    fn get_spelling_suggestion_for_unicode_property_value(
        property_name: &str,
        value: &str,
    ) -> String {
        let Some(values) =
            crate::unicodeproperties::values_list_of_non_binary_unicode_properties(property_name)
        else {
            return String::new();
        };
        tsrs_core::get_spelling_suggestion_for_strings(value, values.iter().copied())
            .map(std::string::ToString::to_string)
            .unwrap_or_default()
    }

    fn get_spelling_suggestion_for_unicode_property_name_or_value(name: &str) -> String {
        tsrs_core::get_spelling_suggestion_for_strings(
            name,
            GENERAL_CATEGORY_VALUES
                .iter()
                .copied()
                .chain(BINARY_UNICODE_PROPERTIES.iter().copied())
                .chain(BINARY_UNICODE_PROPERTIES_OF_STRINGS.iter().copied()),
        )
        .map(std::string::ToString::to_string)
        .unwrap_or_default()
    }

    fn scan_word_characters(&mut self) -> &'static str {
        let start = self.pos();
        while self.pos() < self.end {
            let ch = self.char().expect("position is before regexp end");
            if !is_word_character(u32::from(ch)) {
                break;
            }
            self.inc_pos(1);
        }
        &self.text()[start as usize..self.pos() as usize]
    }

    fn scan_source_character(&mut self) -> Cow<'static, str> {
        if self.pos() >= self.end {
            return Cow::Borrowed("");
        }
        let text = self.text();
        if !self.any_unicode_mode {
            if let Some(low) = self.pending_low_surrogate.take() {
                // Second of two surrogate code units for the same non-BMP character.
                // Now advance past the full UTF-8 sequence (the high surrogate call did not advance).
                let (_, size) = decode_rune(&text.as_bytes()[self.pos() as usize..]);
                self.inc_pos(size as u32);
                return Cow::Owned(stringutil::encode_js_string_rune(low));
            }
            let (ch, size) = decode_rune(&text.as_bytes()[self.pos() as usize..]);
            if ch == RUNE_ERROR || size == 0 {
                // Not a valid rune; consume one raw byte.
                self.inc_pos(1);
                let p = (self.pos() - 1) as usize;
                return match text.get(p..p + 1) {
                    Some(s) => Cow::Borrowed(s),
                    // A multi-byte U+FFFD in valid UTF-8 text: Go returns its first byte.
                    None => Cow::Owned(rune_string(RUNE_ERROR)),
                };
            }
            if ch >= 0x10000 {
                // Non-BMP character: emit the high surrogate first WITHOUT advancing.
                // The low surrogate will be emitted on the next call, which also advances.
                let (high, low) = stringutil::code_point_to_surrogate_pair(ch);
                self.pending_low_surrogate = Some(low);
                return Cow::Owned(stringutil::encode_js_string_rune(high));
            }
            self.inc_pos(size as u32);
            return Cow::Borrowed(&text[(self.pos() - size as u32) as usize..self.pos() as usize]);
        }
        let (ch, size) = decode_rune(&text.as_bytes()[self.pos() as usize..]);
        if size == 0 {
            return Cow::Borrowed("");
        }
        if ch == RUNE_ERROR {
            // Invalid UTF-8; consume the byte to avoid infinite loops.
            self.inc_pos(size as u32);
            return Cow::Borrowed("");
        }
        self.inc_pos(size as u32);
        Cow::Borrowed(&text[(self.pos() - size as u32) as usize..self.pos() as usize])
    }

    fn scan_expected_char(&mut self, ch: u8) {
        if self.char() == Some(ch) {
            self.inc_pos(1);
        } else {
            let s = rune_string(u32::from(ch));
            self.error(&diagnostics::X_0_EXPECTED, self.pos(), 0, &[&s]);
        }
    }

    fn scan_digits(&mut self) {
        let start = self.pos();
        while self.pos() < self.end
            && self.char().is_some_and(|ch| stringutil::is_digit(u32::from(ch)))
        {
            self.inc_pos(1);
        }
        self.scanner.state.token_value = &self.text()[start as usize..self.pos() as usize];
    }

    pub(crate) fn run(&mut self) {
        // Regular expressions are checked more strictly when either in 'u' or 'v' mode, or
        // when not using the looser interpretation of the syntax from ECMA-262 Annex B.
        self.any_unicode_mode_or_non_annex_b = self.any_unicode_mode || !self.annex_b;

        self.scan_disjunction(false /*isInGroup*/);

        let references = std::mem::take(&mut self.group_name_references);
        for reference in &references {
            if !self.group_specifiers.contains(reference.name) {
                self.error(
                    &diagnostics::THERE_IS_NO_CAPTURING_GROUP_NAMED_0_IN_THIS_REGULAR_EXPRESSION,
                    reference.pos,
                    reference.end - reference.pos,
                    &[&reference.name],
                );
                if !self.group_specifiers.is_empty() {
                    let suggestion = tsrs_core::get_spelling_suggestion_for_strings(
                        reference.name,
                        self.group_specifiers_order.iter().copied(),
                    )
                    .map(std::string::ToString::to_string)
                    .unwrap_or_default();
                    if !suggestion.is_empty() {
                        self.error(
                            &diagnostics::DID_YOU_MEAN_0,
                            reference.pos,
                            reference.end - reference.pos,
                            &[&suggestion],
                        );
                    }
                }
            }
        }
        let escapes = std::mem::take(&mut self.decimal_escapes);
        for escape in &escapes {
            // Although a DecimalEscape with a value greater than the number of capturing groups
            // is treated as either a LegacyOctalEscapeSequence or an IdentityEscape in Annex B,
            // an error is nevertheless reported since it's most likely a mistake.
            if escape.value > self.number_of_capturing_groups as i64 {
                if self.number_of_capturing_groups > 0 {
                    let n = self.number_of_capturing_groups;
                    self.error(
                        &diagnostics::THIS_BACKREFERENCE_REFERS_TO_A_GROUP_THAT_DOES_NOT_EXIST_THERE_ARE_ONLY_0_CAPTURING_GROUPS_IN_THIS_REGULAR_EXPRESSION,
                        escape.pos,
                        escape.end - escape.pos,
                        &[&n],
                    );
                } else {
                    self.error(
                        &diagnostics::THIS_BACKREFERENCE_REFERS_TO_A_GROUP_THAT_DOES_NOT_EXIST_THERE_ARE_NO_CAPTURING_GROUPS_IN_THIS_REGULAR_EXPRESSION,
                        escape.pos,
                        escape.end - escape.pos,
                        &[],
                    );
                }
            }
        }
    }
}
