//! Fallback arrays: contiguous `var()` runs collapse into nested `var(a, var(b, …))`.
// parity: babel-plugin src/shared/utils/convert-to-className.js:85-121

use crate::errors::StylexError;

fn is_var(value: &str) -> bool {
    value.starts_with("var(") && value.ends_with(')')
}

pub fn has_var_fallback(values: &[String]) -> bool {
    values.iter().any(|v| is_var(v))
}

/// Errors with `NON_CONTIGUOUS_VARS` when a non-var value sits between vars.
pub fn variable_fallbacks(values: &[String]) -> Result<Vec<String>, StylexError> {
    let first_var = values.iter().position(|v| is_var(v));
    let last_var = values.iter().rposition(|v| is_var(v));
    let (Some(first_var), Some(last_var)) = (first_var, last_var) else {
        // Unreachable via convertStyleToClassName (caller pre-checks has_var_fallback).
        return Ok(values.to_vec());
    };

    let values_before = &values[..first_var];
    let mut var_values: Vec<&String> = values[first_var..=last_var].iter().collect();
    var_values.reverse();
    let values_after = &values[last_var + 1..];

    if var_values.iter().any(|v| !is_var(v)) {
        return Err(StylexError::non_contiguous_vars());
    }
    let var_names: Vec<&str> = var_values.iter().map(|v| &v[4..v.len() - 1]).collect();

    let mut out = Vec::new();
    if values_before.is_empty() {
        out.push(compose_vars(&var_names));
    } else {
        for val in values_before {
            let mut args = var_names.clone();
            args.push(val);
            out.push(compose_vars(&args));
        }
    }
    out.extend(values_after.iter().cloned());
    Ok(out)
}

// parity: convert-to-className.js composeVars
fn compose_vars(args: &[&str]) -> String {
    match args {
        [] => unreachable!("compose_vars is never called with zero args"),
        [first] if first.starts_with("--") => format!("var({first})"),
        [first] => (*first).to_string(),
        [first, rest @ ..] => format!("var({first},{})", compose_vars(rest)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorCode;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn oracle_pinned_shapes() {
        // Pinned via probes 2026-08-27 against @stylexjs/babel-plugin 0.19.0.
        assert_eq!(
            variable_fallbacks(&v(&["var(--a)", "red"])).unwrap(),
            v(&["var(--a)", "red"])
        );
        assert_eq!(
            variable_fallbacks(&v(&["red", "var(--a)", "var(--b)", "blue"])).unwrap(),
            v(&["var(--b,var(--a,red))", "blue"])
        );
        assert_eq!(
            variable_fallbacks(&v(&["red", "blue", "var(--a)", "var(--b)"])).unwrap(),
            v(&["var(--b,var(--a,red))", "var(--b,var(--a,blue))"])
        );
        assert_eq!(
            variable_fallbacks(&v(&["var(--a)", "var(--b)"])).unwrap(),
            v(&["var(--b,var(--a))"])
        );
        assert_eq!(
            variable_fallbacks(&v(&["var(--a)"])).unwrap(),
            v(&["var(--a)"])
        );
    }

    #[test]
    fn non_contiguous_vars_throw() {
        let err = variable_fallbacks(&v(&["var(--a)", "red", "var(--b)"])).unwrap_err();
        assert_eq!(err.code, ErrorCode::NonContiguousVars);
        assert_eq!(
            err.message,
            "All variables passed to firstThatWorks() must be contiguous."
        );
    }
}
