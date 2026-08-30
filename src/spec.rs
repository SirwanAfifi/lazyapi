use std::{fs, path::Path};

use serde_json::Value;

use crate::model::Endpoint;

const METHOD_ORDER: [&str; 8] = [
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

pub fn load_spec(path: &Path) -> Result<Vec<Endpoint>, String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let document = parse_document(&source, path)?;
    endpoints_from_document(&document)
}

fn parse_document(source: &str, path: &Path) -> Result<Value, String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if extension == "json" {
        return serde_json::from_str(source).map_err(|error| format!("invalid JSON: {error}"));
    }

    if matches!(extension.as_str(), "yaml" | "yml") {
        return serde_yaml_ng::from_str(source).map_err(|error| format!("invalid YAML: {error}"));
    }

    serde_json::from_str(source).or_else(|json_error| {
        serde_yaml_ng::from_str(source).map_err(|yaml_error| {
            format!("document is neither valid JSON ({json_error}) nor YAML ({yaml_error})")
        })
    })
}

fn endpoints_from_document(document: &Value) -> Result<Vec<Endpoint>, String> {
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| "spec contains no paths".to_string())?;

    let mut path_names: Vec<_> = paths.keys().collect();
    path_names.sort_unstable();

    let mut endpoints = Vec::new();
    for path in path_names {
        let Some(item) = paths.get(path).and_then(Value::as_object) else {
            continue;
        };
        for method in METHOD_ORDER {
            if item.get(method).is_some_and(Value::is_object) {
                endpoints.push(Endpoint {
                    method: method.to_ascii_uppercase(),
                    path: path.clone(),
                });
            }
        }
    }

    if endpoints.is_empty() {
        return Err("spec contains no API operations".into());
    }
    Ok(endpoints)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::load_spec;
    use crate::model::Endpoint;

    #[test]
    fn loads_every_operation_in_stable_order() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            r#"{{
              "openapi": "3.0.3",
              "paths": {{
                "/z-last": {{"trace": {{}}}},
                "/items": {{"delete": {{}}, "patch": {{}}, "get": {{}}, "head": {{}}, "options": {{}}}}
              }}
            }}"#
        )
        .unwrap();

        let endpoints = load_spec(file.path()).unwrap();
        assert_eq!(
            endpoints,
            vec![
                Endpoint {
                    method: "GET".into(),
                    path: "/items".into()
                },
                Endpoint {
                    method: "PATCH".into(),
                    path: "/items".into()
                },
                Endpoint {
                    method: "DELETE".into(),
                    path: "/items".into()
                },
                Endpoint {
                    method: "HEAD".into(),
                    path: "/items".into()
                },
                Endpoint {
                    method: "OPTIONS".into(),
                    path: "/items".into()
                },
                Endpoint {
                    method: "TRACE".into(),
                    path: "/z-last".into()
                },
            ]
        );
    }

    #[test]
    fn loads_yaml() {
        let mut file = NamedTempFile::with_suffix(".yaml").unwrap();
        write!(
            file,
            "openapi: 3.0.3\npaths:\n  /health:\n    get:\n      responses: {{}}\n"
        )
        .unwrap();

        let endpoints = load_spec(file.path()).unwrap();
        assert_eq!(
            endpoints,
            vec![Endpoint {
                method: "GET".into(),
                path: "/health".into()
            }]
        );
    }

    #[test]
    fn rejects_documents_without_operations() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(file, r#"{{"openapi":"3.0.3","paths":{{}}}}"#).unwrap();
        assert!(
            load_spec(file.path())
                .unwrap_err()
                .contains("no API operations")
        );
    }
}
