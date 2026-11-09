//! The JSON Schema subset `vaern:ai` checks structured replies against.
//!
//! A provider asked for `response_format: json_schema` may still answer with
//! text that does not match, and some local servers ignore the request
//! altogether. The runtime therefore checks the parsed reply itself before
//! the script sees it, so a script can rely on the shape it asked for.
//!
//! Only the keywords below are understood: `type` (one name or a list),
//! `properties`, `required`, `items`, `enum` and `additionalProperties` (a
//! boolean), plus the annotations `title` and `description`. A schema using
//! any other keyword is refused when the call is prepared, rather than
//! checked partly and reported as valid.

use serde_json::{Map, Value};

/// Where and why a value or a schema failed. `path` is a JSONPath-style
/// location such as `$.items[2].name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaMismatch {
    pub path: String,
    pub message: String,
}

const KEYWORDS: [&str; 8] = [
    "type",
    "properties",
    "required",
    "items",
    "enum",
    "additionalProperties",
    "title",
    "description",
];

const TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

fn mismatch(path: &str, message: String) -> SchemaMismatch {
    SchemaMismatch {
        path: path.to_string(),
        message,
    }
}

fn child(path: &str, key: &str) -> String {
    let plain = !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        format!("{path}.{key}")
    } else {
        format!("{path}[{}]", Value::from(key))
    }
}

/// Refuses a schema that uses anything outside the subset. The path names
/// the place in the schema.
pub fn check_schema(schema: &Value) -> Result<(), SchemaMismatch> {
    check_schema_at(schema, "$")
}

