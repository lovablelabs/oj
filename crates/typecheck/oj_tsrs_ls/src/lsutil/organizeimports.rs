use std::cmp::Ordering;
use std::rc::Rc;

use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, Kind, Node, SourceFile};
use tsrs_core::stringutil::{
    compare_strings_case_insensitive_eslint_compatible, compare_strings_case_sensitive,
    decode_rune, go_strings_to_lower, norm_nfd_string, unicode_is_mn, unicode_is_upper,
};
use tsrs_core::{
    P, Tristate, binary_search_unique_func, bool_to_tristate, compare_booleans, tspath,
};

use super::*;

pub type StringComparer = Rc<dyn Fn(&str, &str) -> Ordering>;
pub(crate) type NodeComparer = Rc<dyn Fn(P<Node>, P<Node>) -> Ordering>;

// Go `cmp.Compare` / `strings.Compare`.
fn go_cmp<T: Ord + Copy>(a: T, b: T) -> Ordering {
    a.cmp(&b)
}

// organizeimports.go:18
// FilterImportDeclarations filters out non-import declarations from a list of statements.
pub(crate) fn filter_import_declarations(statements: &[P<Node>]) -> Vec<P<Node>> {
    statements.iter().copied().filter(|stmt| stmt.kind() == Kind::ImportDeclaration).collect()
}

// organizeimports.go:25
// GetDetectionLists returns the lists of comparers and type orders to test for organize imports detection.
pub(crate) fn get_detection_lists(
    preferences: &UserPreferences,
) -> (Vec<StringComparer>, Vec<OrganizeImportsTypeOrder>) {
    let comparers_to_test: Vec<StringComparer> =
        if preferences.organize_imports_sort != OrganizeImportsSort::Auto {
            vec![get_organize_imports_preset_string_comparer(preferences.organize_imports_sort)]
        } else if !preferences.organize_imports_ignore_case.is_unknown() {
            vec![get_organize_imports_string_comparer(
                preferences,
                preferences.organize_imports_ignore_case.is_true(),
            )]
        } else {
            vec![
                get_organize_imports_string_comparer(preferences, true),
                get_organize_imports_string_comparer(preferences, false),
            ]
        };

    let type_orders_to_test =
        if preferences.organize_imports_type_order == OrganizeImportsTypeOrder::Auto {
            vec![
                OrganizeImportsTypeOrder::Last,
                OrganizeImportsTypeOrder::Inline,
                OrganizeImportsTypeOrder::First,
            ]
        } else {
            vec![preferences.organize_imports_type_order]
        };

    (comparers_to_test, type_orders_to_test)
}

// organizeimports.go:50
pub fn resolve_organize_imports_sort(preferences: &UserPreferences) -> OrganizeImportsSort {
    if preferences.organize_imports_sort != OrganizeImportsSort::Auto {
        return preferences.organize_imports_sort;
    }

    if preferences.organize_imports_collation == OrganizeImportsCollation::Unicode {
        return match preferences.organize_imports_ignore_case {
            Tristate::True => OrganizeImportsSort::NaturalIgnoreCase,
            Tristate::False => OrganizeImportsSort::Natural,
            Tristate::Unknown => OrganizeImportsSort::Auto,
        };
    }

    match preferences.organize_imports_ignore_case {
        Tristate::True => OrganizeImportsSort::OrdinalIgnoreCase,
        Tristate::False => OrganizeImportsSort::Ordinal,
        Tristate::Unknown => OrganizeImportsSort::Auto,
    }
}

// organizeimports.go:76
fn get_organize_imports_ordinal_string_comparer(ignore_case: bool) -> StringComparer {
    if ignore_case {
        return Rc::new(compare_strings_case_insensitive_eslint_compatible);
    }
    Rc::new(compare_strings_case_sensitive)
}

// organizeimports.go:83
fn get_organize_imports_natural_string_comparer(case_sensitive: bool) -> StringComparer {
    Rc::new(move |a: &str, b: &str| compare_organize_imports_natural_strings(a, b, case_sensitive))
}

