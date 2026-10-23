use std::sync::Arc;

use serde::{Deserialize, Serialize};

// JSON cannot carry non-finite constVal numbers, so runners and pins tag them
// as {"__jsnum": "Infinity"|"-Infinity"|"NaN"}; assembly decodes via this pair.
pub fn non_finite_to_tag(x: f64) -> Option<serde_json::Value> {
    if x.is_finite() {
        return None;
    }
    Some(serde_json::json!({ "__jsnum": crate::jsrt::js_number_to_string(x) }))
}

pub fn non_finite_from_tag(v: &serde_json::Value) -> Option<f64> {
    let obj = v.as_object().filter(|o| o.len() == 1)?;
    match obj.get("__jsnum")?.as_str()? {
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        "NaN" => Some(f64::NAN),
        _ => None,
    }
}

// Wire mirror of babel metadata.stylex tuples; shared with the oj fork by type, change both sides.
// Arc'd text so clones are refcount bumps; const_val boxed because almost every rule is None.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StylexRule {
    pub class_name: Arc<str>,
    pub ltr: Arc<str>,
    #[serde(default)]
    pub rtl: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub const_key: Option<Arc<str>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub const_val: Option<Box<serde_json::Value>>,
    pub priority: f64,
}

impl StylexRule {
    // [className, {ltr, rtl?, constKey?, constVal?}, priority] as babel emits it.
    pub fn from_metadata_tuple(v: &serde_json::Value) -> Result<Self, String> {
        let tuple = v
            .as_array()
            .filter(|a| a.len() == 3)
            .ok_or("expected a 3-tuple")?;
        let class_name = tuple[0].as_str().ok_or("className must be a string")?;
        let obj = tuple[1].as_object().ok_or("rule body must be an object")?;
        let ltr = obj
            .get("ltr")
            .and_then(|l| l.as_str())
            .ok_or("ltr must be a string")?;
        let rtl = match obj.get("rtl") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => Some(s.as_str().into()),
            Some(_) => return Err("rtl must be a string or null".to_string()),
        };
        let const_key = match obj.get("constKey") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => Some(s.as_str().into()),
            Some(_) => return Err("constKey must be a string or null".to_string()),
        };
        let const_val = obj.get("constVal").cloned().map(Box::new);
        let priority = tuple[2].as_f64().ok_or("priority must be a number")?;
        Ok(StylexRule {
            class_name: class_name.into(),
            ltr: ltr.into(),
            rtl,
            const_key,
            const_val,
            priority,
        })
    }

    pub fn to_metadata_tuple(&self) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        body.insert(
            "ltr".to_string(),
            serde_json::Value::String(self.ltr.to_string()),
        );
        if let Some(rtl) = &self.rtl {
            body.insert(
                "rtl".to_string(),
                serde_json::Value::String(rtl.to_string()),
            );
        }
        if let Some(const_key) = &self.const_key {
            body.insert(
                "constKey".to_string(),
                serde_json::Value::String(const_key.to_string()),
            );
        }
        if let Some(const_val) = &self.const_val {
            body.insert("constVal".to_string(), (**const_val).clone());
        }
        serde_json::json!([self.class_name, body, self.priority])
    }
}

#[cfg(test)]
mod layout_tests {
    #[test]
    fn rule_struct_stays_compact() {
        assert!(std::mem::size_of::<super::StylexRule>() <= 80);
    }
}

#[cfg(test)]
mod non_finite_tag_tests {
    use super::*;

    #[test]
    fn tagged_const_val_round_trips_through_metadata_tuples() {
        for (x, tag) in [
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
            (f64::NAN, "NaN"),
        ] {
            let tagged = non_finite_to_tag(x).unwrap();
            assert_eq!(tagged, serde_json::json!({ "__jsnum": tag }));
            let rule = StylexRule {
                class_name: "xc".into(),
                ltr: "".into(),
                rtl: None,
                const_key: Some("xc".into()),
                const_val: Some(Box::new(tagged.clone())),
                priority: 0.0,
            };
            let round = StylexRule::from_metadata_tuple(&rule.to_metadata_tuple()).unwrap();
            assert_eq!(round.const_val.as_deref(), Some(&tagged));
            let decoded = non_finite_from_tag(&tagged).unwrap();
            assert!(decoded.is_nan() == x.is_nan() && (x.is_nan() || decoded == x));
        }
        assert_eq!(non_finite_to_tag(1.5), None);
        assert_eq!(
            non_finite_from_tag(&serde_json::json!({"__jsnum": "1"})),
            None
        );
        assert_eq!(non_finite_from_tag(&serde_json::json!("Infinity")), None);
    }
}
