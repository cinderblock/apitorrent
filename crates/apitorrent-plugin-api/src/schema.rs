//! Plugin settings: a JSON Schema subset the host validates and the web UI renders.
//!
//! Supporting only what the UI can draw keeps the two in step: a schema with a keyword
//! outside this list is refused at install instead of half-working.
//!
//! - `type`: one of `object`, `array`, `string`, `number`, `integer`, `boolean`. Required on
//!   every schema; the root must be an `object`.
//! - Annotations: `title`, `description`, `default` (validated against its schema).
//! - `enum`: a non-empty list of values of the schema's type.
//! - Objects: `properties`, `required`, and `additionalProperties: false` (unknown keys are
//!   always refused, so `true` isn't allowed).
//! - Arrays: `items` (required), `minItems`, `maxItems`.
//! - Strings: `minLength`, `maxLength`, `format` (`uri`: an absolute `http`/`https` URL;
//!   `multiline`: a UI hint for a text area).
//! - Numbers: `minimum`, `maximum`.
//! - `writeOnly: true` on a top-level string property marks a secret: the API never sends it
//!   back, and the UI shows only whether it is set.
//!
//! Nesting is limited to [`MAX_DEPTH`] levels.

use serde_json::{Map, Value};

pub const MAX_DEPTH: usize = 8;
/// The largest settings document, serialised.
pub const MAX_SETTINGS_BYTES: usize = 256 * 1024;

const KEYWORDS: &[&str] = &[
    "type",
    "title",
    "description",
    "default",
    "enum",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "format",
    "minimum",
    "maximum",
    "writeOnly",
    // Allowed at the root only, so schemas can name their dialect.
    "$schema",
];

/// Check that a settings schema uses only the supported subset, consistently.
pub fn check_schema(schema: &Value) -> Result<(), String> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("the root must have `type: \"object\"`".to_owned());
    }
    check_node(schema, "", 0, true)
}

fn ty(schema: &Value) -> Option<&str> {
    schema.get("type").and_then(Value::as_str)
}

fn check_node(schema: &Value, path: &str, depth: usize, root: bool) -> Result<(), String> {
    let at = |msg: String| {
        if path.is_empty() {
            msg
        } else {
            format!("{path}: {msg}")
        }
    };
    if depth > MAX_DEPTH {
        return Err(at(format!("nested deeper than {MAX_DEPTH} levels")));
    }
    let obj = schema
        .as_object()
        .ok_or_else(|| at("a schema must be an object".to_owned()))?;
    for k in obj.keys() {
        if !KEYWORDS.contains(&k.as_str()) || (k == "$schema" && !root) {
            return Err(at(format!("keyword `{k}` is not supported")));
        }
    }
    let t = ty(schema).ok_or_else(|| at("`type` is required".to_owned()))?;
    if !matches!(
        t,
        "object" | "array" | "string" | "number" | "integer" | "boolean"
    ) {
        return Err(at(format!("type `{t}` is not supported")));
    }
    for k in ["title", "description"] {
        if obj.get(k).is_some_and(|v| !v.is_string()) {
            return Err(at(format!("`{k}` must be a string")));
        }
    }
    let applies = |keys: &[&str], to: &[&str]| -> Result<(), String> {
        for k in keys {
            if obj.contains_key(*k) && !to.contains(&t) {
                return Err(at(format!("`{k}` doesn't apply to type `{t}`")));
            }
        }
        Ok(())
    };
    applies(
        &["properties", "required", "additionalProperties"],
        &["object"],
    )?;
    applies(&["items", "minItems", "maxItems"], &["array"])?;
    applies(&["minLength", "maxLength", "format"], &["string"])?;
    applies(&["minimum", "maximum"], &["number", "integer"])?;
    for k in ["minItems", "maxItems", "minLength", "maxLength"] {
        if obj.get(k).is_some_and(|v| v.as_u64().is_none()) {
            return Err(at(format!("`{k}` must be a non-negative integer")));
        }
    }
    for k in ["minimum", "maximum"] {
        if obj.get(k).is_some_and(|v| !v.is_number()) {
            return Err(at(format!("`{k}` must be a number")));
        }
    }
    if let Some(f) = obj.get("format")
        && !matches!(f.as_str(), Some("uri" | "multiline"))
    {
        return Err(at("`format` must be `uri` or `multiline`".to_owned()));
    }
    if let Some(w) = obj.get("writeOnly") {
        let top_string = depth == 1 && t == "string";
        if w != &Value::Bool(true) || !top_string {
            return Err(at(
                "`writeOnly` must be `true`, on a top-level string property".to_owned(),
            ));
        }
    }
    if t == "object" {
        if obj
            .get("additionalProperties")
            .is_some_and(|v| v != &Value::Bool(false))
        {
            return Err(at(
                "`additionalProperties` may only be `false` (unknown keys are always refused)"
                    .to_owned(),
            ));
        }
        let props = match obj.get("properties") {
            None => &Map::new(),
            Some(Value::Object(p)) => p,
            Some(_) => return Err(at("`properties` must be an object".to_owned())),
        };
        for (name, sub) in props {
            check_node(sub, &join(path, name), depth + 1, false)?;
        }
        if let Some(req) = obj.get("required") {
            let req = req
                .as_array()
                .ok_or_else(|| at("`required` must be a list".to_owned()))?;
            for r in req {
                let name = r
                    .as_str()
                    .ok_or_else(|| at("`required` must list names".to_owned()))?;
                if !props.contains_key(name) {
                    return Err(at(format!("`required` names unknown property `{name}`")));
                }
            }
        }
    }
    if t == "array" {
        let items = obj
            .get("items")
            .ok_or_else(|| at("an array needs `items`".to_owned()))?;
        check_node(items, &format!("{path}[]"), depth + 1, false)?;
    }
    if let Some(e) = obj.get("enum") {
        let list = e
            .as_array()
            .filter(|l| !l.is_empty())
            .ok_or_else(|| at("`enum` must be a non-empty list".to_owned()))?;
        if matches!(t, "object" | "array") {
            return Err(at(
                "`enum` applies to strings, numbers and booleans".to_owned()
            ));
        }
        for v in list {
            if !type_matches(t, v) {
                return Err(at(format!("enum value {v} is not a {t}")));
            }
        }
    }
    if let Some(d) = obj.get("default") {
        let mut errors = Vec::new();
        validate_into(schema, &with_defaults(schema, d), path, &mut errors);
        if let Some(e) = errors.first() {
            return Err(format!("default is invalid: {e}"));
        }
    }
    Ok(())
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        _ => false,
    }
}

