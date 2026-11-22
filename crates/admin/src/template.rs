use hubdouble_model::ApiError;

pub(crate) fn expand(
    text: &str,
    mut f: impl FnMut(&str, &[String]) -> Result<Option<String>, ApiError>,
) -> Result<String, ApiError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let replaced = match parse_call(after) {
            Some((name, args, used)) => f(name, &args)?.map(|v| (v, used)),
            None => None,
        };
        if let Some((value, used)) = replaced {
            out.push_str(&value);
            rest = &after[used..];
        } else {
            out.push_str("{{");
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn parse_call(text: &str) -> Option<(&str, Vec<String>, usize)> {
    let body = text.trim_start();
    let name_len = body
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(body.len());
    let name = &body[..name_len];
    let mut rest = &body[name_len..];
    if name.is_empty() || !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut args = Vec::new();
    loop {
        rest = rest.trim_start();
        if let Some(tail) = rest.strip_prefix("}}") {
            if args.is_empty() {
                return None;
            }
            return Some((name, args, text.len() - tail.len()));
        }
        let quoted = rest.strip_prefix('"')?;
        let end = quoted.find('"')?;
        args.push(quoted[..end].to_owned());
        rest = &quoted[end + 1..];
    }
}

pub(crate) fn expand_env(
    value: &mut serde_json::Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(), ApiError> {
    match value {
        serde_json::Value::String(s) if s.contains("{{") => {
            *s = expand(s, |name, args| match (name, args) {
                ("env", [var]) => env(var).filter(|v| !v.is_empty()).map(Some).ok_or_else(|| {
                    ApiError::unprocessable(format!(
                        "the environment variable {var} is unset or empty"
                    ))
                }),
                _ => Ok(None),
            })?;
        }
        serde_json::Value::Array(items) => {
            for item in items {
                expand_env(item, env)?;
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                expand_env(item, env)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "SET" => Some("value".to_owned()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    #[test]
    fn env_templates_expand_in_nested_strings_and_leave_other_braces() {
        // Given
        let mut v = json!({"a": ["x {{env \"SET\"}} y", {"b": "{{ env \"SET\" }}"}], "c": "${{ github.sha }} {{sha \"o/r\" \"main\"}}"});
        // When
        expand_env(&mut v, &env).unwrap();
        // Then
        assert_eq!(
            v,
            json!({"a": ["x value y", {"b": "value"}], "c": "${{ github.sha }} {{sha \"o/r\" \"main\"}}"})
        );
    }

    #[test]
    fn an_unset_or_empty_variable_fails() {
        for name in ["UNSET", "EMPTY"] {
            let mut v = json!(format!("{{{{env \"{name}\"}}}}"));
            let e = expand_env(&mut v, &env).unwrap_err();
            assert_eq!(e.status, 422, "{name}");
        }
    }

    #[test]
    fn calls_take_several_arguments() {
        let out = expand("pin {{sha \"octo-org/r\" \"v5.0.0\"}}!", |name, args| {
            assert_eq!(
                (name, args),
                ("sha", &["octo-org/r".to_owned(), "v5.0.0".to_owned()][..])
            );
            Ok(Some("abc".to_owned()))
        })
        .unwrap();
        assert_eq!(out, "pin abc!");
    }
}
