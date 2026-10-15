//! Parameters checked against a plugin command's JSON Schema before the
//! plugin sees them (S57, ADR-0006 X11). The subset commands use: `type`,
//! `properties`, `required`, `additionalProperties: false`, `enum`,
//! `const`, numeric bounds, string and list lengths, and `items`. Other
//! keywords are not checked; the plugin checks what it needs beyond these.

use serde_json::Value;

/// Where a value failed, such as `params.label: expected text`.
///
/// # Errors
/// The first place the value does not meet the schema.
pub fn check(schema: &Value, value: &Value) -> Result<(), String> {
    check_at(schema, value, "params")
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn words(kind: &str) -> &'static str {
    match kind {
        "null" => "nothing",
        "boolean" => "true or false",
        "integer" => "a whole number",
        "number" => "a number",
        "string" => "text",
        "array" => "a list",
        "object" => "an object",
        _ => "something else",
    }
}

fn is(value: &Value, wanted: &str) -> bool {
    let actual = kind(value);
    actual == wanted
        || (wanted == "number" && actual == "integer")
        // 2.0 is a whole number.
        || (wanted == "integer" && value.as_f64().is_some_and(|f| f.fract() == 0.0))
}

/// Numbers' bounds, and the lengths of text and lists, and each item.
fn bounds(schema: &serde_json::Map<String, Value>, value: &Value, at: &str) -> Result<(), String> {
    if let Some(number) = value.as_f64() {
        let bound = |key: &str| schema.get(key).and_then(Value::as_f64);
        if let Some(min) = bound("minimum")
            && number < min
        {
            return Err(format!("{at}: at least {min}"));
        }
        if let Some(max) = bound("maximum")
            && number > max
        {
            return Err(format!("{at}: at most {max}"));
        }
        if let Some(min) = bound("exclusiveMinimum")
            && number <= min
        {
            return Err(format!("{at}: more than {min}"));
        }
        if let Some(max) = bound("exclusiveMaximum")
            && number >= max
        {
            return Err(format!("{at}: less than {max}"));
        }
    }
    let length = |key: &str| schema.get(key).and_then(Value::as_u64);
    if let Some(text) = value.as_str() {
        let chars = text.chars().count() as u64;
        if let Some(min) = length("minLength")
            && chars < min
        {
            return Err(format!("{at}: at least {min} characters"));
        }
        if let Some(max) = length("maxLength")
            && chars > max
        {
            return Err(format!("{at}: at most {max} characters"));
        }
    }
    if let Some(items) = value.as_array() {
        let count = items.len() as u64;
        if let Some(min) = length("minItems")
            && count < min
        {
            return Err(format!("{at}: at least {min} items"));
        }
        if let Some(max) = length("maxItems")
            && count > max
        {
            return Err(format!("{at}: at most {max} items"));
        }
        if let Some(each) = schema.get("items") {
            for (n, item) in items.iter().enumerate() {
                check_at(each, item, &format!("{at}[{n}]"))?;
            }
        }
    }
    Ok(())
}

fn check_at(schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    match schema.get("type") {
        Some(Value::String(wanted)) if !is(value, wanted) => {
            return Err(format!("{at}: expected {}", words(wanted)));
        }
        Some(Value::Array(wanted)) => {
            let any = wanted
                .iter()
                .filter_map(Value::as_str)
                .any(|w| is(value, w));
            if !any {
                let list: Vec<&str> = wanted.iter().filter_map(Value::as_str).map(words).collect();
                return Err(format!("{at}: expected {}", list.join(" or ")));
            }
        }
        _ => {}
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!(
            "{at}: not one of {}",
            Value::Array(allowed.clone())
        ));
    }
    if let Some(only) = schema.get("const")
        && only != value
    {
        return Err(format!("{at}: must be {only}"));
    }
    bounds(schema, value, at)?;
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        for name in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(name) {
                return Err(format!("{at}.{name}: missing"));
            }
        }
        for (name, field) in object {
            match properties.and_then(|p| p.get(name)) {
                Some(rule) => check_at(rule, field, &format!("{at}.{name}"))?,
                None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return Err(format!("{at}.{name}: not a parameter of this command"));
                }
                None => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parameters_are_checked_where_they_go_wrong() {
        let schema = json!({
            "type": "object",
            "properties": {
                "label": { "type": "string", "maxLength": 5 },
                "count": { "type": "integer", "minimum": 1, "maximum": 3 },
                "mode": { "enum": ["fast", "slow"] },
                "points": { "type": "array", "items": { "type": "number" }, "maxItems": 2 }
            },
            "required": ["label"],
            "additionalProperties": false
        });
        assert_eq!(check(&schema, &json!({ "label": "A", "count": 2 })), Ok(()));
        assert_eq!(
            check(&schema, &json!({ "label": "A", "count": 2.0 })),
            Ok(())
        );
        assert_eq!(
            check(&schema, &json!({})),
            Err("params.label: missing".into())
        );
        assert_eq!(
            check(&schema, &json!({ "label": 7 })),
            Err("params.label: expected text".into())
        );
        assert_eq!(
            check(&schema, &json!({ "label": "too long" })),
            Err("params.label: at most 5 characters".into())
        );
        assert_eq!(
            check(&schema, &json!({ "label": "A", "count": 9 })),
            Err("params.count: at most 3".into())
        );
        assert_eq!(
            check(&schema, &json!({ "label": "A", "mode": "medium" })),
            Err(r#"params.mode: not one of ["fast","slow"]"#.into())
        );
        assert_eq!(
            check(&schema, &json!({ "label": "A", "points": [1, "x"] })),
            Err("params.points[1]: expected a number".into())
        );
        assert_eq!(
            check(&schema, &json!({ "label": "A", "extra": true })),
            Err("params.extra: not a parameter of this command".into())
        );
    }
}