/// Validate settings (with defaults already applied) against a checked schema. Returns
/// every problem, each prefixed with the path of the offending value.
pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    validate_into(schema, value, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_into(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let name = if path.is_empty() { "settings" } else { path };
    let Some(t) = ty(schema) else {
        return;
    };
    if !type_matches(t, value) {
        errors.push(format!("{name}: expected {t}"));
        return;
    }
    if let Some(list) = schema.get("enum").and_then(Value::as_array)
        && !list.contains(value)
    {
        errors.push(format!(
            "{name}: must be one of {}",
            Value::Array(list.clone())
        ));
    }
    let num = |k: &str| schema.get(k).and_then(Value::as_f64);
    let count = |k: &str| schema.get(k).and_then(Value::as_u64);
    match value {
        Value::Object(map) => {
            let empty = Map::new();
            let props = schema
                .get("properties")
                .and_then(Value::as_object)
                .unwrap_or(&empty);
            for (k, v) in map {
                match props.get(k) {
                    Some(sub) => validate_into(sub, v, &join(path, k), errors),
                    None => errors.push(format!("{}: unknown setting", join(path, k))),
                }
            }
            if let Some(req) = schema.get("required").and_then(Value::as_array) {
                for r in req.iter().filter_map(Value::as_str) {
                    if !map.contains_key(r) {
                        errors.push(format!("{}: required", join(path, r)));
                    }
                }
            }
        }
        Value::Array(items) => {
            if let Some(min) = count("minItems")
                && (items.len() as u64) < min
            {
                errors.push(format!("{name}: needs at least {min} items"));
            }
            if let Some(max) = count("maxItems")
                && (items.len() as u64) > max
            {
                errors.push(format!("{name}: allows at most {max} items"));
            }
            if let Some(sub) = schema.get("items") {
                for (i, v) in items.iter().enumerate() {
                    validate_into(sub, v, &format!("{name}[{i}]"), errors);
                }
            }
        }
        Value::String(s) => {
            let len = s.chars().count() as u64;
            if let Some(min) = count("minLength")
                && len < min
            {
                errors.push(format!("{name}: needs at least {min} characters"));
            }
            if let Some(max) = count("maxLength")
                && len > max
            {
                errors.push(format!("{name}: allows at most {max} characters"));
            }
            let is_url =
                || url::Url::parse(s).is_ok_and(|u| matches!(u.scheme(), "http" | "https"));
            if schema.get("format").and_then(Value::as_str) == Some("uri") && !is_url() {
                errors.push(format!("{name}: must be an http:// or https:// URL"));
            }
        }
        Value::Number(n) => {
            let v = n.as_f64().unwrap_or(f64::NAN);
            if let Some(min) = num("minimum")
                && v < min
            {
                errors.push(format!("{name}: must be at least {min}"));
            }
            if let Some(max) = num("maximum")
                && v > max
            {
                errors.push(format!("{name}: must be at most {max}"));
            }
        }
        _ => {}
    }
}

/// `value` with defaults filled in: missing properties take their `default`, and missing
/// object-typed properties without one become objects with their own defaults. Array items
/// that are objects get their defaults too. Values of the wrong type are left for
/// [`validate`] to report.
pub fn with_defaults(schema: &Value, value: &Value) -> Value {
    match (ty(schema), value) {
        (Some("object"), Value::Object(map)) => {
            let mut out = map.clone();
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (k, sub) in props {
                    match out.get(k) {
                        Some(v) => {
                            let filled = with_defaults(sub, v);
                            out.insert(k.clone(), filled);
                        }
                        None => {
                            if let Some(d) = sub.get("default") {
                                out.insert(k.clone(), with_defaults(sub, d));
                            } else if ty(sub) == Some("object") {
                                out.insert(
                                    k.clone(),
                                    with_defaults(sub, &Value::Object(Map::new())),
                                );
                            }
                        }
                    }
                }
            }
            Value::Object(out)
        }
        (Some("array"), Value::Array(items)) => match schema.get("items") {
            Some(sub) => Value::Array(items.iter().map(|v| with_defaults(sub, v)).collect()),
            None => value.clone(),
        },
        _ => value.clone(),
    }
}

