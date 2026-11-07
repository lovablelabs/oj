use super::*;

// definitions.go:8 (Go package-level `var`s built with `transformers.Chain`)
fn es_decorator_and_class_fields(opts: &TransformOptions) -> Option<P<Transformer>> {
    chain(&[new_es_decorator_transformer, new_class_fields_transformer])(opts)
}

fn new_esnext_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    chain(&[new_using_declaration_transformer, es_decorator_and_class_fields])(opts)
}

// 2026: no new downlevel syntax
// 2025: only module system syntax (import attributes, json modules), untransformed regex modifiers
// 2024: no new downlevel syntax
// 2023: no new downlevel syntax
// 2022: class static blocks and class fields are handled by newClassFieldsTransformer
fn new_es2021_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    chain(&[new_esnext_transformer, new_logical_assignment_transformer])(opts)
}

fn new_es2020_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    chain(&[
        new_es2021_transformer,
        new_nullish_coalescing_transformer,
        new_optional_chain_transformer,
    ])(opts)
}

fn new_es2019_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    chain(&[new_es2020_transformer, new_optional_catch_transformer])(opts)
}

fn new_es2018_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    chain(&[
        new_es2019_transformer,
        new_object_rest_spread_transformer,
        newforawait_transformer,
        new_tagged_template_lift_restriction_transformer,
    ])(opts)
}

fn new_es2017_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    chain(&[new_es2018_transformer, new_async_transformer])(opts)
}

fn new_es2016_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    chain(&[new_es2017_transformer, new_exponentiation_transformer])(opts)
}

// definitions.go:24
pub fn get_es_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    let options = opts.compiler_options;
    match options.get_emit_script_target() {
        ScriptTarget::ES_NEXT => es_decorator_and_class_fields(opts),
        ScriptTarget::ES2026
        | ScriptTarget::ES2025
        | ScriptTarget::ES2024
        | ScriptTarget::ES2023
        | ScriptTarget::ES2022
        | ScriptTarget::ES2021 => new_esnext_transformer(opts),
        ScriptTarget::ES2020 => new_es2021_transformer(opts),
        ScriptTarget::ES2019 => new_es2020_transformer(opts),
        ScriptTarget::ES2018 => new_es2019_transformer(opts),
        ScriptTarget::ES2017 => new_es2018_transformer(opts),
        ScriptTarget::ES2016 => new_es2017_transformer(opts),
        // other, older, option, transform maximally
        _ => new_es2016_transformer(opts),
    }
}