// organizeimports.go:89
fn get_organize_imports_unicode_string_comparer(
    ignore_case: bool,
    preferences: &UserPreferences,
) -> StringComparer {
    let case_first = preferences.organize_imports_case_first;
    let numeric = preferences.organize_imports_numeric_collation.is_true();
    let accents = !preferences.organize_imports_accent_collation.is_false();

    Rc::new(move |a: &str, b: &str| {
        compare_organize_imports_unicode_strings(a, b, ignore_case, case_first, numeric, accents)
    })
}

// organizeimports.go:99
fn compare_organize_imports_natural_strings(a: &str, b: &str, case_sensitive: bool) -> Ordering {
    let cmp = compare_strings_numeric(&natural_collation_key(a), &natural_collation_key(b));
    if !cmp.is_eq() {
        return cmp;
    }

    if case_sensitive {
        let cmp = compare_organize_imports_case_upper_first(a, b);
        if !cmp.is_eq() {
            return cmp;
        }
    }

    go_cmp(a, b)
}

// organizeimports.go:113
fn compare_organize_imports_unicode_strings(
    a: &str,
    b: &str,
    ignore_case: bool,
    case_first: OrganizeImportsCaseFirst,
    numeric: bool,
    accents: bool,
) -> Ordering {
    let cmp = compare_organize_imports_unicode_keys(
        &natural_collation_key(a),
        &natural_collation_key(b),
        numeric,
    );
    if !cmp.is_eq() {
        return cmp;
    }

    if accents {
        let cmp = compare_organize_imports_unicode_keys(
            &go_strings_to_lower(a),
            &go_strings_to_lower(b),
            numeric,
        );
        if !cmp.is_eq() {
            return cmp;
        }
    }

    if !ignore_case {
        let cmp = compare_organize_imports_case(a, b, case_first);
        if !cmp.is_eq() {
            return cmp;
        }
    }

    go_cmp(a, b)
}

// organizeimports.go:133
fn natural_collation_key(s: &str) -> String {
    go_strings_to_lower(&remove_diacritics(s))
}

// organizeimports.go:137
fn remove_diacritics(s: &str) -> String {
    norm_nfd_string(s).chars().filter(|&r| !unicode_is_mn(r as u32)).collect()
}

// organizeimports.go:146
fn compare_organize_imports_unicode_keys(a: &str, b: &str, numeric: bool) -> Ordering {
    if numeric {
        return compare_strings_numeric(a, b);
    }
    go_cmp(a, b)
}

// organizeimports.go:153
fn compare_strings_numeric(a: &str, b: &str) -> Ordering {
    let mut a = a.as_bytes();
    let mut b = b.as_bytes();
    while !a.is_empty() && !b.is_empty() {
        if is_ascii_digit(a[0]) && is_ascii_digit(b[0]) {
            let a_run_end = ascii_digit_run_end(a);
            let b_run_end = ascii_digit_run_end(b);

            let cmp = compare_numeric_text(&a[..a_run_end], &b[..b_run_end]);
            if !cmp.is_eq() {
                return cmp;
            }

            a = &a[a_run_end..];
            b = &b[b_run_end..];
            continue;
        }

        let (a_rune, a_size) = decode_rune(a);
        let (b_rune, b_size) = decode_rune(b);
        if a_rune != b_rune {
            return go_cmp(a_rune, b_rune);
        }

        a = &a[a_size..];
        b = &b[b_size..];
    }

    go_cmp(a.len(), b.len())
}

// organizeimports.go:181
fn is_ascii_digit(ch: u8) -> bool {
    ch.is_ascii_digit()
}

// organizeimports.go:185
fn ascii_digit_run_end(s: &[u8]) -> usize {
    let mut i = 0;
    i += (i..s.len()).take_while(|&index| is_ascii_digit(s[index])).count();
    i
}

