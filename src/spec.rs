use std::{collections::BTreeMap, fs, path::Path};

use serde_json::{Map, Value};

use crate::model::{ApiParameter, Endpoint, MediaTypeSpec, RequestBodySpec, ResponseSpec};

const METHOD_ORDER: [&str; 8] = [
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];
const MAX_REF_DEPTH: usize = 128;
const MAX_RESOLVED_NODES: usize = 50_000;
const MAX_RESOLVED_BYTES: usize = 4 * 1024 * 1024;

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
    validate_openapi_version(document)?;
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .ok_or_else(|| "spec contains no paths".to_string())?;
    let resolver = LocalRefResolver::new(document);
    resolver.validate_local_refs(document)?;

    let mut path_names: Vec<_> = paths.keys().collect();
    path_names.sort_unstable();

    let mut endpoints = Vec::new();
    let mut resolution_budget = ResolutionBudget::new();
    for path in path_names {
        let Some(item) = paths.get(path) else {
            continue;
        };
        let item = resolver.resolve(item, &mut resolution_budget)?;
        let Some(item) = item.as_object() else {
            continue;
        };
        let path_parameters = parse_parameters(item.get("parameters"));

        for method in METHOD_ORDER {
            let Some(operation) = item.get(method).and_then(Value::as_object) else {
                continue;
            };
            let operation_parameters = parse_parameters(operation.get("parameters"));
            endpoints.push(Endpoint {
                summary: string_field(operation, "summary"),
                operation_id: string_field(operation, "operationId"),
                tags: operation
                    .get("tags")
                    .and_then(Value::as_array)
                    .map(|tags| {
                        tags.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                parameters: merge_parameters(&path_parameters, operation_parameters),
                request_body: operation.get("requestBody").and_then(parse_request_body),
                responses: parse_responses(operation.get("responses")),
                ..Endpoint::new(method.to_ascii_uppercase(), path)
            });
        }
    }

    if endpoints.is_empty() {
        return Err("spec contains no API operations".into());
    }
    Ok(endpoints)
}

fn validate_openapi_version(document: &Value) -> Result<(), String> {
    let version = document
        .get("openapi")
        .and_then(Value::as_str)
        .ok_or_else(|| "spec is missing the required string field `openapi`".to_string())?;
    let parts: Vec<_> = version.split('.').collect();
    let supported = matches!(parts.as_slice(), ["3", "0"] | ["3", "1"])
        || matches!(
            parts.as_slice(),
            ["3", "0" | "1", patch]
                if !patch.is_empty() && patch.bytes().all(|byte| byte.is_ascii_digit())
        );
    if supported {
        return Ok(());
    }
    Err(format!(
        "unsupported OpenAPI version `{version}`; LazyAPI supports OpenAPI 3.0 and 3.1"
    ))
}

fn string_field(object: &Map<String, Value>, name: &str) -> Option<String> {
    object.get(name).and_then(Value::as_str).map(str::to_string)
}

fn parse_parameters(value: Option<&Value>) -> Vec<ApiParameter> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|parameter| {
            let parameter = parameter.as_object()?;
            let name = string_field(parameter, "name")?;
            let location = string_field(parameter, "in")?;
            let schema = parameter.get("schema").cloned().or_else(|| {
                parameter
                    .get("content")
                    .and_then(Value::as_object)
                    .and_then(|content| content.values().next())
                    .and_then(|media| media.get("schema"))
                    .cloned()
            });
            let example = parameter.get("example").cloned().or_else(|| {
                schema
                    .as_ref()
                    .and_then(|schema| schema.get("example").cloned())
            });
            Some(ApiParameter {
                name,
                required: location == "path"
                    || parameter
                        .get("required")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                location,
                style: parameter
                    .get("style")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                explode: parameter.get("explode").and_then(Value::as_bool),
                description: string_field(parameter, "description"),
                schema,
                example,
            })
        })
        .collect()
}

fn merge_parameters(
    path_parameters: &[ApiParameter],
    operation_parameters: Vec<ApiParameter>,
) -> Vec<ApiParameter> {
    let mut merged = path_parameters.to_vec();
    for parameter in operation_parameters {
        if let Some(index) = merged.iter().position(|existing| {
            existing.name == parameter.name && existing.location == parameter.location
        }) {
            merged[index] = parameter;
        } else {
            merged.push(parameter);
        }
    }
    merged
}