fn check_schema_at(schema: &Value, path: &str) -> Result<(), SchemaMismatch> {
    let Some(fields) = schema.as_object() else {
        return Err(mismatch(path, "a schema must be an object".to_string()));
    };
    for key in fields.keys() {
        if !KEYWORDS.contains(&key.as_str()) {
            return Err(mismatch(
                path,
                format!(
                    "the keyword '{key}' is not supported; use only {}",
                    KEYWORDS.join(", ")
                ),
            ));
        }
    }
    if let Some(kind) = fields.get("type") {
        let names: Vec<&Value> = match kind {
            Value::Array(list) if !list.is_empty() => list.iter().collect(),
            other => vec![other],
        };
        for name in names {
            if !name.as_str().is_some_and(|n| TYPES.contains(&n)) {
                return Err(mismatch(
                    path,
                    format!("'type' must name one of {}", TYPES.join(", ")),
                ));
            }
        }
    }
    if let Some(properties) = fields.get("properties") {
        let Some(properties) = properties.as_object() else {
            return Err(mismatch(path, "'properties' must be an object".to_string()));
        };
        for (key, sub) in properties {
            check_schema_at(sub, &child(path, key))?;
        }
    }
    if let Some(items) = fields.get("items") {
        check_schema_at(items, &format!("{path}[*]"))?;
    }
    if let Some(required) = fields.get("required")
        && !required
            .as_array()
            .is_some_and(|list| list.iter().all(Value::is_string))
    {
        return Err(mismatch(
            path,
            "'required' must be a list of property names".to_string(),
        ));
    }
    if fields.get("enum").is_some_and(|e| !e.is_array()) {
        return Err(mismatch(path, "'enum' must be a list".to_string()));
    }
    if fields
        .get("additionalProperties")
        .is_some_and(|a| !a.is_boolean())
    {
        return Err(mismatch(
            path,
            "'additionalProperties' must be true or false".to_string(),
        ));
    }
    Ok(())
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn has_type(value: &Value, name: &str) -> bool {
    match name {
        "integer" => value.as_f64().is_some_and(|n| n.fract() == 0.0),
        other => type_name(value) == other,
    }
}

/// Checks `value` against a schema that passed [`check_schema`].
pub fn validate(value: &Value, schema: &Value) -> Result<(), SchemaMismatch> {
    validate_at(value, schema, "$")
}

fn validate_at(value: &Value, schema: &Value, path: &str) -> Result<(), SchemaMismatch> {
    let empty = Map::new();
    let fields = schema.as_object().unwrap_or(&empty);
    if let Some(kind) = fields.get("type") {
        let names: Vec<&str> = match kind {
            Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
            other => other.as_str().into_iter().collect(),
        };
        if !names.iter().any(|name| has_type(value, name)) {
            return Err(mismatch(
                path,
                format!(
                    "expected {}, found {}",
                    names.join(" or "),
                    type_name(value)
                ),
            ));
        }
    }
    if let Some(allowed) = fields.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(mismatch(
            path,
            format!("{value} is not one of {}", Value::from(allowed.clone())),
        ));
    }
    if let Some(object) = value.as_object() {
        let properties = fields
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        for name in fields
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(name) {
                return Err(mismatch(
                    path,
                    format!("the required property '{name}' is missing"),
                ));
            }
        }
        let closed = fields.get("additionalProperties") == Some(&Value::Bool(false));
        for (key, item) in object {
            match properties.get(key) {
                Some(sub) => validate_at(item, sub, &child(path, key))?,
                None if closed => {
                    return Err(mismatch(
                        path,
                        format!("the property '{key}' is not allowed"),
                    ));
                }
                None => {}
            }
        }
    }
    if let (Some(list), Some(items)) = (value.as_array(), fields.get("items")) {
        for (index, item) in list.iter().enumerate() {
            validate_at(item, items, &format!("{path}[{index}]"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn person() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "age": { "type": "integer" },
                "tags": { "type": "array", "items": { "type": "string", "enum": ["a", "b"] } },
                "nick name": { "type": ["string", "null"] },
            },
            "required": ["name", "age"],
            "additionalProperties": false,
        })
    }

    fn fails(value: Value) -> (String, String) {
        let err = validate(&value, &person()).unwrap_err();
        (err.path, err.message)
    }

    #[test]
    fn test_matching_values_pass_and_mismatches_name_their_path() {
        assert!(check_schema(&person()).is_ok());
        let ok = json!({ "name": "x", "age": 3.0, "tags": ["a"], "nick name": null });
        assert_eq!(validate(&ok, &person()), Ok(()));

        assert_eq!(
            fails(json!({ "name": "x" })),
            ("$".into(), "the required property 'age' is missing".into())
        );
        assert_eq!(
            fails(json!({ "name": "x", "age": 1.5 })),
            ("$.age".into(), "expected integer, found number".into())
        );
        assert_eq!(
            fails(json!({ "name": "x", "age": 1, "tags": ["a", "c"] })),
            ("$.tags[1]".into(), r#""c" is not one of ["a","b"]"#.into())
        );
        assert_eq!(
            fails(json!({ "name": "x", "age": 1, "nick name": 2 })),
            (
                r#"$["nick name"]"#.into(),
                "expected string or null, found number".into()
            )
        );
        assert_eq!(
            fails(json!({ "name": "x", "age": 1, "extra": true })),
            ("$".into(), "the property 'extra' is not allowed".into())
        );
        assert_eq!(
            fails(json!([])),
            ("$".into(), "expected object, found array".into())
        );
    }

    #[test]
    fn test_schemas_outside_the_subset_are_refused() {
        for (schema, path) in [
            (json!({ "type": "string", "pattern": "^a" }), "$"),
            (json!({ "type": "decimal" }), "$"),
            (json!({ "properties": { "a": { "minimum": 1 } } }), "$.a"),
            (json!({ "items": { "oneOf": [] } }), "$[*]"),
            (json!({ "required": "a" }), "$"),
            (json!({ "additionalProperties": {} }), "$"),
            (json!(true), "$"),
        ] {
            let err = check_schema(&schema).unwrap_err();
            assert_eq!(err.path, path, "{schema}");
        }
    }
}