// organizeimports.go:193
fn compare_numeric_text(a: &[u8], b: &[u8]) -> Ordering {
    let mut a_digits: &[u8] = &a[a.iter().position(|&c| c != b'0').unwrap_or(a.len())..];
    let mut b_digits: &[u8] = &b[b.iter().position(|&c| c != b'0').unwrap_or(b.len())..];
    if a_digits.is_empty() {
        a_digits = b"0";
    }
    if b_digits.is_empty() {
        b_digits = b"0";
    }

    if a_digits.len() != b_digits.len() {
        return go_cmp(a_digits.len(), b_digits.len());
    }
    let cmp = go_cmp(a_digits, b_digits);
    if !cmp.is_eq() {
        return cmp;
    }
    go_cmp(a, b)
}

// organizeimports.go:212
fn compare_organize_imports_case_upper_first(a: &str, b: &str) -> Ordering {
    compare_organize_imports_case(a, b, OrganizeImportsCaseFirst::Upper)
}

// organizeimports.go:216
fn compare_organize_imports_case(
    a: &str,
    b: &str,
    case_first: OrganizeImportsCaseFirst,
) -> Ordering {
    let a_runes: Vec<char> = a.chars().collect();
    let b_runes: Vec<char> = b.chars().collect();
    let min_len = a_runes.len().min(b_runes.len());

    for i in 0..min_len {
        let a_upper = unicode_is_upper(a_runes[i] as u32);
        let b_upper = unicode_is_upper(b_runes[i] as u32);
        if a_upper != b_upper {
            match case_first {
                OrganizeImportsCaseFirst::Upper => {
                    if a_upper {
                        return Ordering::Less;
                    }
                    return Ordering::Greater;
                }
                OrganizeImportsCaseFirst::Lower => {
                    if !a_upper {
                        return Ordering::Less;
                    }
                    return Ordering::Greater;
                }
                OrganizeImportsCaseFirst::False => {
                    if a_upper {
                        return Ordering::Greater;
                    }
                    return Ordering::Less;
                }
            }
        }
    }

    go_cmp(a_runes.len(), b_runes.len())
}

// organizeimports.go:248
pub fn get_organize_imports_preset_string_comparer(sort: OrganizeImportsSort) -> StringComparer {
    match sort {
        OrganizeImportsSort::OrdinalIgnoreCase => {
            get_organize_imports_ordinal_string_comparer(true)
        }
        OrganizeImportsSort::Natural => get_organize_imports_natural_string_comparer(true),
        OrganizeImportsSort::NaturalIgnoreCase => {
            get_organize_imports_natural_string_comparer(false)
        }
        _ => get_organize_imports_ordinal_string_comparer(false),
    }
}

// organizeimports.go:261
fn get_organize_imports_string_comparer(
    preferences: &UserPreferences,
    ignore_case: bool,
) -> StringComparer {
    if preferences.organize_imports_sort != OrganizeImportsSort::Auto {
        return get_organize_imports_preset_string_comparer(preferences.organize_imports_sort);
    }
    if preferences.organize_imports_collation == OrganizeImportsCollation::Unicode {
        return get_organize_imports_unicode_string_comparer(ignore_case, preferences);
    }
    get_organize_imports_ordinal_string_comparer(ignore_case)
}