fn parse_request_body(value: &Value) -> Option<RequestBodySpec> {
    let body = value.as_object()?;
    Some(RequestBodySpec {
        required: body
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        description: string_field(body, "description"),
        content: parse_content(body.get("content")),
    })
}

fn parse_responses(value: Option<&Value>) -> BTreeMap<String, ResponseSpec> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|responses| responses.iter())
        .filter_map(|(status, response)| {
            let response = response.as_object()?;
            Some((
                status.clone(),
                ResponseSpec {
                    description: string_field(response, "description"),
                    content: parse_content(response.get("content")),
                },
            ))
        })
        .collect()
}

fn parse_content(value: Option<&Value>) -> BTreeMap<String, MediaTypeSpec> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|content| content.iter())
        .filter_map(|(content_type, media)| {
            let media = media.as_object()?;
            Some((
                content_type.clone(),
                MediaTypeSpec {
                    schema: media.get("schema").cloned(),
                    example: media.get("example").cloned(),
                    examples: parse_examples(media.get("examples")),
                },
            ))
        })
        .collect()
}

fn parse_examples(value: Option<&Value>) -> BTreeMap<String, Value> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|examples| examples.iter())
        .filter_map(|(name, example)| {
            let value = example.get("value")?.clone();
            Some((name.clone(), value))
        })
        .collect()
}

struct LocalRefResolver<'a> {
    root: &'a Value,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ResolutionContext {
    Structural,
    Literal,
    MapEntries,
    Schema,
    SchemaArray,
    SchemaMap,
    Examples,
    ExampleObject,
}

impl ResolutionContext {
    fn allows_reference(self) -> bool {
        matches!(self, Self::Structural | Self::Schema | Self::ExampleObject)
    }
}

struct ResolutionBudget {
    nodes_remaining: usize,
    bytes_remaining: usize,
}

impl ResolutionBudget {
    fn new() -> Self {
        Self {
            nodes_remaining: MAX_RESOLVED_NODES,
            bytes_remaining: MAX_RESOLVED_BYTES,
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<(), String> {
        self.nodes_remaining = self.nodes_remaining.checked_sub(1).ok_or_else(|| {
            format!("OpenAPI reference expansion exceeded {MAX_RESOLVED_NODES} output nodes")
        })?;
        self.charge_bytes(bytes)
    }

    fn charge_key(&mut self, key: &str) -> Result<(), String> {
        self.charge_bytes(key.len())
    }

    fn charge_bytes(&mut self, bytes: usize) -> Result<(), String> {
        self.bytes_remaining = self.bytes_remaining.checked_sub(bytes).ok_or_else(|| {
            format!(
                "OpenAPI reference expansion exceeded {} MiB of output",
                MAX_RESOLVED_BYTES / (1024 * 1024)
            )
        })?;
        Ok(())
    }

    fn charge_literal(&mut self, value: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| format!("could not measure OpenAPI literal: {error}"))?
            .len();
        self.charge(bytes)
    }
}

impl<'a> LocalRefResolver<'a> {
    fn new(root: &'a Value) -> Self {
        Self { root }
    }

    fn validate_local_refs(&self, value: &Value) -> Result<(), String> {
        self.validate_local_refs_inner(value, ResolutionContext::Structural)
    }

    fn validate_local_refs_inner(
        &self,
        value: &Value,
        context: ResolutionContext,
    ) -> Result<(), String> {
        if context == ResolutionContext::Literal {
            return Ok(());
        }
        match value {
            Value::Array(values) => values.iter().try_for_each(|value| {
                self.validate_local_refs_inner(value, array_child_context(context))
            }),
            Value::Object(object) => {
                if context.allows_reference()
                    && let Some(reference_value) = object.get("$ref")
                {
                    let reference = reference_value
                        .as_str()
                        .ok_or_else(|| "OpenAPI `$ref` values must be strings".to_string())?;
                    if !is_local_reference(reference) {
                        return Err(format!(
                            "unsupported OpenAPI reference `{reference}`; only local JSON Pointer references (`#/...`) are supported"
                        ));
                    }
                    self.local_target(reference)?;
                }
                object.iter().try_for_each(|(name, value)| {
                    self.validate_local_refs_inner(value, child_context(context, name))
                })
            }
            _ => Ok(()),
        }
    }