/// The names of the secret (top-level `writeOnly`) properties.
pub fn secrets(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| {
            p.iter()
                .filter(|(_, s)| s.get("writeOnly") == Some(&Value::Bool(true)))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rss_like() -> Value {
        json!({
            "type": "object",
            "properties": {
                "interval": {"type": "integer", "minimum": 5, "default": 15, "title": "Minutes"},
                "token": {"type": "string", "writeOnly": true},
                "mode": {"type": "string", "enum": ["auto", "manual"], "default": "auto"},
                "feeds": {
                    "type": "array", "maxItems": 3, "default": [],
                    "items": {
                        "type": "object",
                        "required": ["url"],
                        "properties": {
                            "url": {"type": "string", "format": "uri"},
                            "paused": {"type": "boolean", "default": false},
                            "include": {"type": "string", "format": "multiline"}
                        }
                    }
                },
                "advanced": {
                    "type": "object",
                    "properties": {"retries": {"type": "integer", "default": 3, "maximum": 10}}
                }
            }
        })
    }

    #[test]
    fn accepts_the_subset() {
        check_schema(&rss_like()).unwrap();
        assert_eq!(secrets(&rss_like()), ["token"]);
    }

    #[test]
    fn rejects_what_the_ui_cannot_draw() {
        let cases = [
            json!({"type": "string"}),
            json!({"type": "object", "properties": {"a": {"type": "string", "pattern": "x"}}}),
            json!({"type": "object", "properties": {"a": {"type": ["string", "null"]}}}),
            json!({"type": "object", "properties": {"a": {"oneOf": []}}}),
            json!({"type": "object", "additionalProperties": true}),
            json!({"type": "object", "required": ["missing"]}),
            json!({"type": "object", "properties": {"a": {"type": "array"}}}),
            json!({"type": "object", "properties": {"a": {"type": "string", "default": 3}}}),
            json!({"type": "object", "properties": {"a": {"type": "string", "enum": [1]}}}),
            json!({"type": "object", "properties": {"a": {"type": "integer", "writeOnly": true}}}),
            json!({"type": "object", "properties": {"a": {"type": "object", "properties": {"b": {"type": "string", "writeOnly": true}}}}}),
            json!({"type": "object", "properties": {"a": {"type": "string", "format": "email"}}}),
            json!({"type": "object", "properties": {"a": {"type": "string", "minimum": 1}}}),
        ];
        for c in cases {
            assert!(check_schema(&c).is_err(), "{c}");
        }
    }

    #[test]
    fn defaults_fill_nested_values() {
        let s = rss_like();
        let v = with_defaults(&s, &json!({"feeds": [{"url": "https://x.org/rss"}]}));
        assert_eq!(
            v,
            json!({
                "interval": 15, "mode": "auto",
                "feeds": [{"url": "https://x.org/rss", "paused": false}],
                "advanced": {"retries": 3}
            })
        );
        validate(&s, &v).unwrap();
    }

    #[test]
    fn validation_reports_paths() {
        let s = rss_like();
        let v = with_defaults(
            &s,
            &json!({
                "interval": 2, "mode": "sometimes", "extra": 1,
                "feeds": [{"url": "ftp://x"}, {"paused": true}, {"url": "https://a"}, {"url": "https://b"}],
                "advanced": {"retries": 1.5}
            }),
        );
        let errors = validate(&s, &v).unwrap_err();
        for expected in [
            "interval: must be at least 5",
            "mode: must be one of",
            "extra: unknown setting",
            "feeds: allows at most 3 items",
            "feeds[0].url: must be an http:// or https:// URL",
            "feeds[1].url: required",
            "advanced.retries: expected integer",
        ] {
            assert!(
                errors.iter().any(|e| e.starts_with(expected)),
                "missing {expected:?} in {errors:#?}"
            );
        }
    }
}