// organizeimports.go:271
fn get_module_specifier_expression(declaration: P<Node>) -> Option<P<Node>> {
    match declaration.kind() {
        Kind::ImportEqualsDeclaration => {
            let import_equals = declaration.as_import_equals_declaration();
            if import_equals.module_reference.kind() == Kind::ExternalModuleReference {
                return import_equals.module_reference.expression();
            }
            None
        }
        Kind::ImportDeclaration => declaration.module_specifier(),
        Kind::VariableStatement => {
            let declarations = declaration
                .as_variable_statement()
                .declaration_list
                .as_variable_declaration_list()
                .declarations
                .nodes();
            if !declarations.is_empty() {
                let initializer = declarations[0].initializer();
                if let Some(initializer) = initializer
                    && initializer.kind() == Kind::CallExpression
                {
                    let call_expr = initializer.as_call_expression();
                    if !call_expr.arguments.nodes().is_empty() {
                        return Some(call_expr.arguments.nodes()[0]);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

// organizeimports.go:299
// GetExternalModuleName returns the module name from a module specifier expression.
pub(crate) fn get_external_module_name(specifier: Option<P<Node>>) -> &'static str {
    if let Some(specifier) = specifier
        && ast::is_string_literal_like(specifier)
    {
        return specifier.text();
    }
    ""
}

// organizeimports.go:307
// CompareModuleSpecifiers compares two module specifiers using the given comparer.
pub(crate) fn compare_module_specifiers(
    m1: Option<P<Node>>,
    m2: Option<P<Node>>,
    comparer: &dyn Fn(&str, &str) -> Ordering,
) -> Ordering {
    let name1 = get_external_module_name(m1);
    let name2 = get_external_module_name(m2);
    let cmp = compare_booleans(name1.is_empty(), name2.is_empty());
    if !cmp.is_eq() {
        return cmp;
    }
    let cmp = compare_booleans(
        tspath::is_external_module_name_relative(name1),
        tspath::is_external_module_name_relative(name2),
    );
    if !cmp.is_eq() {
        return cmp;
    }
    comparer(name1, name2)
}

// organizeimports.go:319
fn compare_import_kind(s1: P<Node>, s2: P<Node>) -> Ordering {
    go_cmp(get_import_kind_order(s1), get_import_kind_order(s2))
}

// organizeimports.go:332
// getImportKindOrder returns the sort order for different import kinds:
// 1. Side-effect imports
// 2. Type-only imports
// 3. Namespace imports
// 4. Default imports
// 5. Named imports
// 6. ImportEqualsDeclarations
// 7. Require variable statements
const IMPORT_KIND_ORDER_SIDE_EFFECT: u8 = 0;
const IMPORT_KIND_ORDER_TYPE_ONLY: u8 = 1;
const IMPORT_KIND_ORDER_NAMESPACE: u8 = 2;
const IMPORT_KIND_ORDER_DEFAULT: u8 = 3;
const IMPORT_KIND_ORDER_NAMED: u8 = 4;
const IMPORT_KIND_ORDER_IMPORT_EQUALS: u8 = 5;
const IMPORT_KIND_ORDER_REQUIRE: u8 = 6;
const IMPORT_KIND_ORDER_UNKNOWN: u8 = 7;

// organizeimports.go:342
fn get_import_kind_order(s1: P<Node>) -> u8 {
    match s1.kind() {
        Kind::ImportDeclaration => {
            let import_decl = s1.as_import_declaration();
            let Some(import_clause_node) = import_decl.import_clause else {
                return IMPORT_KIND_ORDER_SIDE_EFFECT;
            };
            let import_clause = import_clause_node.as_import_clause();
            if import_clause_node.is_type_only() {
                return IMPORT_KIND_ORDER_TYPE_ONLY;
            }
            if import_clause.named_bindings.is_some_and(|nb| nb.kind() == Kind::NamespaceImport) {
                return IMPORT_KIND_ORDER_NAMESPACE;
            }
            if import_clause_node.name().is_some() {
                return IMPORT_KIND_ORDER_DEFAULT;
            }
            IMPORT_KIND_ORDER_NAMED
        }
        Kind::ImportEqualsDeclaration => IMPORT_KIND_ORDER_IMPORT_EQUALS,
        Kind::VariableStatement => IMPORT_KIND_ORDER_REQUIRE,
        _ => IMPORT_KIND_ORDER_UNKNOWN,
    }
}

// organizeimports.go:370
// CompareImportsOrRequireStatements compares two import or require statements.
pub(crate) fn compare_imports_or_require_statements(
    s1: P<Node>,
    s2: P<Node>,
    comparer: &dyn Fn(&str, &str) -> Ordering,
) -> Ordering {
    let cmp = compare_module_specifiers(
        get_module_specifier_expression(s1),
        get_module_specifier_expression(s2),
        comparer,
    );
    if !cmp.is_eq() {
        return cmp;
    }
    compare_import_kind(s1, s2)
}

// organizeimports.go:377
fn compare_import_or_export_specifiers(
    s1: P<Node>,
    s2: P<Node>,
    comparer: &dyn Fn(&str, &str) -> Ordering,
    preferences: &UserPreferences,
) -> Ordering {
    let type_order = preferences.organize_imports_type_order;

    let s1_name = s1.name().unwrap().text();
    let s2_name = s2.name().unwrap().text();

    match type_order {
        OrganizeImportsTypeOrder::First => {
            let cmp = compare_booleans(s2.is_type_only(), s1.is_type_only());
            if !cmp.is_eq() {
                return cmp;
            }
            comparer(s1_name, s2_name)
        }
        OrganizeImportsTypeOrder::Inline => comparer(s1_name, s2_name),
        _ => {
            // OrganizeImportsTypeOrderLast
            let cmp = compare_booleans(s1.is_type_only(), s2.is_type_only());
            if !cmp.is_eq() {
                return cmp;
            }
            comparer(s1_name, s2_name)
        }
    }
}

// organizeimports.go:400
// GetNamedImportSpecifierComparer returns a comparer function for sorting import specifiers.
pub(crate) fn get_named_import_specifier_comparer(
    preferences: &UserPreferences,
    comparer: Option<StringComparer>,
) -> NodeComparer {
    let comparer = if let Some(comparer) = comparer {
        comparer
    } else {
        let mut ignore_case = false;
        if !preferences.organize_imports_ignore_case.is_unknown() {
            ignore_case = preferences.organize_imports_ignore_case.is_true();
        }
        get_organize_imports_string_comparer(preferences, ignore_case)
    };
    let preferences = preferences.clone();
    Rc::new(move |s1: P<Node>, s2: P<Node>| {
        compare_import_or_export_specifiers(s1, s2, &*comparer, &preferences)
    })
}

// organizeimports.go:414
// GetImportSpecifierInsertionIndex returns the index at which to insert a new import specifier.
pub(crate) fn get_import_specifier_insertion_index(
    sorted_imports: &[P<Node>],
    new_import: P<Node>,
    comparer: &dyn Fn(P<Node>, P<Node>) -> Ordering,
) -> usize {
    binary_search_unique_func(sorted_imports, |_, &value| comparer(value, new_import)).0
}

// organizeimports.go:421
// GetImportDeclarationInsertIndex returns the index at which to insert a new import declaration.
pub(crate) fn get_import_declaration_insert_index(
    sorted_imports: &[P<Node>],
    new_import: P<Node>,
    comparer: &dyn Fn(P<Node>, P<Node>) -> Ordering,
) -> usize {
    binary_search_unique_func(sorted_imports, |_, &value| comparer(value, new_import)).0
}

// organizeimports.go:428
// GetOrganizeImportsStringComparerWithDetection returns a string comparer based on detecting the order of import statements by the module specifier
pub(crate) fn get_organize_imports_string_comparer_with_detection(
    original_import_decls: &[P<Node>],
    preferences: &UserPreferences,
) -> (Option<StringComparer>, bool) {
    let (result, sorted) = detect_module_specifier_case_by_sort(
        &[original_import_decls.to_vec()],
        &get_comparers(preferences),
    );
    (result, sorted)
}

// organizeimports.go:433
fn get_comparers(preferences: &UserPreferences) -> Vec<StringComparer> {
    if preferences.organize_imports_sort != OrganizeImportsSort::Auto
        || !preferences.organize_imports_ignore_case.is_unknown()
    {
        let mut ignore_case = false;
        if !preferences.organize_imports_ignore_case.is_unknown() {
            ignore_case = preferences.organize_imports_ignore_case.is_true();
        }
        return vec![get_organize_imports_string_comparer(preferences, ignore_case)];
    }
    vec![
        get_organize_imports_string_comparer(preferences, true),
        get_organize_imports_string_comparer(preferences, false),
    ]
}

// organizeimports.go:447
struct NamedImportSortResult {
    named_import_comparer: Option<StringComparer>,
    type_order: OrganizeImportsTypeOrder,
    is_sorted: bool,
}

// organizeimports.go:454
// DetectNamedImportOrganizationBySort detects the order of named imports throughout the file by considering the named imports in each statement as a group
pub(crate) fn detect_named_import_organization_by_sort(
    original_groups: &[P<Node>],
    comparers_to_test: &[StringComparer],
    types_to_test: &[OrganizeImportsTypeOrder],
) -> (Option<StringComparer>, OrganizeImportsTypeOrder, bool) {
    let result = detect_named_import_organization_by_sort_worker(
        original_groups,
        comparers_to_test,
        types_to_test,
    );
    match result {
        None => (None, OrganizeImportsTypeOrder::Last, false),
        Some(result) => (result.named_import_comparer, result.type_order, true),
    }
}

// organizeimports.go:466
fn detect_named_import_organization_by_sort_worker(
    original_groups: &[P<Node>],
    comparers_to_test: &[StringComparer],
    types_to_test: &[OrganizeImportsTypeOrder],
) -> Option<NamedImportSortResult> {
    let mut both_named_imports = false;
    let mut import_decls_with_named: Vec<P<Node>> = Vec::new();

    for &imp in original_groups {
        let Some(import_clause) = imp.as_import_declaration().import_clause else {
            continue;
        };
        let clause = import_clause.as_import_clause();
        let Some(named_bindings) =
            clause.named_bindings.filter(|nb| nb.kind() == Kind::NamedImports)
        else {
            continue;
        };
        let named_imports = named_bindings.as_named_imports();
        if named_imports.elements.nodes().is_empty() {
            continue;
        }

        if !both_named_imports {
            let mut has_type_only = false;
            let mut has_regular = false;
            for &elem in named_imports.elements.nodes() {
                if elem.is_type_only() {
                    has_type_only = true;
                } else {
                    has_regular = true;
                }
            }
            if has_type_only && has_regular {
                both_named_imports = true;
            }
        }

        import_decls_with_named.push(imp);
    }

    if import_decls_with_named.is_empty() {
        return None;
    }

    let mut named_imports_by_decl: Vec<&'static [P<Node>]> =
        Vec::with_capacity(import_decls_with_named.len());
    named_imports_by_decl.extend(import_decls_with_named.iter().map(|&imp| {
        let clause = imp.as_import_declaration().import_clause.unwrap().as_import_clause();
        let named_imports = clause.named_bindings.unwrap().as_named_imports();
        named_imports.elements.nodes()
    }));

    if !both_named_imports || types_to_test.is_empty() {
        let names_list: Vec<Vec<String>> = named_imports_by_decl
            .iter()
            .map(|imports| {
                imports.iter().map(|imp| imp.name().unwrap().text().to_string()).collect()
            })
            .collect();
        let sort_state = detect_case_sensitivity_by_sort(&names_list, comparers_to_test);
        let mut type_order = OrganizeImportsTypeOrder::Last;
        if types_to_test.len() == 1 {
            type_order = types_to_test[0];
        }
        return Some(NamedImportSortResult {
            named_import_comparer: sort_state.comparer,
            type_order,
            is_sorted: sort_state.is_sorted,
        });
    }

    let mut best_diff: FxHashMap<OrganizeImportsTypeOrder, i64> = FxHashMap::default();
    best_diff.insert(OrganizeImportsTypeOrder::First, i64::MAX);
    best_diff.insert(OrganizeImportsTypeOrder::Last, i64::MAX);
    best_diff.insert(OrganizeImportsTypeOrder::Inline, i64::MAX);
    let mut best_comparer: FxHashMap<OrganizeImportsTypeOrder, StringComparer> =
        FxHashMap::default();
    best_comparer.insert(OrganizeImportsTypeOrder::First, Rc::clone(&comparers_to_test[0]));
    best_comparer.insert(OrganizeImportsTypeOrder::Last, Rc::clone(&comparers_to_test[0]));
    best_comparer.insert(OrganizeImportsTypeOrder::Inline, Rc::clone(&comparers_to_test[0]));

    // Go map reads of a missing key yield the zero value.
    let diff_of = |m: &FxHashMap<OrganizeImportsTypeOrder, i64>, t: OrganizeImportsTypeOrder| {
        m.get(&t).copied().unwrap_or(0)
    };

    for cur_comparer in comparers_to_test {
        let mut curr_diff: FxHashMap<OrganizeImportsTypeOrder, i64> = FxHashMap::default();
        curr_diff.insert(OrganizeImportsTypeOrder::First, 0);
        curr_diff.insert(OrganizeImportsTypeOrder::Last, 0);
        curr_diff.insert(OrganizeImportsTypeOrder::Inline, 0);

        for import_decl in &named_imports_by_decl {
            for &type_order in types_to_test {
                let prefs = UserPreferences {
                    organize_imports_type_order: type_order,
                    ..Default::default()
                };
                let diff = measure_sortedness(import_decl, |&n1, &n2| {
                    compare_import_or_export_specifiers(n1, n2, &**cur_comparer, &prefs)
                });
                let updated = diff_of(&curr_diff, type_order) + diff as i64;
                curr_diff.insert(type_order, updated);
            }
        }

        for &type_order in types_to_test {
            if diff_of(&curr_diff, type_order) < diff_of(&best_diff, type_order) {
                best_diff.insert(type_order, diff_of(&curr_diff, type_order));
                best_comparer.insert(type_order, Rc::clone(cur_comparer));
            }
        }
    }

    for &best_type_order in types_to_test {
        let is_best = !(types_to_test.iter().any(|&test_type_order| {
            diff_of(&best_diff, test_type_order) < diff_of(&best_diff, best_type_order)
        }));
        if is_best {
            return Some(NamedImportSortResult {
                named_import_comparer: best_comparer.get(&best_type_order).cloned(),
                type_order: best_type_order,
                is_sorted: diff_of(&best_diff, best_type_order) == 0,
            });
        }
    }

    Some(NamedImportSortResult {
        named_import_comparer: best_comparer.get(&OrganizeImportsTypeOrder::Last).cloned(),
        type_order: OrganizeImportsTypeOrder::Last,
        is_sorted: diff_of(&best_diff, OrganizeImportsTypeOrder::Last) == 0,
    })
}

// organizeimports.go:597
struct CaseSensitivityDetectionResult {
    comparer: Option<StringComparer>,
    is_sorted: bool,
}

// organizeimports.go:603
// DetectModuleSpecifierCaseBySort detects the order of module specifiers based on import statements throughout the module/file
pub(crate) fn detect_module_specifier_case_by_sort(
    import_decls_by_group: &[Vec<P<Node>>],
    comparers_to_test: &[StringComparer],
) -> (Option<StringComparer>, bool) {
    let mut module_specifiers_by_group: Vec<Vec<String>> =
        Vec::with_capacity(import_decls_by_group.len());
    module_specifiers_by_group.extend(import_decls_by_group.iter().map(|import_group| {
        let mut module_names: Vec<String> = Vec::with_capacity(import_group.len());
        module_names.extend(import_group.iter().map(|&decl| {
            if let Some(expr) = get_module_specifier_expression(decl) {
                get_external_module_name(Some(expr)).to_string()
            } else {
                String::new()
            }
        }));
        module_names
    }));
    let result = detect_case_sensitivity_by_sort(&module_specifiers_by_group, comparers_to_test);
    (result.comparer, result.is_sorted)
}

// organizeimports.go:620
fn detect_case_sensitivity_by_sort(
    original_groups: &[Vec<String>],
    comparers_to_test: &[StringComparer],
) -> CaseSensitivityDetectionResult {
    let mut best_comparer: Option<StringComparer> = None;
    let mut best_diff = i64::MAX;

    for cur_comparer in comparers_to_test {
        let mut diff_of_current_comparer: i64 = 0;

        for list_to_sort in original_groups {
            if list_to_sort.len() <= 1 {
                continue;
            }
            let diff = measure_sortedness(list_to_sort, |a, b| cur_comparer(a, b));
            diff_of_current_comparer += diff as i64;
        }

        if diff_of_current_comparer < best_diff {
            best_diff = diff_of_current_comparer;
            best_comparer = Some(Rc::clone(cur_comparer));
        }
    }

    if best_comparer.is_none() && !comparers_to_test.is_empty() {
        best_comparer = Some(Rc::clone(&comparers_to_test[0]));
    }

    CaseSensitivityDetectionResult { comparer: best_comparer, is_sorted: best_diff == 0 }
}

// organizeimports.go:651
fn measure_sortedness<T>(arr: &[T], comparer: impl Fn(&T, &T) -> Ordering) -> usize {
    let mut i = 0;
    i += arr.windows(2).filter(|pair| comparer(&pair[0], &pair[1]).is_gt()).count();
    i
}

// organizeimports.go:662
// GetNamedImportSpecifierComparerWithDetection returns a specifier comparer based on detecting the existing sort order within a single import statement
pub(crate) fn get_named_import_specifier_comparer_with_detection(
    import_decl: P<Node>,
    source_file: Option<P<SourceFile>>,
    preferences: &UserPreferences,
) -> (NodeComparer, Tristate) {
    let (comparers_to_test, type_orders_to_test) = get_detection_lists(preferences);

    let mut import_stmt: Option<P<Node>> = None;
    if import_decl.kind() == Kind::ImportDeclaration {
        import_stmt = Some(import_decl);
    }

    let mut specifier_comparer =
        get_named_import_specifier_comparer(preferences, Some(Rc::clone(&comparers_to_test[0])));
    let mut is_sorted = Tristate::Unknown;

    if (resolve_organize_imports_sort(preferences) == OrganizeImportsSort::Auto
        || preferences.organize_imports_type_order == OrganizeImportsTypeOrder::Auto)
        && let Some(import_stmt) = import_stmt
    {
        let detect_from_decl = detect_named_import_organization_by_sort_worker(
            &[import_stmt],
            &comparers_to_test,
            &type_orders_to_test,
        );
        if let Some(detect_from_decl) = detect_from_decl {
            is_sorted = bool_to_tristate(detect_from_decl.is_sorted);
            specifier_comparer = get_named_import_specifier_comparer(
                &UserPreferences {
                    organize_imports_type_order: detect_from_decl.type_order,
                    ..Default::default()
                },
                detect_from_decl.named_import_comparer,
            );
        } else if let Some(source_file) = source_file {
            let all_imports = filter_import_declarations(source_file.statements.nodes());
            let detect_from_file = detect_named_import_organization_by_sort_worker(
                &all_imports,
                &comparers_to_test,
                &type_orders_to_test,
            );
            if let Some(detect_from_file) = detect_from_file {
                is_sorted = bool_to_tristate(detect_from_file.is_sorted);
                specifier_comparer = get_named_import_specifier_comparer(
                    &UserPreferences {
                        organize_imports_type_order: detect_from_file.type_order,
                        ..Default::default()
                    },
                    detect_from_file.named_import_comparer,
                );
            }
        }
    }

    (specifier_comparer, is_sorted)
}