    fn resolve(&self, value: &Value, budget: &mut ResolutionBudget) -> Result<Value, String> {
        self.resolve_inner(
            value,
            &mut Vec::new(),
            0,
            ResolutionContext::Structural,
            budget,
        )
    }

    fn resolve_inner(
        &self,
        value: &Value,
        stack: &mut Vec<String>,
        depth: usize,
        context: ResolutionContext,
        budget: &mut ResolutionBudget,
    ) -> Result<Value, String> {
        if depth >= MAX_REF_DEPTH {
            return Err("OpenAPI reference resolution exceeded the supported depth".into());
        }
        if context == ResolutionContext::Literal {
            budget.charge_literal(value)?;
            return Ok(value.clone());
        }
        budget.charge(scalar_size(value))?;

        match value {
            Value::Array(values) => values
                .iter()
                .map(|value| {
                    self.resolve_inner(value, stack, depth, array_child_context(context), budget)
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Value::Object(object) => {
                if context.allows_reference()
                    && let Some(reference) = object.get("$ref").and_then(Value::as_str)
                {
                    if stack.iter().any(|active| active == reference) {
                        let recursive = object
                            .iter()
                            .map(|(name, value)| {
                                budget.charge_key(name)?;
                                if name == "$ref" {
                                    budget.charge(scalar_size(value))?;
                                    Ok((name.clone(), value.clone()))
                                } else {
                                    self.resolve_inner(
                                        value,
                                        stack,
                                        depth,
                                        child_context(context, name),
                                        budget,
                                    )
                                    .map(|value| (name.clone(), value))
                                }
                            })
                            .collect::<Result<Map<_, _>, String>>()?;
                        return Ok(Value::Object(recursive));
                    }

                    let target = self.local_target(reference)?;
                    stack.push(reference.to_string());
                    let resolved = self.resolve_inner(target, stack, depth + 1, context, budget);
                    stack.pop();
                    let resolved = resolved?;
                    let mut siblings = Map::new();
                    for (name, value) in object.iter().filter(|(name, _)| name.as_str() != "$ref") {
                        budget.charge_key(name)?;
                        siblings.insert(
                            name.clone(),
                            self.resolve_inner(
                                value,
                                stack,
                                depth,
                                child_context(context, name),
                                budget,
                            )?,
                        );
                    }
                    return match resolved {
                        Value::Object(mut resolved) => {
                            resolved.extend(siblings);
                            Ok(Value::Object(resolved))
                        }
                        Value::Bool(allowed) if context == ResolutionContext::Schema => {
                            if siblings.is_empty() {
                                return Ok(Value::Bool(allowed));
                            }
                            if allowed {
                                Ok(Value::Object(siblings))
                            } else {
                                let mut branches = vec![Value::Bool(false)];
                                if let Some(existing) = siblings.remove("allOf") {
                                    match existing {
                                        Value::Array(existing) => branches.extend(existing),
                                        existing => branches.push(existing),
                                    }
                                }
                                siblings.insert("allOf".into(), Value::Array(branches));
                                Ok(Value::Object(siblings))
                            }
                        }
                        _ => Err(format!(
                            "OpenAPI reference `{reference}` resolves to a value that is invalid in this position"
                        )),
                    };
                }

                object
                    .iter()
                    .map(|(name, value)| {
                        budget.charge_key(name)?;
                        self.resolve_inner(
                            value,
                            stack,
                            depth,
                            child_context(context, name),
                            budget,
                        )
                        .map(|value| (name.clone(), value))
                    })
                    .collect::<Result<Map<_, _>, String>>()
                    .map(Value::Object)
            }
            _ => Ok(value.clone()),
        }
    }

    fn local_target(&self, reference: &str) -> Result<&'a Value, String> {
        let Some(pointer) = local_pointer(reference)? else {
            return Err(format!("invalid local reference `{reference}`"));
        };
        if pointer.is_empty() {
            return Ok(self.root);
        }
        self.root
            .pointer(&pointer)
            .ok_or_else(|| format!("dangling local reference `{reference}`"))
    }
}

fn child_context(parent: ResolutionContext, field: &str) -> ResolutionContext {
    use ResolutionContext::{
        ExampleObject, Examples, Literal, MapEntries, Schema, SchemaArray, SchemaMap, Structural,
    };
    match parent {
        Literal => Literal,
        MapEntries => Structural,
        SchemaMap => Schema,
        Examples => ExampleObject,
        SchemaArray => Schema,
        ExampleObject if field == "value" || field.starts_with("x-") => Literal,
        ExampleObject => Structural,
        Schema => match field {
            "example" | "examples" | "default" | "const" | "enum" => Literal,
            "properties" | "$defs" | "definitions" | "patternProperties" | "dependentSchemas" => {
                SchemaMap
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => SchemaArray,
            "items"
            | "not"
            | "additionalProperties"
            | "if"
            | "then"
            | "else"
            | "contains"
            | "propertyNames"
            | "unevaluatedProperties"
            | "unevaluatedItems" => Schema,
            _ if field.starts_with("x-") => Literal,
            _ => Structural,
        },
        Structural => match field {
            "schema" => Schema,
            "schemas" => SchemaMap,
            "examples" => Examples,
            "example" => Literal,
            "paths" | "webhooks" | "responses" | "parameters" | "requestBodies" | "headers"
            | "securitySchemes" | "links" | "callbacks" | "pathItems" | "content" | "encoding" => {
                MapEntries
            }
            _ if field.starts_with("x-") => Literal,
            _ => Structural,
        },
    }
}

fn array_child_context(parent: ResolutionContext) -> ResolutionContext {
    match parent {
        ResolutionContext::SchemaArray => ResolutionContext::Schema,
        ResolutionContext::Literal => ResolutionContext::Literal,
        _ => ResolutionContext::Structural,
    }
}

fn scalar_size(value: &Value) -> usize {
    match value {
        Value::String(value) => value.len(),
        Value::Number(value) => value.to_string().len(),
        Value::Bool(_) => 5,
        Value::Null => 4,
        Value::Array(_) | Value::Object(_) => 1,
    }
}

fn is_local_reference(reference: &str) -> bool {
    reference == "#" || reference.starts_with("#/")
}

fn local_pointer(reference: &str) -> Result<Option<String>, String> {
    if reference == "#" {
        return Ok(Some(String::new()));
    }
    let Some(pointer) = reference
        .strip_prefix('#')
        .filter(|pointer| pointer.starts_with('/'))
    else {
        return Ok(None);
    };
    let bytes = pointer.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let Some(high) = bytes.get(index + 1).and_then(|byte| hex_value(*byte)) else {
                return Err(format!(
                    "invalid percent-encoding in reference `{reference}`"
                ));
            };
            let Some(low) = bytes.get(index + 2).and_then(|byte| hex_value(*byte)) else {
                return Err(format!(
                    "invalid percent-encoding in reference `{reference}`"
                ));
            };
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map(Some)
        .map_err(|_| format!("reference `{reference}` is not valid UTF-8 after percent-decoding"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, io::Write};

    use serde_json::json;
    use tempfile::NamedTempFile;

    use super::{MAX_RESOLVED_BYTES, load_spec};
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
                Endpoint::new("GET", "/items"),
                Endpoint::new("PATCH", "/items"),
                Endpoint::new("DELETE", "/items"),
                Endpoint::new("HEAD", "/items"),
                Endpoint::new("OPTIONS", "/items"),
                Endpoint::new("TRACE", "/z-last"),
            ]
        );
    }

