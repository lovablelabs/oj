//! Unused compiled-create declarator pruning at Program.exit.
// parity: babel-plugin src/index.js:187-301 (varsToKeep + styleVarsToKeep)

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::eval::value::{EvalValue, JsObjectMap};
use crate::transform::merge::{NonNullProps, StyleVarToKeep};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keep {
    All,
    Namespaces(Vec<String>),
}

pub fn compute_vars_to_keep(tuples: &[StyleVarToKeep]) -> BTreeMap<String, Keep> {
    let mut out: BTreeMap<String, Keep> = BTreeMap::new();
    for tuple in tuples {
        match out.get_mut(&tuple.var_name) {
            Some(Keep::All) => {}
            Some(Keep::Namespaces(list)) => match &tuple.namespace {
                None => {
                    out.insert(tuple.var_name.clone(), Keep::All);
                }
                Some(ns) => list.push(ns.clone()),
            },
            None => {
                let keep = match &tuple.namespace {
                    None => Keep::All,
                    Some(ns) => Keep::Namespaces(vec![ns.clone()]),
                };
                out.insert(tuple.var_name.clone(), keep);
            }
        }
    }
    out
}

#[derive(Debug, PartialEq)]
pub enum DceAction {
    KeepAll,
    Remove,
    Prune(JsObjectMap),
}

pub fn dce_action(
    var_name: &str,
    compiled: &JsObjectMap,
    exported: bool,
    vars_to_keep: &BTreeMap<String, Keep>,
    tuples: &[StyleVarToKeep],
) -> DceAction {
    if exported {
        return DceAction::KeepAll;
    }
    match vars_to_keep.get(var_name) {
        Some(Keep::All) => DceAction::KeepAll,
        None => DceAction::Remove,
        Some(Keep::Namespaces(namespaces)) => {
            let mut pruned = JsObjectMap::new();
            for (key, value) in compiled.entries() {
                if !namespaces.iter().any(|ns| ns == key) {
                    continue;
                }
                pruned.insert(key, prune_namespace(var_name, key, value, tuples));
            }
            DceAction::Prune(pruned)
        }
    }
}

fn prune_namespace(
    var_name: &str,
    namespace: &str,
    value: &EvalValue,
    tuples: &[StyleVarToKeep],
) -> EvalValue {
    let relevant: Vec<&NonNullProps> = tuples
        .iter()
        .filter(|t| t.var_name == var_name && t.namespace.as_deref() == Some(namespace))
        .map(|t| &t.non_null_props)
        .collect();
    if relevant.iter().any(|p| matches!(p, NonNullProps::True)) {
        return value.clone();
    }
    let EvalValue::Obj(map) = value else {
        return value.clone();
    };
    let mut keep_nulls: Vec<&str> = Vec::new();
    for props in relevant {
        if let NonNullProps::Props(list) = props {
            keep_nulls.extend(list.iter().map(String::as_str));
        }
    }
    let mut pruned = JsObjectMap::new();
    for (key, prop_value) in map.entries() {
        if matches!(prop_value, EvalValue::Null) && !keep_nulls.contains(&key) {
            continue;
        }
        pruned.insert(key, prop_value.clone());
    }
    EvalValue::Obj(Arc::new(pruned))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tuple(var: &str, ns: Option<&str>, props: NonNullProps) -> StyleVarToKeep {
        StyleVarToKeep {
            var_name: var.to_string(),
            namespace: ns.map(str::to_string),
            non_null_props: props,
        }
    }

    fn compiled() -> JsObjectMap {
        let mut a = JsObjectMap::new();
        a.insert("k1", EvalValue::Str("x1".into()));
        a.insert("k2", EvalValue::Null);
        a.insert("$$css", EvalValue::Bool(true));
        let mut b = JsObjectMap::new();
        b.insert("k3", EvalValue::Null);
        b.insert("$$css", EvalValue::Bool(true));
        let mut top = JsObjectMap::new();
        top.insert("a", EvalValue::Obj(a.into()));
        top.insert("b", EvalValue::Obj(b.into()));
        top
    }

    #[test]
    fn unreferenced_vars_are_removed_and_exported_kept() {
        let vars = compute_vars_to_keep(&[]);
        assert_eq!(
            dce_action("styles", &compiled(), false, &vars, &[]),
            DceAction::Remove
        );
        assert_eq!(
            dce_action("styles", &compiled(), true, &vars, &[]),
            DceAction::KeepAll
        );
    }

    #[test]
    fn namespace_and_null_prop_pruning() {
        let tuples = vec![tuple(
            "styles",
            Some("a"),
            NonNullProps::Props(vec!["k1".to_string()]),
        )];
        let vars = compute_vars_to_keep(&tuples);
        let DceAction::Prune(map) = dce_action("styles", &compiled(), false, &vars, &tuples) else {
            panic!("expected prune");
        };
        assert_eq!(map.keys().collect::<Vec<_>>(), vec!["a"]);
        let EvalValue::Obj(a) = map.get("a").unwrap() else {
            panic!("namespace object");
        };
        // k2 is null and not in the keep list; $$css survives (non-null).
        assert_eq!(a.keys().collect::<Vec<_>>(), vec!["k1", "$$css"]);
    }

    #[test]
    fn true_tuple_disables_null_pruning_and_bare_use_keeps_all() {
        let tuples = vec![
            tuple("styles", Some("b"), NonNullProps::True),
            tuple("other", None, NonNullProps::True),
        ];
        let vars = compute_vars_to_keep(&tuples);
        let DceAction::Prune(map) = dce_action("styles", &compiled(), false, &vars, &tuples) else {
            panic!("expected prune");
        };
        let EvalValue::Obj(b) = map.get("b").unwrap() else {
            panic!("namespace object");
        };
        assert_eq!(b.keys().collect::<Vec<_>>(), vec!["k3", "$$css"]);
        assert_eq!(
            dce_action("other", &compiled(), false, &vars, &tuples),
            DceAction::KeepAll
        );
    }
}