    #[test]
    fn loads_yaml() {
        let mut file = NamedTempFile::with_suffix(".yaml").unwrap();
        file.write_all(
            b"openapi: 3.0.3\npaths:\n  /health:\n    get:\n      responses:\n        200:\n          description: Healthy\n",
        )
        .unwrap();

        let endpoints = load_spec(file.path()).unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].method, "GET");
        assert_eq!(endpoints[0].path, "/health");
        assert_eq!(
            endpoints[0].responses["200"].description.as_deref(),
            Some("Healthy")
        );
    }

    #[test]
    fn loads_operation_metadata_and_merges_parameters() {
        let mut file = NamedTempFile::with_suffix(".yaml").unwrap();
        file.write_all(
            r#"openapi: 3.1.0
paths:
  /users/{id}:
    parameters:
      - name: id
        in: path
        schema: { type: string }
      - name: locale
        in: query
        schema: { type: string }
    get:
      summary: Get one user
      operationId: getUser
      tags: [users, public]
      parameters:
        - name: locale
          in: query
          required: true
          schema: { type: string, example: en }
      responses:
        "200":
          description: Found
          content:
            application/json:
              schema: { type: object }
              examples:
                ada:
                  value: { id: "42", name: Ada }
"#
            .as_bytes(),
        )
        .unwrap();

        let endpoint = load_spec(file.path()).unwrap().remove(0);
        assert_eq!(endpoint.summary.as_deref(), Some("Get one user"));
        assert_eq!(endpoint.operation_id.as_deref(), Some("getUser"));
        assert_eq!(endpoint.tags, ["users", "public"]);
        assert_eq!(endpoint.parameters.len(), 2);
        assert!(
            endpoint
                .parameters
                .iter()
                .all(|parameter| parameter.required)
        );
        assert_eq!(
            endpoint.responses["200"].content["application/json"].examples["ada"],
            json!({ "id": "42", "name": "Ada" })
        );
    }

    #[test]
    fn captures_parameter_serialization_metadata() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/items": {
                        "get": {
                            "parameters": [{
                                "name": "ids",
                                "in": "query",
                                "style": "pipeDelimited",
                                "explode": false,
                                "schema": { "type": "array", "items": { "type": "integer" } }
                            }]
                        }
                    }
                }
            })
        )
        .unwrap();

        let endpoint = load_spec(file.path()).unwrap().remove(0);
        assert_eq!(
            endpoint.parameters[0].style.as_deref(),
            Some("pipeDelimited")
        );
        assert_eq!(endpoint.parameters[0].explode, Some(false));
    }

    #[test]
    fn resolves_common_local_component_references() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/users": {
                        "post": {
                            "parameters": [{ "$ref": "#/components/parameters/TraceId" }],
                            "requestBody": { "$ref": "#/components/requestBodies/UserBody" },
                            "responses": {
                                "201": { "$ref": "#/components/responses/UserCreated" }
                            }
                        }
                    }
                },
                "components": {
                    "parameters": {
                        "TraceId": {
                            "name": "X-Trace-Id",
                            "in": "header",
                            "required": true,
                            "schema": { "type": "string" }
                        }
                    },
                    "requestBodies": {
                        "UserBody": {
                            "required": true,
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/User" }
                                }
                            }
                        }
                    },
                    "responses": {
                        "UserCreated": {
                            "description": "Created",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/User" }
                                }
                            }
                        }
                    },
                    "schemas": {
                        "User": {
                            "type": "object",
                            "required": ["id"],
                            "properties": {
                                "id": { "type": "integer", "example": 9 },
                                "manager": { "$ref": "#/components/schemas/User" }
                            }
                        }
                    }
                }
            })
        )
        .unwrap();

        let endpoint = load_spec(file.path()).unwrap().remove(0);
        assert_eq!(endpoint.parameters[0].name, "X-Trace-Id");
        assert!(endpoint.request_body.as_ref().unwrap().required);
        let schema = endpoint.responses["201"].content["application/json"]
            .schema
            .as_ref()
            .unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["id"]["example"], 9);
        assert_eq!(
            schema["properties"]["manager"]["$ref"],
            "#/components/schemas/User"
        );
        assert_eq!(endpoint.mock_response().unwrap().status, 201);
    }

    #[test]
    fn rejects_dangling_local_references_even_in_unused_components() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/health": {
                        "get": {
                            "responses": {
                                "204": { "description": "Healthy" }
                            }
                        }
                    }
                },
                "components": {
                    "schemas": {
                        "Unused": { "$ref": "#/components/schemas/Missing" }
                    }
                }
            })
        )
        .unwrap();

        let error = load_spec(file.path()).unwrap_err();
        assert!(error.contains("dangling local reference"));
        assert!(error.contains("#/components/schemas/Missing"));
    }

    #[test]
    fn rejects_missing_or_unsupported_openapi_versions() {
        for document in [
            json!({ "paths": { "/health": { "get": {} } } }),
            json!({ "swagger": "2.0", "paths": { "/health": { "get": {} } } }),
            json!({ "openapi": "3.2.0", "paths": { "/health": { "get": {} } } }),
            json!({ "openapi": "3.0.not-a-version", "paths": { "/health": { "get": {} } } }),
            json!({ "openapi": "3.1.0.1", "paths": { "/health": { "get": {} } } }),
        ] {
            let mut file = NamedTempFile::with_suffix(".json").unwrap();
            write!(file, "{document}").unwrap();
            let error = load_spec(file.path()).unwrap_err();
            assert!(error.contains("openapi") || error.contains("OpenAPI version"));
        }
    }

    #[test]
    fn rejects_external_and_anchor_references() {
        for reference in [
            "other.yaml#/User",
            "https://example.com/openapi.json",
            "#User",
        ] {
            let mut file = NamedTempFile::with_suffix(".json").unwrap();
            write!(
                file,
                "{}",
                json!({
                    "openapi": "3.1.0",
                    "paths": {
                        "/users": {
                            "get": {
                                "responses": {
                                    "200": {
                                        "description": "ok",
                                        "content": {
                                            "application/json": {
                                                "schema": { "$ref": reference }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                })
            )
            .unwrap();
            let error = load_spec(file.path()).unwrap_err();
            assert!(error.contains("only local JSON Pointer references"));
            assert!(error.contains(reference));
        }
    }

    #[test]
    fn literal_ref_fields_in_examples_defaults_consts_and_enums_are_not_resolved() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/literal": {
                        "get": {
                            "responses": {
                                "200": {
                                    "description": "ok",
                                    "content": {
                                        "application/json": {
                                            "example": { "$ref": "literal example value" },
                                            "examples": {
                                                "named": {
                                                    "value": { "$ref": "literal examples value" }
                                                }
                                            },
                                            "schema": {
                                                "type": "object",
                                                "default": { "$ref": "literal default value" },
                                                "const": { "$ref": "literal const value" },
                                                "enum": [{ "$ref": "literal enum value" }],
                                                "properties": {
                                                    "example": { "$ref": "#/components/schemas/Leaf" },
                                                    "value": { "$ref": "#/components/schemas/Leaf" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "components": {
                    "schemas": {
                        "Leaf": { "type": "string" }
                    }
                }
            })
        )
        .unwrap();

        let endpoint = load_spec(file.path()).unwrap().remove(0);
        let media = &endpoint.responses["200"].content["application/json"];
        assert_eq!(
            media.example.as_ref().unwrap()["$ref"],
            "literal example value"
        );
        assert_eq!(media.examples["named"]["$ref"], "literal examples value");
        let schema = media.schema.as_ref().unwrap();
        assert_eq!(schema["default"]["$ref"], "literal default value");
        assert_eq!(schema["const"]["$ref"], "literal const value");
        assert_eq!(schema["enum"][0]["$ref"], "literal enum value");
        assert_eq!(schema["properties"]["example"]["type"], "string");
        assert_eq!(schema["properties"]["value"]["type"], "string");
    }

    #[test]
    fn recursive_schema_branches_are_partial_when_they_are_exercised() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/node": {
                        "get": {
                            "responses": {
                                "200": {
                                    "description": "ok",
                                    "content": {
                                        "application/json": {
                                            "schema": { "$ref": "#/components/schemas/Node" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "components": {
                    "schemas": {
                        "Node": {
                            "type": "object",
                            "properties": {
                                "next": { "$ref": "#/components/schemas/Node" }
                            }
                        }
                    }
                }
            })
        )
        .unwrap();

        let endpoint = load_spec(file.path()).unwrap().remove(0);
        let mut entry = crate::model::LogEntry {
            method: "GET".into(),
            path: "/node".into(),
            status: 200,
            response: crate::model::ExchangePart {
                headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
                body: r#"{"next":{}}"#.into(),
                size: 11,
                ..crate::model::ExchangePart::default()
            },
            ..crate::model::LogEntry::default()
        };
        entry.validate_against(&[endpoint]);
        assert!(entry.contract.inconclusive);
        assert!(!entry.contract.is_valid());
        assert!(
            entry
                .contract
                .violations
                .iter()
                .any(|violation| { violation.message.contains("recursive `$ref`") })
        );
    }

    #[test]
    fn reference_expansion_has_an_output_budget() {
        let mut schemas = serde_json::Map::new();
        schemas.insert("S0".into(), json!({ "type": "string" }));
        for index in 1..=16 {
            let previous = format!("#/components/schemas/S{}", index - 1);
            schemas.insert(
                format!("S{index}"),
                json!({ "allOf": [{ "$ref": previous }, { "$ref": previous }] }),
            );
        }
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/large": {
                        "get": {
                            "responses": {
                                "200": {
                                    "description": "ok",
                                    "content": {
                                        "application/json": {
                                            "schema": { "$ref": "#/components/schemas/S16" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "components": { "schemas": schemas }
            })
        )
        .unwrap();

        let error = load_spec(file.path()).unwrap_err();
        assert!(error.contains("reference expansion exceeded"));
    }

    #[test]
    fn reference_expansion_budget_is_shared_across_all_paths() {
        let mut paths = serde_json::Map::new();
        for index in 0..20 {
            paths.insert(
                format!("/path-{index:02}"),
                json!({
                    "get": {
                        "responses": {
                            "200": {
                                "description": "ok",
                                "content": {
                                    "application/json": {
                                        "schema": { "$ref": "#/components/schemas/Large" }
                                    }
                                }
                            }
                        }
                    }
                }),
            );
        }
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": paths,
                "components": {
                    "schemas": {
                        "Large": {
                            "type": "string",
                            "default": "x".repeat(300_000)
                        }
                    }
                }
            })
        )
        .unwrap();

        let error = load_spec(file.path()).unwrap_err();
        assert!(error.contains("reference expansion exceeded"));
    }

    #[test]
    fn reference_expansion_byte_budget_counts_object_keys() {
        let mut properties = serde_json::Map::new();
        properties.insert(
            "k".repeat(MAX_RESOLVED_BYTES + 1),
            json!({ "type": "string" }),
        );
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/large-key": {
                        "get": {
                            "responses": {
                                "200": {
                                    "description": "ok",
                                    "content": {
                                        "application/json": {
                                            "schema": {
                                                "type": "object",
                                                "properties": properties
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            })
        )
        .unwrap();

        let error = load_spec(file.path()).unwrap_err();
        assert!(error.contains("reference expansion exceeded"));
    }

    #[test]
    fn resolves_percent_encoded_json_pointer_fragments() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/value": {
                        "get": {
                            "responses": {
                                "200": {
                                    "description": "ok",
                                    "content": {
                                        "application/json": {
                                            "schema": { "$ref": "#/components/schemas/Foo%20Bar~1Thing" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "components": {
                    "schemas": {
                        "Foo Bar/Thing": { "type": "string" }
                    }
                }
            })
        )
        .unwrap();

        let endpoint = load_spec(file.path()).unwrap().remove(0);
        assert_eq!(
            endpoint.responses["200"].content["application/json"]
                .schema
                .as_ref()
                .unwrap()["type"],
            "string"
        );
    }

    #[test]
    fn preserves_schema_ref_siblings_for_boolean_targets() {
        let mut file = NamedTempFile::with_suffix(".json").unwrap();
        write!(
            file,
            "{}",
            json!({
                "openapi": "3.1.0",
                "paths": {
                    "/value": {
                        "get": {
                            "responses": {
                                "200": {
                                    "description": "ok",
                                    "content": {
                                        "application/json": {
                                            "schema": {
                                                "$ref": "#/components/schemas/Anything",
                                                "type": "string",
                                                "minLength": 3
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                "components": {
                    "schemas": { "Anything": true }
                }
            })
        )
        .unwrap();

        let endpoint = load_spec(file.path()).unwrap().remove(0);
        let schema = endpoint.responses["200"].content["application/json"]
            .schema
            .as_ref()
            .unwrap();
        assert_eq!(schema["type"], "string");
        assert_eq!(schema["minLength"], 3);
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
