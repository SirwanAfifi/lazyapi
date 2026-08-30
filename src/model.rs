use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashSet},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use url::form_urlencoded;

const MAX_CONTRACT_FINDINGS: usize = 100;
const MAX_SCHEMA_VALIDATION_DEPTH: usize = 64;
const MAX_SCHEMA_VALIDATION_STEPS: usize = 10_000;
const MAX_MOCK_SCHEMA_DEPTH: usize = 32;
const MAX_UNIQUE_ITEMS: usize = 10_000;
const MAX_MOCK_COLLECTION_ITEMS: usize = 256;
const MAX_MOCK_STRING_LENGTH: usize = 16_384;

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<ApiParameter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<RequestBodySpec>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub responses: BTreeMap<String, ResponseSpec>,
}

impl Endpoint {
    pub fn new(method: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            ..Self::default()
        }
    }

    pub fn matches(&self, method: &str, request_path: &str) -> bool {
        if !self.method.eq_ignore_ascii_case(method) {
            return false;
        }

        let path = request_path
            .split_once('?')
            .map_or(request_path, |(path, _)| path);
        let template = split_path(&self.path);
        let request = split_path(path);

        template.len() == request.len()
            && template.iter().zip(request).all(|(expected, actual)| {
                (expected.starts_with('{') && expected.ends_with('}')) || *expected == actual
            })
    }

    pub fn validate_exchange(&self, entry: &LogEntry) -> ContractCheck {
        let mut check = ContractCheck {
            checked: true,
            matched_endpoint: Some(
                self.operation_id
                    .clone()
                    .unwrap_or_else(|| format!("{} {}", self.method, self.path)),
            ),
            ..ContractCheck::default()
        };
        let mut budget = ValidationBudget::new();

        self.validate_parameters(entry, &mut check, &mut budget);
        self.validate_request(entry, &mut check, &mut budget);
        self.validate_response(entry, &mut check, &mut budget);
        check
    }

    pub fn mock_response(&self) -> Option<MockResponse> {
        let (status, response) = self.preferred_response()?;
        let Some((content_type, media)) = preferred_media(&response.content) else {
            return Some(MockResponse {
                status,
                content_type: None,
                body: String::new(),
            });
        };

        let explicit_example = media
            .example
            .clone()
            .or_else(|| media.examples.values().next().cloned());
        let example = explicit_example.or_else(|| {
            media.schema.as_ref().and_then(|schema| {
                let value = mock_value_from_schema(schema, SchemaDirection::Response, 0)?;
                (is_json_content_type(content_type)
                    || (is_textual_content_type(content_type)
                        && !value.is_array()
                        && !value.is_object()))
                .then_some(value)
            })
        });
        let body = example
            .as_ref()
            .map(|value| render_mock_body(value, content_type))
            .unwrap_or_default();

        Some(MockResponse {
            status,
            content_type: Some(content_type.clone()),
            body,
        })
    }

    fn preferred_response(&self) -> Option<(u16, &ResponseSpec)> {
        let mut candidates: Vec<(u16, bool, &ResponseSpec)> = self
            .responses
            .iter()
            .filter_map(|(status, response)| {
                status
                    .parse::<u16>()
                    .ok()
                    .map(|code| (code, true, response))
            })
            .collect();
        for class in 1_u16..=5 {
            let pattern = format!("{class}XX");
            if let Some(response) = self
                .responses
                .iter()
                .find(|(status, _)| status.eq_ignore_ascii_case(&pattern))
                .map(|(_, response)| response)
            {
                candidates.push((class * 100, false, response));
            }
        }

        candidates.sort_by_key(|(status, exact, _)| {
            (
                u8::from(!(200..=299).contains(status)),
                u8::from(!*exact),
                *status,
            )
        });
        if let Some((status, _, response)) = candidates.into_iter().next() {
            return Some((status, response));
        }

        self.responses
            .iter()
            .find(|(status, _)| status.eq_ignore_ascii_case("default"))
            .map(|(_, response)| (200, response))
    }

    fn response_for_status(&self, status: u16) -> Option<&ResponseSpec> {
        let exact = status.to_string();
        self.responses
            .get(&exact)
            .or_else(|| {
                let pattern = format!("{}XX", status / 100);
                self.responses
                    .iter()
                    .find(|(documented, _)| documented.eq_ignore_ascii_case(&pattern))
                    .map(|(_, response)| response)
            })
            .or_else(|| {
                self.responses
                    .iter()
                    .find(|(documented, _)| documented.eq_ignore_ascii_case("default"))
                    .map(|(_, response)| response)
            })
    }

    fn validate_parameters(
        &self,
        entry: &LogEntry,
        check: &mut ContractCheck,
        budget: &mut ValidationBudget,
    ) {
        let mut query: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if let Some(raw_query) = entry.query.as_deref() {
            for (name, value) in form_urlencoded::parse(raw_query.as_bytes()) {
                query
                    .entry(name.into_owned())
                    .or_default()
                    .push(value.into_owned());
            }
        }

        for parameter in &self.parameters {
            if check.finding_budget_exhausted() {
                break;
            }
            if !budget.consume(check, "request.parameters") {
                break;
            }
            let values = parameter_values(parameter, &self.path, entry, &query);
            if values.is_empty() {
                if !parameter.required {
                    continue;
                }
                check.push_violation(ContractViolation::new(
                    "missing_required_parameter",
                    format!("request.{}.{}", parameter.location, parameter.name),
                    format!(
                        "required {} parameter `{}` is missing",
                        parameter.location, parameter.name
                    ),
                ));
                continue;
            }

            let Some(schema) = &parameter.schema else {
                continue;
            };
            let location = format!("request.{}.{}", parameter.location, parameter.name);
            if let Some(reason) = unsupported_parameter_serialization(parameter, schema) {
                check.mark_inconclusive(&location, reason);
                continue;
            }
            if schema_has_unusable_type(schema) {
                check.mark_inconclusive(
                    &location,
                    format!(
                        "parameter `{}` uses an unsupported or malformed schema type",
                        parameter.name
                    ),
                );
                continue;
            }
            match parameter_json_value(&values, schema) {
                Ok(value) => validate_json_schema(
                    &value,
                    schema,
                    &location,
                    SchemaDirection::Neutral,
                    check,
                    budget,
                    0,
                ),
                Err(expected) => check.push_violation(ContractViolation::new(
                    "schema_type_mismatch",
                    location,
                    format!("parameter `{}` must be {expected}", parameter.name),
                )),
            }
        }
    }

    fn validate_request(
        &self,
        entry: &LogEntry,
        check: &mut ContractCheck,
        budget: &mut ValidationBudget,
    ) {
        let Some(request_body) = &self.request_body else {
            return;
        };
        let has_body = entry.request.size > 0 || !entry.request.body.is_empty();
        if request_body.required && !has_body {
            check.push_violation(ContractViolation::new(
                "missing_required_request_body",
                "request.body",
                "required request body is missing",
            ));
            return;
        }
        if has_body {
            validate_content(
                &entry.request,
                &request_body.content,
                "request",
                check,
                budget,
            );
        }
    }

    fn validate_response(
        &self,
        entry: &LogEntry,
        check: &mut ContractCheck,
        budget: &mut ValidationBudget,
    ) {
        let Some(response) = self.response_for_status(entry.status) else {
            check.push_violation(ContractViolation::new(
                "undocumented_status",
                "response.status",
                format!("response status {} is not documented", entry.status),
            ));
            return;
        };

        let has_body = entry.response.size > 0 || !entry.response.body.is_empty();
        if has_body {
            validate_content(
                &entry.response,
                &response.content,
                "response",
                check,
                budget,
            );
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiParameter {
    pub name: String,
    #[serde(rename = "in")]
    pub location: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<Value>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestBodySpec {
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub content: BTreeMap<String, MediaTypeSpec>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub content: BTreeMap<String, MediaTypeSpec>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaTypeSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<Value>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub examples: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MockResponse {
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractCheck {
    #[serde(default)]
    pub checked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_endpoint: Option<String>,
    /// True when some relevant contract assertions could not be evaluated safely.
    ///
    /// This defaults to false so sessions written before partial validation existed remain
    /// readable. Inconclusive checks also carry an explanatory finding for older UIs.
    #[serde(default)]
    pub inconclusive: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub violations: Vec<ContractViolation>,
}

impl ContractCheck {
    pub fn is_valid(&self) -> bool {
        self.checked && !self.inconclusive && self.violations.is_empty()
    }

    pub fn violation_count(&self) -> usize {
        self.violations.len()
    }

    fn is_definitively_invalid(&self) -> bool {
        self.violations.iter().any(|violation| {
            !matches!(
                violation.code.as_str(),
                "validation_inconclusive" | "validation_budget_exceeded"
            )
        })
    }

    fn finding_budget_exhausted(&self) -> bool {
        self.violations.iter().any(|violation| {
            violation.code == "validation_budget_exceeded" && violation.message.contains("findings")
        })
    }

    fn push_violation(&mut self, violation: ContractViolation) {
        if self.violations.len() < MAX_CONTRACT_FINDINGS.saturating_sub(1) {
            self.violations.push(violation);
            return;
        }

        self.mark_budget_exhausted();
    }

    fn mark_inconclusive(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.inconclusive = true;
        let location = location.into();
        let message = message.into();
        if self.violations.iter().any(|violation| {
            violation.code == "validation_inconclusive"
                && violation.location == location
                && violation.message == message
        }) {
            return;
        }
        self.push_violation(ContractViolation::new(
            "validation_inconclusive",
            location,
            message,
        ));
    }

    fn mark_budget_exhausted(&mut self) {
        self.inconclusive = true;
        if self
            .violations
            .iter()
            .any(|violation| violation.code == "validation_budget_exceeded")
        {
            return;
        }
        if self.violations.len() >= MAX_CONTRACT_FINDINGS {
            self.violations.pop();
        }
        self.violations.push(ContractViolation::new(
            "validation_budget_exceeded",
            "contract",
            format!(
                "validation stopped after {MAX_CONTRACT_FINDINGS} findings; this result is partial"
            ),
        ));
    }
}

struct ValidationBudget {
    remaining: usize,
}

impl ValidationBudget {
    fn new() -> Self {
        Self {
            remaining: MAX_SCHEMA_VALIDATION_STEPS,
        }
    }

    fn consume(&mut self, check: &mut ContractCheck, location: &str) -> bool {
        if let Some(remaining) = self.remaining.checked_sub(1) {
            self.remaining = remaining;
            true
        } else {
            self.note_exhausted(check, location);
            false
        }
    }

    fn is_exhausted(&self) -> bool {
        self.remaining == 0
    }

    fn note_exhausted(&self, check: &mut ContractCheck, location: &str) {
        check.inconclusive = true;
        if check.violations.iter().any(|violation| {
            violation.code == "validation_budget_exceeded"
                && violation.message.contains("schema traversal")
        }) {
            return;
        }
        if check.violations.len() >= MAX_CONTRACT_FINDINGS {
            check.violations.pop();
        }
        check.violations.push(ContractViolation::new(
            "validation_budget_exceeded",
            location,
            format!(
                "schema traversal stopped after {MAX_SCHEMA_VALIDATION_STEPS} steps; this result is partial"
            ),
        ));
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractViolation {
    pub code: String,
    pub location: String,
    pub message: String,
}

impl ContractViolation {
    pub fn new(
        code: impl Into<String>,
        location: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            location: location.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct HeaderValue {
    pub name: String,
    pub value: String,
}

impl HeaderValue {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExchangePart {
    pub headers: BTreeMap<String, String>,
    #[serde(
        default,
        rename = "headerValues",
        alias = "header_values",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub header_values: Vec<HeaderValue>,
    pub body: String,
    #[serde(default)]
    pub size: usize,
    #[serde(default)]
    pub truncated: bool,
}

impl ExchangePart {
    /// Iterates over headers exactly as captured, falling back to the legacy flattened map.
    pub fn iter_headers(&self) -> impl Iterator<Item = (&str, &str)> {
        let use_legacy_headers = self.header_values.is_empty();
        self.header_values
            .iter()
            .map(|header| (header.name.as_str(), header.value.as_str()))
            .chain(
                self.headers
                    .iter()
                    .filter(move |_| use_legacy_headers)
                    .map(|(name, value)| (name.as_str(), value.as_str())),
            )
    }

    pub fn header_value(&self, wanted: &str) -> Option<&str> {
        self.iter_headers()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value)
    }

    pub fn header_count(&self) -> usize {
        if self.header_values.is_empty() {
            self.headers.len()
        } else {
            self.header_values.len()
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub method: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    pub status: u16,
    pub timestamp: String,
    pub request: ExchangePart,
    pub response: ExchangePart,
    pub latency_ms: u128,
    #[serde(default)]
    pub contract: ContractCheck,
}

impl LogEntry {
    pub fn validate_against(&mut self, endpoints: &[Endpoint]) -> &ContractCheck {
        self.contract = validate_exchange(endpoints, self);
        &self.contract
    }
}

pub fn validate_exchange(endpoints: &[Endpoint], entry: &LogEntry) -> ContractCheck {
    endpoints
        .iter()
        .find(|endpoint| endpoint.matches(&entry.method, &entry.path))
        .map_or_else(
            || ContractCheck {
                checked: true,
                matched_endpoint: None,
                violations: vec![ContractViolation::new(
                    "undocumented_operation",
                    "request.operation",
                    format!("{} {} is not documented", entry.method, entry.path),
                )],
                ..ContractCheck::default()
            },
            |endpoint| endpoint.validate_exchange(entry),
        )
}

fn split_path(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect()
}

fn path_parameter_value<'a>(template: &str, path: &'a str, name: &str) -> Option<&'a str> {
    let wanted = format!("{{{name}}}");
    split_path(template)
        .into_iter()
        .zip(split_path(path))
        .find_map(|(expected, actual)| (expected == wanted).then_some(actual))
}

fn cookie_value<'a>(part: &'a ExchangePart, wanted: &str) -> Option<&'a str> {
    part.header_value("cookie")?.split(';').find_map(|cookie| {
        let (name, value) = cookie.trim().split_once('=')?;
        name.trim().eq(wanted).then_some(value.trim())
    })
}

fn parameter_values(
    parameter: &ApiParameter,
    template: &str,
    entry: &LogEntry,
    query: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    match parameter.location.to_ascii_lowercase().as_str() {
        "path" => path_parameter_value(template, &entry.path, &parameter.name)
            .map(|value| vec![value.to_string()])
            .unwrap_or_default(),
        "query" => query.get(&parameter.name).cloned().unwrap_or_default(),
        "header" => entry
            .request
            .header_value(&parameter.name)
            .map(|value| vec![value.to_string()])
            .unwrap_or_default(),
        "cookie" => cookie_value(&entry.request, &parameter.name)
            .map(|value| vec![value.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn parameter_json_value(values: &[String], schema: &Value) -> Result<Value, String> {
    let schema_object = schema.as_object();
    let is_array = schema_object.is_some_and(|schema| {
        schema_types(schema).contains(&"array") || schema.contains_key("items")
    });
    if is_array {
        let item_schema = schema_object.and_then(|schema| schema.get("items"));
        let items = values
            .iter()
            .flat_map(|value| value.split(','))
            .map(|value| {
                item_schema.map_or_else(
                    || Ok(Value::String(value.to_string())),
                    |schema| coerce_parameter_scalar(value, schema),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Value::Array(items));
    }

    let value = values.first().map_or("", String::as_str);
    coerce_parameter_scalar(value, schema)
}

fn schema_has_unusable_type(schema: &Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    let Some(value) = schema.get("type") else {
        return false;
    };
    match value {
        Value::String(expected) => !is_supported_json_type(expected),
        Value::Array(expected) => {
            expected.len() > 64
                || expected.iter().any(|expected| {
                    expected
                        .as_str()
                        .is_none_or(|expected| !is_supported_json_type(expected))
                })
        }
        _ => true,
    }
}

fn unsupported_parameter_serialization(parameter: &ApiParameter, schema: &Value) -> Option<String> {
    let location = parameter.location.to_ascii_lowercase();
    let default_style = match location.as_str() {
        "path" | "header" => "simple",
        "query" | "cookie" => "form",
        _ => {
            return Some(format!(
                "parameter location `{}` is not supported",
                parameter.location
            ));
        }
    };
    let style = parameter.style.as_deref().unwrap_or(default_style);
    if style != default_style {
        return Some(format!(
            "parameter serialization style `{style}` is not supported for {location} parameters"
        ));
    }

    let schema = schema.as_object()?;
    if schema
        .get("type")
        .and_then(Value::as_array)
        .is_some_and(|types| types.len() > 64)
    {
        return Some("parameter schema declares too many serialization types".into());
    }
    let types = schema_types(schema);
    if types.contains(&"object") {
        return Some("object-valued parameter serialization is not supported".into());
    }
    let is_array = types.contains(&"array") || schema.contains_key("items");
    let explode = parameter.explode.unwrap_or(style == "form");
    if is_array && location == "cookie" && explode {
        return Some("exploded cookie array serialization cannot be validated faithfully".into());
    }
    None
}

fn coerce_parameter_scalar(value: &str, schema: &Value) -> Result<Value, String> {
    let Some(schema) = schema.as_object() else {
        return Ok(Value::String(value.to_string()));
    };
    if schema
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|values| values.contains(&Value::String(value.to_string())))
    {
        return Ok(Value::String(value.to_string()));
    }

    let types = schema_types(schema);
    if types.is_empty() {
        return Ok(Value::String(value.to_string()));
    }
    for expected in types
        .iter()
        .copied()
        .filter(|expected| *expected != "string")
        .chain(
            types
                .iter()
                .copied()
                .filter(|expected| *expected == "string"),
        )
    {
        let parsed = match expected {
            "integer" => value
                .parse::<i64>()
                .map(Value::from)
                .or_else(|_| value.parse::<u64>().map(Value::from))
                .ok(),
            "number" => serde_json::from_str::<Number>(value)
                .ok()
                .map(Value::Number),
            "boolean" => match value {
                "true" => Some(Value::Bool(true)),
                "false" => Some(Value::Bool(false)),
                _ => None,
            },
            "null" if value == "null" => Some(Value::Null),
            "string" => Some(Value::String(value.to_string())),
            _ => None,
        };
        if let Some(parsed) = parsed {
            return Ok(parsed);
        }
    }

    Err(types.join(" or "))
}

#[derive(Clone, Copy)]
enum SchemaDirection {
    Neutral,
    Request,
    Response,
}

fn validate_content(
    part: &ExchangePart,
    documented: &BTreeMap<String, MediaTypeSpec>,
    side: &str,
    check: &mut ContractCheck,
    budget: &mut ValidationBudget,
) {
    let actual_content_type = part
        .header_value("content-type")
        .map(normalize_content_type)
        .filter(|value| !value.is_empty());
    let media = actual_content_type
        .as_deref()
        .and_then(|actual| find_media(documented, actual));

    if media.is_none() {
        let message = match actual_content_type.as_deref() {
            Some(actual) => format!("content type `{actual}` is not documented for this {side}"),
            None => format!("{side} body has no Content-Type header"),
        };
        check.push_violation(ContractViolation::new(
            format!("undocumented_{side}_content_type"),
            format!("{side}.headers.content-type"),
            message,
        ));
        return;
    }

    if part.truncated {
        check.mark_inconclusive(
            format!("{side}.body"),
            format!("{side} body validation was skipped because the capture is truncated"),
        );
        return;
    }
    if has_non_identity_content_encoding(part) {
        check.mark_inconclusive(
            format!("{side}.body"),
            format!(
                "{side} body validation was skipped because the captured body is content-encoded"
            ),
        );
        return;
    }
    if part.body.contains('\u{fffd}') {
        check.mark_inconclusive(
            format!("{side}.body"),
            format!("{side} body validation was skipped because the capture contains lossy UTF-8"),
        );
        return;
    }
    let (documented_content_type, media) = media.expect("checked above");
    let is_json = actual_content_type
        .as_deref()
        .is_some_and(is_json_content_type)
        || is_json_content_type(documented_content_type);
    if !is_json {
        if media.schema.is_some() {
            check.mark_inconclusive(
                format!("{side}.body"),
                format!(
                    "schema validation for non-JSON media type `{documented_content_type}` is not supported"
                ),
            );
        }
        return;
    }

    let direction = if side == "request" {
        SchemaDirection::Request
    } else {
        SchemaDirection::Response
    };
    match serde_json::from_str::<Value>(&part.body) {
        Ok(value) => {
            if let Some(schema) = &media.schema {
                validate_json_schema(
                    &value,
                    schema,
                    &format!("{side}.body"),
                    direction,
                    check,
                    budget,
                    0,
                );
            }
        }
        Err(error) => check.push_violation(ContractViolation::new(
            "invalid_json",
            format!("{side}.body"),
            format!("body is not valid JSON: {error}"),
        )),
    }
}

fn has_non_identity_content_encoding(part: &ExchangePart) -> bool {
    part.header_value("content-encoding")
        .is_some_and(|encoding| {
            encoding
                .split(',')
                .map(str::trim)
                .filter(|encoding| !encoding.is_empty())
                .any(|encoding| !encoding.eq_ignore_ascii_case("identity"))
        })
}

fn normalize_content_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn find_media<'a>(
    documented: &'a BTreeMap<String, MediaTypeSpec>,
    actual: &str,
) -> Option<(&'a String, &'a MediaTypeSpec)> {
    documented
        .iter()
        .filter_map(|(media_type, media)| {
            media_type_specificity(media_type, actual)
                .map(|specificity| (specificity, media_type, media))
        })
        .max_by_key(|(specificity, _, _)| *specificity)
        .map(|(_, media_type, media)| (media_type, media))
}

fn media_type_specificity(documented: &str, actual: &str) -> Option<u8> {
    let documented = normalize_content_type(documented);
    let actual = normalize_content_type(actual);
    if documented == actual {
        return Some(4);
    }
    if documented == "*/*" {
        return Some(0);
    }

    let (documented_type, documented_subtype) = documented.split_once('/')?;
    let (actual_type, actual_subtype) = actual.split_once('/')?;
    if documented_type != "*" && documented_type != actual_type {
        return None;
    }
    if documented_subtype == actual_subtype {
        return Some(2);
    }
    if documented_subtype == "*" {
        return Some(u8::from(documented_type != "*"));
    }
    documented_subtype.strip_prefix("*+").and_then(|suffix| {
        actual_subtype
            .ends_with(&format!("+{suffix}"))
            .then_some(if documented_type == "*" { 2 } else { 3 })
    })
}

fn is_json_content_type(content_type: &str) -> bool {
    let content_type = normalize_content_type(content_type);
    content_type == "application/json" || content_type.ends_with("+json")
}

fn is_textual_content_type(content_type: &str) -> bool {
    let content_type = normalize_content_type(content_type);
    content_type.starts_with("text/")
        || content_type == "application/xml"
        || content_type.ends_with("+xml")
}

fn validate_json_schema(
    value: &Value,
    schema: &Value,
    location: &str,
    direction: SchemaDirection,
    check: &mut ContractCheck,
    budget: &mut ValidationBudget,
    depth: usize,
) {
    if !budget.consume(check, location) {
        return;
    }
    if depth >= MAX_SCHEMA_VALIDATION_DEPTH {
        check.mark_inconclusive(
            location,
            format!(
                "schema validation exceeded the supported depth of {MAX_SCHEMA_VALIDATION_DEPTH}"
            ),
        );
        return;
    }
    if let Some(allowed) = schema.as_bool() {
        if !allowed {
            check.push_violation(ContractViolation::new(
                "schema_type_mismatch",
                location,
                "value is rejected by the documented schema",
            ));
        }
        return;
    }
    let Some(schema) = schema.as_object() else {
        check.mark_inconclusive(location, "documented schema is malformed");
        return;
    };

    mark_unsupported_schema_keywords(schema, location, check, budget);
    mark_malformed_schema_keywords(schema, location, check, budget);
    if check.finding_budget_exhausted() {
        return;
    }
    if budget.is_exhausted() {
        budget.note_exhausted(check, location);
        return;
    }

    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for branch in all_of {
            if check.finding_budget_exhausted() {
                break;
            }
            if budget.is_exhausted() {
                budget.note_exhausted(check, location);
                break;
            }
            validate_json_schema(value, branch, location, direction, check, budget, depth + 1);
        }
    }
    if check.finding_budget_exhausted() {
        return;
    }
    if budget.is_exhausted() {
        budget.note_exhausted(check, location);
        return;
    }
    if let Some(branches) = schema.get("anyOf").and_then(Value::as_array) {
        let mut results = Vec::new();
        for branch in branches {
            if budget.is_exhausted() {
                budget.note_exhausted(check, location);
                break;
            }
            let result =
                schema_branch_result(value, branch, location, direction, budget, depth + 1);
            let matched = result.is_valid();
            results.push(result);
            if matched {
                break;
            }
        }
        let evaluated_all = results.len() == branches.len();
        if budget.is_exhausted() && results.iter().any(|result| result.inconclusive) {
            budget.note_exhausted(check, location);
        }
        if budget.is_exhausted() && !evaluated_all && !results.iter().any(ContractCheck::is_valid) {
            budget.note_exhausted(check, location);
            return;
        }
        if !results.iter().any(ContractCheck::is_valid) {
            if results
                .iter()
                .any(|result| !result.is_definitively_invalid())
            {
                check.mark_inconclusive(
                    location,
                    "could not determine whether the value matches an `anyOf` schema",
                );
            } else {
                check.push_violation(ContractViolation::new(
                    "schema_composition_mismatch",
                    location,
                    "value does not match any `anyOf` schema",
                ));
            }
            return;
        }
    }
    if budget.is_exhausted() {
        budget.note_exhausted(check, location);
        return;
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let mut results = Vec::new();
        for branch in branches {
            if budget.is_exhausted() {
                budget.note_exhausted(check, location);
                break;
            }
            let result =
                schema_branch_result(value, branch, location, direction, budget, depth + 1);
            results.push(result);
            if results
                .iter()
                .filter(|result| ContractCheck::is_valid(result))
                .count()
                > 1
            {
                break;
            }
        }
        let matches = results
            .iter()
            .filter(|result| ContractCheck::is_valid(result))
            .count();
        let could_match = results
            .iter()
            .any(|result| result.inconclusive && !result.is_definitively_invalid());
        let evaluated_all = results.len() == branches.len();
        if budget.is_exhausted() && results.iter().any(|result| result.inconclusive) {
            budget.note_exhausted(check, location);
        }
        if budget.is_exhausted() && !evaluated_all && matches <= 1 {
            budget.note_exhausted(check, location);
            return;
        }
        if could_match && matches <= 1 {
            check.mark_inconclusive(
                location,
                "could not determine whether exactly one `oneOf` schema matches",
            );
            return;
        }
        if matches != 1 {
            check.push_violation(ContractViolation::new(
                "schema_composition_mismatch",
                location,
                format!("value must match exactly one `oneOf` schema; matched {matches}"),
            ));
            return;
        }
    }
    if budget.is_exhausted() {
        budget.note_exhausted(check, location);
        return;
    }
    if let Some(branch) = schema.get("not") {
        let result = schema_branch_result(value, branch, location, direction, budget, depth + 1);
        if budget.is_exhausted() && result.inconclusive {
            budget.note_exhausted(check, location);
        }
        if result.inconclusive && !result.is_definitively_invalid() {
            check.mark_inconclusive(
                location,
                "could not determine whether the value is rejected by the `not` schema",
            );
        } else if result.is_valid() {
            check.push_violation(ContractViolation::new(
                "schema_composition_mismatch",
                location,
                "value matches the disallowed `not` schema",
            ));
        }
    }

    let (expected_types, types_complete) = validation_schema_types(schema, location, check, budget);
    if !types_complete {
        return;
    }
    let has_unsupported_type = expected_types
        .iter()
        .any(|expected| !is_supported_json_type(expected));
    if has_unsupported_type {
        check.mark_inconclusive(
            location,
            "schema declares a type that this validator does not support",
        );
    }
    let nullable_null = value.is_null()
        && schema.get("nullable").and_then(Value::as_bool) == Some(true)
        && schema.contains_key("type");
    if !has_unsupported_type
        && !nullable_null
        && !expected_types.is_empty()
        && !expected_types
            .iter()
            .filter(|expected| is_supported_json_type(expected))
            .any(|expected| json_type_matches(value, expected))
    {
        check.push_violation(ContractViolation::new(
            "schema_type_mismatch",
            location,
            format!(
                "expected {}, found {}",
                expected_types.join(" or "),
                json_type_name(value)
            ),
        ));
        return;
    }

    if let Some(constant) = schema.get("const")
        && constant != value
    {
        check.push_violation(ContractViolation::new(
            "schema_value_mismatch",
            location,
            "value does not match the documented constant",
        ));
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        check.push_violation(ContractViolation::new(
            "schema_value_mismatch",
            location,
            "value is not one of the documented enum values",
        ));
    }

    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        validate_size_bounds(
            object.len(),
            schema,
            "minProperties",
            "maxProperties",
            "object",
            location,
            check,
        );
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for required_name in required {
                if check.finding_budget_exhausted() || !budget.consume(check, location) {
                    break;
                }
                let Some(name) = required_name.as_str() else {
                    check.mark_inconclusive(location, "schema keyword `required` is malformed");
                    continue;
                };
                if required_property_applies(properties, name, direction)
                    && !object.contains_key(name)
                {
                    check.push_violation(ContractViolation::new(
                        "missing_required_property",
                        format!("{location}.{name}"),
                        format!("required property `{name}` is missing"),
                    ));
                }
            }
        }
        if let Some(properties) = properties {
            for (name, property_schema) in properties {
                if check.finding_budget_exhausted() || !budget.consume(check, location) {
                    break;
                }
                if !property_schema.is_object() && !property_schema.is_boolean() {
                    check.mark_inconclusive(
                        format!("{location}.{name}"),
                        "property schema is malformed",
                    );
                    continue;
                }
                if let Some(property) = object.get(name) {
                    validate_json_schema(
                        property,
                        property_schema,
                        &format!("{location}.{name}"),
                        direction,
                        check,
                        budget,
                        depth + 1,
                    );
                }
            }
        }

        if let Some(additional) = schema.get("additionalProperties") {
            for (name, property) in object {
                if check.finding_budget_exhausted() || !budget.consume(check, location) {
                    break;
                }
                if properties.is_some_and(|properties| properties.contains_key(name)) {
                    continue;
                }
                match additional {
                    Value::Bool(false) => check.push_violation(ContractViolation::new(
                        "additional_property_not_allowed",
                        format!("{location}.{name}"),
                        format!("additional property `{name}` is not allowed"),
                    )),
                    Value::Object(_) | Value::Bool(true) => {
                        if !additional.is_boolean() {
                            validate_json_schema(
                                property,
                                additional,
                                &format!("{location}.{name}"),
                                direction,
                                check,
                                budget,
                                depth + 1,
                            );
                        }
                    }
                    _ => check.mark_inconclusive(
                        location,
                        "`additionalProperties` has an unsupported value",
                    ),
                }
            }
        }
    }

    if let Some(array) = value.as_array() {
        validate_size_bounds(
            array.len(),
            schema,
            "minItems",
            "maxItems",
            "array",
            location,
            check,
        );
        if schema.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
            if array.len() > MAX_UNIQUE_ITEMS {
                check.mark_inconclusive(
                    location,
                    format!(
                        "`uniqueItems` validation is limited to {MAX_UNIQUE_ITEMS} array items"
                    ),
                );
            } else {
                let mut seen = HashSet::with_capacity(array.len());
                for (index, item) in array.iter().enumerate() {
                    if check.finding_budget_exhausted() || !budget.consume(check, location) {
                        break;
                    }
                    if !seen.insert(item) {
                        check.push_violation(ContractViolation::new(
                            "schema_value_mismatch",
                            format!("{location}[{index}]"),
                            "array items must be unique",
                        ));
                    }
                }
            }
        }
        if let Some(items) = schema.get("items") {
            for (index, item) in array.iter().enumerate() {
                if check.finding_budget_exhausted() {
                    break;
                }
                if budget.is_exhausted() {
                    budget.note_exhausted(check, location);
                    break;
                }
                validate_json_schema(
                    item,
                    items,
                    &format!("{location}[{index}]"),
                    direction,
                    check,
                    budget,
                    depth + 1,
                );
            }
        }
    }

    if let Some(string) = value.as_str() {
        validate_size_bounds(
            string.chars().count(),
            schema,
            "minLength",
            "maxLength",
            "string",
            location,
            check,
        );
    }

    if let Some(number) = value.as_number() {
        validate_numeric_bounds(number, schema, location, check);
    }
}

fn schema_branch_result(
    value: &Value,
    schema: &Value,
    location: &str,
    direction: SchemaDirection,
    budget: &mut ValidationBudget,
    depth: usize,
) -> ContractCheck {
    let mut result = ContractCheck {
        checked: true,
        ..ContractCheck::default()
    };
    validate_json_schema(
        value,
        schema,
        location,
        direction,
        &mut result,
        budget,
        depth,
    );
    result
}

fn mark_unsupported_schema_keywords(
    schema: &Map<String, Value>,
    location: &str,
    check: &mut ContractCheck,
    budget: &mut ValidationBudget,
) {
    for keyword in schema.keys() {
        if !budget.consume(check, location) {
            break;
        }
        let supported_or_annotation = matches!(
            keyword.as_str(),
            "$schema"
                | "$id"
                | "$anchor"
                | "$defs"
                | "definitions"
                | "title"
                | "description"
                | "default"
                | "example"
                | "examples"
                | "deprecated"
                | "readOnly"
                | "writeOnly"
                | "xml"
                | "externalDocs"
                | "discriminator"
                | "type"
                | "nullable"
                | "enum"
                | "const"
                | "allOf"
                | "anyOf"
                | "oneOf"
                | "not"
                | "properties"
                | "required"
                | "additionalProperties"
                | "minProperties"
                | "maxProperties"
                | "items"
                | "minItems"
                | "maxItems"
                | "uniqueItems"
                | "minLength"
                | "maxLength"
                | "minimum"
                | "maximum"
                | "exclusiveMinimum"
                | "exclusiveMaximum"
                | "multipleOf"
        ) || keyword.starts_with("x-");
        if !supported_or_annotation {
            let explanation = if keyword == "$ref" {
                "an unresolved recursive `$ref` cannot be evaluated completely".to_string()
            } else {
                format!("schema keyword `{keyword}` is not supported by this validator")
            };
            check.mark_inconclusive(location, explanation);
        }
    }
}

fn mark_malformed_schema_keywords(
    schema: &Map<String, Value>,
    location: &str,
    check: &mut ContractCheck,
    budget: &mut ValidationBudget,
) {
    if budget.is_exhausted() {
        budget.note_exhausted(check, location);
        return;
    }
    if let Some(value) = schema.get("type")
        && !matches!(value, Value::String(_))
        && !value.as_array().is_some_and(|types| !types.is_empty())
    {
        check.mark_inconclusive(location, "schema keyword `type` is malformed");
    }
    for keyword in ["allOf", "anyOf", "oneOf", "enum", "required"] {
        if schema.get(keyword).is_some_and(|value| !value.is_array()) {
            check.mark_inconclusive(location, format!("schema keyword `{keyword}` is malformed"));
        }
    }
    if schema
        .get("not")
        .is_some_and(|branch| !branch.is_object() && !branch.is_boolean())
    {
        check.mark_inconclusive(location, "schema keyword `not` is malformed");
    }
    if schema
        .get("properties")
        .is_some_and(|value| !value.is_object())
    {
        check.mark_inconclusive(location, "schema keyword `properties` is malformed");
    }
    if schema
        .get("items")
        .is_some_and(|value| !value.is_object() && !value.is_boolean())
    {
        check.mark_inconclusive(location, "schema keyword `items` is malformed");
    }
    if schema
        .get("additionalProperties")
        .is_some_and(|value| !value.is_object() && !value.is_boolean())
    {
        check.mark_inconclusive(
            location,
            "schema keyword `additionalProperties` is malformed",
        );
    }
    for keyword in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        if schema
            .get(keyword)
            .is_some_and(|value| value.as_u64().is_none())
        {
            check.mark_inconclusive(location, format!("schema keyword `{keyword}` is malformed"));
        }
    }
    for keyword in ["minimum", "maximum", "multipleOf"] {
        if schema.get(keyword).is_some_and(|value| !value.is_number()) {
            check.mark_inconclusive(location, format!("schema keyword `{keyword}` is malformed"));
        }
    }
    for keyword in ["exclusiveMinimum", "exclusiveMaximum"] {
        if schema
            .get(keyword)
            .is_some_and(|value| !value.is_number() && value.as_bool().is_none())
        {
            check.mark_inconclusive(location, format!("schema keyword `{keyword}` is malformed"));
        }
    }
    for keyword in ["nullable", "uniqueItems"] {
        if schema.get(keyword).is_some_and(|value| !value.is_boolean()) {
            check.mark_inconclusive(location, format!("schema keyword `{keyword}` is malformed"));
        }
    }
}

fn validate_size_bounds(
    actual: usize,
    schema: &Map<String, Value>,
    minimum_keyword: &str,
    maximum_keyword: &str,
    kind: &str,
    location: &str,
    check: &mut ContractCheck,
) {
    if let Some(minimum) = schema.get(minimum_keyword).and_then(Value::as_u64)
        && u64::try_from(actual).is_ok_and(|actual| actual < minimum)
    {
        check.push_violation(ContractViolation::new(
            "schema_size_mismatch",
            location,
            format!("{kind} must contain at least {minimum} item(s)"),
        ));
    }
    if let Some(maximum) = schema.get(maximum_keyword).and_then(Value::as_u64)
        && u64::try_from(actual).is_ok_and(|actual| actual > maximum)
    {
        check.push_violation(ContractViolation::new(
            "schema_size_mismatch",
            location,
            format!("{kind} must contain at most {maximum} item(s)"),
        ));
    }
}

fn validate_numeric_bounds(
    actual: &Number,
    schema: &Map<String, Value>,
    location: &str,
    check: &mut ContractCheck,
) {
    let minimum = schema.get("minimum").and_then(Value::as_number);
    let maximum = schema.get("maximum").and_then(Value::as_number);
    let exclusive_minimum = schema.get("exclusiveMinimum");
    let exclusive_maximum = schema.get("exclusiveMaximum");

    let below_minimum = minimum.and_then(|minimum| {
        compare_json_numbers(actual, minimum).map(|ordering| {
            ordering == Ordering::Less
                || (exclusive_minimum.and_then(Value::as_bool) == Some(true)
                    && ordering == Ordering::Equal)
        })
    });
    let below_exclusive = exclusive_minimum
        .and_then(Value::as_number)
        .and_then(|minimum| {
            compare_json_numbers(actual, minimum).map(|ordering| ordering != Ordering::Greater)
        });
    if (minimum.is_some() && below_minimum.is_none())
        || (exclusive_minimum.is_some_and(Value::is_number) && below_exclusive.is_none())
    {
        check.mark_inconclusive(
            location,
            "numeric minimum cannot be compared exactly without losing integer precision",
        );
    } else if below_minimum == Some(true) || below_exclusive == Some(true) {
        check.push_violation(ContractViolation::new(
            "schema_range_mismatch",
            location,
            "number is below the documented minimum",
        ));
    }
    let above_maximum = maximum.and_then(|maximum| {
        compare_json_numbers(actual, maximum).map(|ordering| {
            ordering == Ordering::Greater
                || (exclusive_maximum.and_then(Value::as_bool) == Some(true)
                    && ordering == Ordering::Equal)
        })
    });
    let above_exclusive = exclusive_maximum
        .and_then(Value::as_number)
        .and_then(|maximum| {
            compare_json_numbers(actual, maximum).map(|ordering| ordering != Ordering::Less)
        });
    if (maximum.is_some() && above_maximum.is_none())
        || (exclusive_maximum.is_some_and(Value::is_number) && above_exclusive.is_none())
    {
        check.mark_inconclusive(
            location,
            "numeric maximum cannot be compared exactly without losing integer precision",
        );
    } else if above_maximum == Some(true) || above_exclusive == Some(true) {
        check.push_violation(ContractViolation::new(
            "schema_range_mismatch",
            location,
            "number is above the documented maximum",
        ));
    }
    if let Some(multiple) = schema.get("multipleOf").and_then(Value::as_number) {
        if let (Some(actual), Some(multiple)) = (exact_integer(actual), exact_integer(multiple)) {
            if multiple <= 0 {
                check.mark_inconclusive(location, "schema has an invalid `multipleOf` value");
            } else if actual % multiple != 0 {
                check.push_violation(ContractViolation::new(
                    "schema_range_mismatch",
                    location,
                    format!("number must be a multiple of {multiple}"),
                ));
            }
        } else if let (Some(actual), Some(multiple)) = (safe_f64(actual), safe_f64(multiple)) {
            if multiple <= 0.0 || !multiple.is_finite() {
                check.mark_inconclusive(location, "schema has an invalid `multipleOf` value");
            } else {
                let quotient = actual / multiple;
                let tolerance = f64::EPSILON * quotient.abs().max(1.0) * 8.0;
                if (quotient - quotient.round()).abs() > tolerance {
                    check.push_violation(ContractViolation::new(
                        "schema_range_mismatch",
                        location,
                        format!("number must be a multiple of {multiple}"),
                    ));
                }
            }
        } else {
            check.mark_inconclusive(
                location,
                "`multipleOf` cannot be evaluated without losing integer precision",
            );
        }
    }
}

fn compare_json_numbers(left: &Number, right: &Number) -> Option<Ordering> {
    if let (Some(left), Some(right)) = (exact_integer(left), exact_integer(right)) {
        return Some(left.cmp(&right));
    }
    safe_f64(left)?.partial_cmp(&safe_f64(right)?)
}

fn exact_integer(number: &Number) -> Option<i128> {
    number
        .as_i64()
        .map(i128::from)
        .or_else(|| number.as_u64().map(i128::from))
}

fn safe_f64(number: &Number) -> Option<f64> {
    const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_992;
    if exact_integer(number).is_some_and(|integer| integer.abs() > MAX_SAFE_INTEGER) {
        return None;
    }
    number.as_f64().filter(|number| number.is_finite())
}

fn required_property_applies(
    properties: Option<&Map<String, Value>>,
    name: &str,
    direction: SchemaDirection,
) -> bool {
    let property = properties.and_then(|properties| properties.get(name));
    match direction {
        SchemaDirection::Request => {
            property
                .and_then(|property| property.get("readOnly"))
                .and_then(Value::as_bool)
                != Some(true)
        }
        SchemaDirection::Response => {
            property
                .and_then(|property| property.get("writeOnly"))
                .and_then(Value::as_bool)
                != Some(true)
        }
        SchemaDirection::Neutral => true,
    }
}

fn schema_types(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(value)) => vec![value.as_str()],
        Some(Value::Array(values)) => values.iter().filter_map(Value::as_str).collect(),
        _ if schema.contains_key("properties") || schema.contains_key("required") => vec!["object"],
        _ if schema.contains_key("items") => vec!["array"],
        _ => Vec::new(),
    }
}

fn validation_schema_types<'a>(
    schema: &'a Map<String, Value>,
    location: &str,
    check: &mut ContractCheck,
    budget: &mut ValidationBudget,
) -> (Vec<&'a str>, bool) {
    match schema.get("type") {
        Some(Value::String(value)) => {
            let complete = budget.consume(check, location);
            (
                complete.then_some(value.as_str()).into_iter().collect(),
                complete,
            )
        }
        Some(Value::Array(values)) => {
            let mut types = Vec::with_capacity(values.len().min(8));
            let mut seen = HashSet::new();
            for value in values {
                if !budget.consume(check, location) {
                    return (types, false);
                }
                let Some(value) = value.as_str() else {
                    check.mark_inconclusive(location, "schema keyword `type` is malformed");
                    continue;
                };
                if !seen.insert(value) {
                    check.mark_inconclusive(location, "schema keyword `type` is malformed");
                }
                types.push(value);
            }
            (types, true)
        }
        _ if schema.contains_key("properties") || schema.contains_key("required") => {
            (vec!["object"], true)
        }
        _ if schema.contains_key("items") => (vec!["array"], true),
        _ => (Vec::new(), true),
    }
}

fn is_supported_json_type(expected: &str) -> bool {
    matches!(
        expected,
        "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
    )
}

fn json_type_matches(value: &Value, expected: &str) -> bool {
    match expected {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number.fract() == 0.0),
        "string" => value.is_string(),
        _ => true,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn preferred_media(content: &BTreeMap<String, MediaTypeSpec>) -> Option<(&String, &MediaTypeSpec)> {
    content
        .get_key_value("application/json")
        .or_else(|| {
            content
                .iter()
                .find(|(content_type, _)| is_json_content_type(content_type))
        })
        .or_else(|| content.iter().next())
}

fn mock_value_from_schema(
    schema: &Value,
    direction: SchemaDirection,
    depth: usize,
) -> Option<Value> {
    if depth >= MAX_MOCK_SCHEMA_DEPTH {
        return None;
    }
    let schema = schema.as_object()?;
    for keyword in ["example", "default", "const"] {
        if let Some(value) = schema.get(keyword) {
            return Some(filter_schema_derived_mock(
                value.clone(),
                schema,
                direction,
                depth,
            ));
        }
    }
    if let Some(value) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return Some(filter_schema_derived_mock(
            value.clone(),
            schema,
            direction,
            depth,
        ));
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(value) = schema
            .get(keyword)
            .and_then(Value::as_array)
            .and_then(|schemas| {
                schemas
                    .iter()
                    .find_map(|schema| mock_value_from_schema(schema, direction, depth + 1))
            })
        {
            return Some(value);
        }
    }
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        let mut object = Map::new();
        for value in all_of
            .iter()
            .filter_map(|schema| mock_value_from_schema(schema, direction, depth + 1))
        {
            if let Value::Object(properties) = value {
                object.extend(properties);
            }
        }
        if !object.is_empty() {
            return Some(Value::Object(object));
        }
    }

    let schema_type = schema
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| schema.contains_key("properties").then_some("object"))
        .or_else(|| schema.contains_key("items").then_some("array"));
    match schema_type {
        Some("object") => mock_object(schema, direction, depth),
        Some("array") => mock_array(schema, direction, depth),
        Some("string") => mock_string(schema).map(Value::String),
        Some("integer") => mock_number(schema, true),
        Some("number") => mock_number(schema, false),
        Some("boolean") => Some(Value::Bool(false)),
        Some("null") => Some(Value::Null),
        _ => None,
    }
}

fn mock_object(
    schema: &Map<String, Value>,
    direction: SchemaDirection,
    depth: usize,
) -> Option<Value> {
    let properties = schema.get("properties").and_then(Value::as_object)?;
    let minimum = schema
        .get("minProperties")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let maximum = schema
        .get("maxProperties")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX);
    if minimum > maximum || minimum > MAX_MOCK_COLLECTION_ITEMS {
        return None;
    }

    let mut names = Vec::new();
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            if required_property_applies(Some(properties), name, direction) {
                let property = properties.get(name)?;
                if !names.contains(&name) {
                    names.push(name);
                }
                if !property_applies_to_mock(property, direction) {
                    return None;
                }
            }
        }
    }
    if names.len() > maximum || names.len() > MAX_MOCK_COLLECTION_ITEMS {
        return None;
    }
    for (name, property) in properties {
        if names.len() >= maximum.min(MAX_MOCK_COLLECTION_ITEMS) {
            break;
        }
        if property_applies_to_mock(property, direction) && !names.contains(&name.as_str()) {
            names.push(name);
        }
    }

    let required_names: HashSet<_> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut object = Map::new();
    for name in names {
        let property = properties.get(name)?;
        if let Some(value) = mock_value_from_schema(property, direction, depth + 1) {
            object.insert(name.to_string(), value);
        } else if required_names.contains(name) {
            return None;
        }
    }
    if object.len() < minimum || object.len() > maximum {
        return None;
    }
    Some(Value::Object(object))
}

fn mock_array(
    schema: &Map<String, Value>,
    direction: SchemaDirection,
    depth: usize,
) -> Option<Value> {
    let minimum = schema
        .get("minItems")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let maximum = schema
        .get("maxItems")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX);
    if minimum > maximum || minimum > MAX_MOCK_COLLECTION_ITEMS {
        return None;
    }
    let count = if minimum > 0 {
        minimum
    } else if maximum == 0 {
        0
    } else {
        1
    };
    if count == 0 {
        return Some(Value::Array(Vec::new()));
    }
    if count > 1 && schema.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let item = schema
        .get("items")
        .and_then(|schema| mock_value_from_schema(schema, direction, depth + 1))?;
    Some(Value::Array(vec![item; count]))
}

fn mock_number(schema: &Map<String, Value>, integer: bool) -> Option<Value> {
    let mut candidates = vec![Value::from(0), Value::from(1), Value::from(-1)];
    for keyword in [
        "minimum",
        "exclusiveMinimum",
        "maximum",
        "exclusiveMaximum",
        "multipleOf",
    ] {
        if let Some(value) = schema.get(keyword).filter(|value| value.is_number()) {
            candidates.push(value.clone());
        }
    }

    if integer {
        let lower = integer_lower_bound(schema);
        let upper = integer_upper_bound(schema);
        if lower.zip(upper).is_some_and(|(lower, upper)| lower > upper) {
            return None;
        }
        let mut candidate = lower.unwrap_or(0).max(0.min(upper.unwrap_or(0)));
        if let Some(multiple) = schema
            .get("multipleOf")
            .and_then(Value::as_number)
            .and_then(exact_integer)
            .filter(|multiple| *multiple > 0)
        {
            candidate = ceil_multiple(candidate, multiple);
        }
        if upper.is_none_or(|upper| candidate <= upper)
            && let Some(value) = integer_value(candidate)
        {
            candidates.insert(0, value);
        }
    } else if let (Some(minimum), Some(multiple)) = (
        schema
            .get("minimum")
            .and_then(Value::as_number)
            .and_then(safe_f64),
        schema
            .get("multipleOf")
            .and_then(Value::as_number)
            .and_then(safe_f64)
            .filter(|multiple| *multiple > 0.0),
    ) && let Some(number) = Number::from_f64((minimum / multiple).ceil() * multiple)
    {
        candidates.insert(0, Value::Number(number));
    }

    candidates.into_iter().find(|candidate| {
        if integer && !json_type_matches(candidate, "integer") {
            return false;
        }
        let Some(number) = candidate.as_number() else {
            return false;
        };
        let mut check = ContractCheck {
            checked: true,
            ..ContractCheck::default()
        };
        validate_numeric_bounds(number, schema, "mock", &mut check);
        check.is_valid()
    })
}

fn integer_lower_bound(schema: &Map<String, Value>) -> Option<i128> {
    let mut lower = schema
        .get("minimum")
        .and_then(Value::as_number)
        .and_then(exact_integer);
    if schema.get("exclusiveMinimum").and_then(Value::as_bool) == Some(true) {
        lower = lower.and_then(|value| value.checked_add(1));
    }
    if let Some(exclusive) = schema
        .get("exclusiveMinimum")
        .and_then(Value::as_number)
        .and_then(exact_integer)
        .and_then(|value| value.checked_add(1))
    {
        lower = Some(lower.map_or(exclusive, |lower| lower.max(exclusive)));
    }
    lower
}

fn integer_upper_bound(schema: &Map<String, Value>) -> Option<i128> {
    let mut upper = schema
        .get("maximum")
        .and_then(Value::as_number)
        .and_then(exact_integer);
    if schema.get("exclusiveMaximum").and_then(Value::as_bool) == Some(true) {
        upper = upper.and_then(|value| value.checked_sub(1));
    }
    if let Some(exclusive) = schema
        .get("exclusiveMaximum")
        .and_then(Value::as_number)
        .and_then(exact_integer)
        .and_then(|value| value.checked_sub(1))
    {
        upper = Some(upper.map_or(exclusive, |upper| upper.min(exclusive)));
    }
    upper
}

fn ceil_multiple(value: i128, multiple: i128) -> i128 {
    let quotient = value.div_euclid(multiple);
    let remainder = value.rem_euclid(multiple);
    if remainder == 0 {
        quotient * multiple
    } else {
        (quotient + 1) * multiple
    }
}

fn integer_value(value: i128) -> Option<Value> {
    i64::try_from(value)
        .ok()
        .map(Value::from)
        .or_else(|| u64::try_from(value).ok().map(Value::from))
}

fn filter_schema_derived_mock(
    value: Value,
    schema: &Map<String, Value>,
    direction: SchemaDirection,
    depth: usize,
) -> Value {
    if depth >= MAX_MOCK_SCHEMA_DEPTH {
        return Value::Null;
    }
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .filter_map(|(name, value)| {
                    if property_is_hidden(schema, &name, direction, depth) {
                        return None;
                    }
                    let value =
                        property_schema(schema, &name, depth).map_or(value.clone(), |schema| {
                            schema.as_object().map_or(value.clone(), |schema| {
                                filter_schema_derived_mock(value, schema, direction, depth + 1)
                            })
                        });
                    Some((name, value))
                })
                .collect(),
        ),
        Value::Array(values) => {
            let items = schema.get("items").and_then(Value::as_object);
            Value::Array(
                values
                    .into_iter()
                    .map(|value| {
                        items.map_or(value.clone(), |items| {
                            filter_schema_derived_mock(value, items, direction, depth + 1)
                        })
                    })
                    .collect(),
            )
        }
        value => value,
    }
}

fn property_is_hidden(
    schema: &Map<String, Value>,
    name: &str,
    direction: SchemaDirection,
    depth: usize,
) -> bool {
    if depth >= MAX_MOCK_SCHEMA_DEPTH {
        return false;
    }
    if schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(name))
        .is_some_and(|schema| !property_applies_to_mock(schema, direction))
    {
        return true;
    }
    ["allOf", "anyOf", "oneOf"].iter().any(|keyword| {
        schema
            .get(*keyword)
            .and_then(Value::as_array)
            .is_some_and(|branches| {
                branches.iter().any(|branch| {
                    branch.as_object().is_some_and(|branch| {
                        property_is_hidden(branch, name, direction, depth + 1)
                    })
                })
            })
    })
}

fn property_schema<'a>(
    schema: &'a Map<String, Value>,
    name: &str,
    depth: usize,
) -> Option<&'a Value> {
    if depth >= MAX_MOCK_SCHEMA_DEPTH {
        return None;
    }
    schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(name))
        .or_else(|| {
            ["allOf", "anyOf", "oneOf"].iter().find_map(|keyword| {
                schema
                    .get(*keyword)
                    .and_then(Value::as_array)
                    .and_then(|branches| {
                        branches.iter().find_map(|branch| {
                            branch
                                .as_object()
                                .and_then(|branch| property_schema(branch, name, depth + 1))
                        })
                    })
            })
        })
}

fn property_applies_to_mock(schema: &Value, direction: SchemaDirection) -> bool {
    match direction {
        SchemaDirection::Request => schema.get("readOnly").and_then(Value::as_bool) != Some(true),
        SchemaDirection::Response => schema.get("writeOnly").and_then(Value::as_bool) != Some(true),
        SchemaDirection::Neutral => true,
    }
}

fn mock_string(schema: &Map<String, Value>) -> Option<String> {
    let mut value = match schema.get("format").and_then(Value::as_str) {
        Some("date") => "2026-01-01",
        Some("date-time") => "2026-01-01T00:00:00Z",
        Some("email") => "user@example.com",
        Some("uuid") => "00000000-0000-4000-8000-000000000000",
        Some("uri" | "url") => "https://example.com",
        _ => "string",
    }
    .to_string();
    let minimum = schema
        .get("minLength")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let maximum = schema
        .get("maxLength")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(usize::MAX);
    if minimum > maximum || minimum > MAX_MOCK_STRING_LENGTH {
        return None;
    }
    let current = value.chars().count();
    if current < minimum {
        value.extend(std::iter::repeat_n('x', minimum - current));
    } else if current > maximum {
        value = value.chars().take(maximum).collect();
    }
    Some(value)
}

fn render_mock_body(value: &Value, content_type: &str) -> String {
    if !is_json_content_type(content_type) {
        return match value {
            Value::String(value) => value.clone(),
            Value::Null => String::new(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::Array(_) | Value::Object(_) => String::new(),
        };
    }
    serde_json::to_string_pretty(value).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use super::{
        ApiParameter, Endpoint, ExchangePart, LogEntry, MAX_CONTRACT_FINDINGS,
        MAX_SCHEMA_VALIDATION_DEPTH, MAX_SCHEMA_VALIDATION_STEPS, MediaTypeSpec, RequestBodySpec,
        ResponseSpec, validate_exchange,
    };

    #[test]
    fn endpoint_matches_literal_and_template_paths() {
        let endpoint = Endpoint::new("GET", "/users/{userId}");

        assert!(endpoint.matches("get", "/users/42?expand=team"));
        assert!(!endpoint.matches("POST", "/users/42"));
        assert!(!endpoint.matches("GET", "/users/42/settings"));
        assert!(!endpoint.matches("GET", "/teams/42"));
    }

    #[test]
    fn root_path_matches_root_only() {
        let endpoint = Endpoint::new("GET", "/");
        assert!(endpoint.matches("GET", "/"));
        assert!(!endpoint.matches("GET", "/health"));
    }

    #[test]
    fn validates_required_parameters_and_json_schemas() {
        let endpoint = Endpoint {
            parameters: vec![
                ApiParameter {
                    name: "id".into(),
                    location: "path".into(),
                    required: true,
                    ..ApiParameter::default()
                },
                ApiParameter {
                    name: "token".into(),
                    location: "query".into(),
                    required: true,
                    ..ApiParameter::default()
                },
            ],
            request_body: Some(RequestBodySpec {
                required: true,
                content: BTreeMap::from([(
                    "application/json".into(),
                    MediaTypeSpec {
                        schema: Some(json!({
                            "type": "object",
                            "required": ["name"],
                            "properties": { "name": { "type": "string" } }
                        })),
                        ..MediaTypeSpec::default()
                    },
                )]),
                ..RequestBodySpec::default()
            }),
            responses: BTreeMap::from([(
                "201".into(),
                ResponseSpec {
                    content: BTreeMap::from([(
                        "application/json".into(),
                        MediaTypeSpec {
                            schema: Some(json!({
                                "type": "object",
                                "required": ["id"],
                                "properties": { "id": { "type": "integer" } }
                            })),
                            ..MediaTypeSpec::default()
                        },
                    )]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("POST", "/users/{id}")
        };
        let entry = LogEntry {
            method: "POST".into(),
            path: "/users/42".into(),
            query: None,
            status: 201,
            request: ExchangePart {
                headers: BTreeMap::from([(
                    "Content-Type".into(),
                    "application/json; charset=utf-8".into(),
                )]),
                body: json!({}).to_string(),
                size: 2,
                ..ExchangePart::default()
            },
            response: ExchangePart {
                headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
                body: json!({ "id": "not-an-integer" }).to_string(),
                size: 23,
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        };

        let check = validate_exchange(&[endpoint], &entry);
        let codes: Vec<_> = check
            .violations
            .iter()
            .map(|violation| violation.code.as_str())
            .collect();
        assert!(codes.contains(&"missing_required_parameter"));
        assert!(codes.contains(&"missing_required_property"));
        assert!(codes.contains(&"schema_type_mismatch"));
    }

    #[test]
    fn validates_present_parameter_values_in_every_location() {
        let endpoint = Endpoint {
            parameters: vec![
                ApiParameter {
                    name: "id".into(),
                    location: "path".into(),
                    required: true,
                    schema: Some(json!({ "type": "integer" })),
                    ..ApiParameter::default()
                },
                ApiParameter {
                    name: "ratio".into(),
                    location: "query".into(),
                    schema: Some(json!({ "type": "number" })),
                    ..ApiParameter::default()
                },
                ApiParameter {
                    name: "X-Enabled".into(),
                    location: "header".into(),
                    schema: Some(json!({ "type": "boolean" })),
                    ..ApiParameter::default()
                },
                ApiParameter {
                    name: "ids".into(),
                    location: "cookie".into(),
                    explode: Some(false),
                    schema: Some(json!({
                        "type": "array",
                        "items": { "type": "integer" }
                    })),
                    ..ApiParameter::default()
                },
            ],
            responses: BTreeMap::from([("204".into(), ResponseSpec::default())]),
            ..Endpoint::new("GET", "/items/{id}")
        };
        let valid = LogEntry {
            method: "GET".into(),
            path: "/items/42".into(),
            query: Some("ratio=1.5".into()),
            status: 204,
            request: ExchangePart {
                headers: BTreeMap::from([
                    ("X-Enabled".into(), "true".into()),
                    ("Cookie".into(), "ids=1,2,3".into()),
                ]),
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        };
        assert!(validate_exchange(std::slice::from_ref(&endpoint), &valid).is_valid());

        let invalid = LogEntry {
            path: "/items/not-an-integer".into(),
            query: Some("ratio=not-a-number".into()),
            request: ExchangePart {
                headers: BTreeMap::from([
                    ("X-Enabled".into(), "yes".into()),
                    ("Cookie".into(), "ids=1,nope".into()),
                ]),
                ..ExchangePart::default()
            },
            ..valid
        };
        let check = validate_exchange(&[endpoint], &invalid);
        let locations: Vec<_> = check
            .violations
            .iter()
            .filter(|violation| violation.code == "schema_type_mismatch")
            .map(|violation| violation.location.as_str())
            .collect();
        assert_eq!(locations.len(), 4);
        assert!(locations.contains(&"request.path.id"));
        assert!(locations.contains(&"request.query.ratio"));
        assert!(locations.contains(&"request.header.X-Enabled"));
        assert!(locations.contains(&"request.cookie.ids"));
    }

    #[test]
    fn exact_media_type_wins_over_wildcards() {
        let endpoint = Endpoint {
            responses: BTreeMap::from([(
                "200".into(),
                ResponseSpec {
                    content: BTreeMap::from([
                        (
                            "*/*".into(),
                            MediaTypeSpec {
                                schema: Some(json!({ "type": "string" })),
                                ..MediaTypeSpec::default()
                            },
                        ),
                        (
                            "application/json".into(),
                            MediaTypeSpec {
                                schema: Some(json!({ "type": "integer" })),
                                ..MediaTypeSpec::default()
                            },
                        ),
                    ]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("GET", "/value")
        };
        let entry = LogEntry {
            method: "GET".into(),
            path: "/value".into(),
            status: 200,
            response: ExchangePart {
                headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
                body: "7".into(),
                size: 1,
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        };

        assert!(validate_exchange(&[endpoint], &entry).is_valid());
    }

    #[test]
    fn one_of_requires_exactly_one_matching_schema() {
        let endpoint = Endpoint {
            responses: BTreeMap::from([(
                "200".into(),
                ResponseSpec {
                    content: BTreeMap::from([(
                        "application/json".into(),
                        MediaTypeSpec {
                            schema: Some(json!({
                                "oneOf": [
                                    { "type": "number" },
                                    { "type": "integer" }
                                ]
                            })),
                            ..MediaTypeSpec::default()
                        },
                    )]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("GET", "/value")
        };
        let entry = LogEntry {
            method: "GET".into(),
            path: "/value".into(),
            status: 200,
            response: ExchangePart {
                headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
                body: "1".into(),
                size: 1,
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        };
        assert_eq!(
            validate_exchange(std::slice::from_ref(&endpoint), &entry).violations[0].code,
            "schema_composition_mismatch"
        );

        let mut single_match = entry.clone();
        single_match.response.body = "1.5".into();
        single_match.response.size = 3;
        assert!(validate_exchange(&[endpoint], &single_match).is_valid());
    }

    #[test]
    fn read_only_and_write_only_properties_are_directional() {
        let endpoint = Endpoint {
            request_body: Some(RequestBodySpec {
                required: true,
                content: BTreeMap::from([(
                    "application/json".into(),
                    MediaTypeSpec {
                        schema: Some(json!({
                            "type": "object",
                            "required": ["serverId", "name"],
                            "properties": {
                                "serverId": { "type": "integer", "readOnly": true },
                                "name": { "type": "string" }
                            }
                        })),
                        ..MediaTypeSpec::default()
                    },
                )]),
                ..RequestBodySpec::default()
            }),
            responses: BTreeMap::from([(
                "200".into(),
                ResponseSpec {
                    content: BTreeMap::from([(
                        "application/json".into(),
                        MediaTypeSpec {
                            schema: Some(json!({
                                "type": "object",
                                "required": ["password", "id"],
                                "properties": {
                                    "password": { "type": "string", "writeOnly": true },
                                    "id": { "type": "integer" }
                                }
                            })),
                            ..MediaTypeSpec::default()
                        },
                    )]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("POST", "/users")
        };
        let entry = LogEntry {
            method: "POST".into(),
            path: "/users".into(),
            status: 200,
            request: ExchangePart {
                headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
                body: json!({ "name": "Ada" }).to_string(),
                size: 14,
                ..ExchangePart::default()
            },
            response: ExchangePart {
                headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
                body: json!({ "id": 7 }).to_string(),
                size: 8,
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        };

        assert!(validate_exchange(&[endpoint], &entry).is_valid());
    }

    #[test]
    fn compressed_and_truncated_json_bodies_are_inconclusive() {
        let endpoint = Endpoint {
            responses: BTreeMap::from([(
                "200".into(),
                ResponseSpec {
                    content: BTreeMap::from([(
                        "application/json".into(),
                        MediaTypeSpec {
                            schema: Some(json!({
                                "type": "object",
                                "required": ["id"]
                            })),
                            ..MediaTypeSpec::default()
                        },
                    )]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("GET", "/compressed")
        };
        let compressed = LogEntry {
            method: "GET".into(),
            path: "/compressed".into(),
            status: 200,
            response: ExchangePart {
                headers: BTreeMap::from([
                    ("Content-Type".into(), "application/json".into()),
                    ("Content-Encoding".into(), "gzip".into()),
                ]),
                body: "not raw JSON".into(),
                size: 12,
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        };
        let compressed_check = validate_exchange(std::slice::from_ref(&endpoint), &compressed);
        assert!(compressed_check.inconclusive);
        assert!(!compressed_check.is_valid());
        assert!(compressed_check.violations.iter().any(|violation| {
            violation.code == "validation_inconclusive"
                && violation.message.contains("content-encoded")
        }));

        let mut truncated = compressed.clone();
        truncated.response.truncated = true;
        truncated.response.headers =
            BTreeMap::from([("Content-Type".into(), "application/json".into())]);
        let truncated_check = validate_exchange(std::slice::from_ref(&endpoint), &truncated);
        assert!(truncated_check.inconclusive);
        assert!(truncated_check.violations.iter().any(|violation| {
            violation.code == "validation_inconclusive" && violation.message.contains("truncated")
        }));

        let mut identity = compressed.clone();
        identity.response.headers = BTreeMap::from([
            ("Content-Type".into(), "application/json".into()),
            ("Content-Encoding".into(), "identity".into()),
        ]);
        assert_eq!(
            validate_exchange(&[endpoint], &identity).violations[0].code,
            "invalid_json"
        );
    }

    #[test]
    fn parses_documented_json_even_without_a_schema() {
        let endpoint = Endpoint {
            responses: BTreeMap::from([(
                "200".into(),
                ResponseSpec {
                    content: BTreeMap::from([(
                        "application/json".into(),
                        MediaTypeSpec::default(),
                    )]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("GET", "/untyped-json")
        };
        let entry = LogEntry {
            method: "GET".into(),
            path: "/untyped-json".into(),
            status: 200,
            response: ExchangePart {
                headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
                body: "not-json".into(),
                size: 8,
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        };

        assert_eq!(
            validate_exchange(&[endpoint], &entry).violations[0].code,
            "invalid_json"
        );
    }

    #[test]
    fn unsupported_schema_assertions_are_explicitly_inconclusive() {
        let endpoint = endpoint_with_json_response(json!({
            "type": "string",
            "pattern": "^[a-z]+$"
        }));
        let entry = json_response_entry(json!("abc"));

        let check = validate_exchange(&[endpoint], &entry);
        assert!(check.inconclusive);
        assert!(!check.is_valid());
        assert!(check.violations.iter().any(|violation| {
            violation.code == "validation_inconclusive" && violation.message.contains("pattern")
        }));
    }

    #[test]
    fn validates_common_object_string_array_and_numeric_assertions() {
        let endpoint = endpoint_with_json_response(json!({
            "type": "object",
            "additionalProperties": false,
            "minProperties": 1,
            "properties": {
                "name": { "type": "string", "minLength": 3, "maxLength": 5 },
                "scores": {
                    "type": "array",
                    "minItems": 2,
                    "maxItems": 3,
                    "uniqueItems": true,
                    "items": { "type": "number", "minimum": 1, "exclusiveMaximum": 10, "multipleOf": 0.5 }
                }
            }
        }));
        let entry = json_response_entry(json!({
            "name": "x",
            "scores": [0, 0, 10.0, 2.25],
            "unexpected": true
        }));

        let check = validate_exchange(&[endpoint], &entry);
        let codes: Vec<_> = check
            .violations
            .iter()
            .map(|violation| violation.code.as_str())
            .collect();
        assert!(codes.contains(&"additional_property_not_allowed"));
        assert!(codes.contains(&"schema_size_mismatch"));
        assert!(codes.contains(&"schema_value_mismatch"));
        assert!(codes.contains(&"schema_range_mismatch"));
        assert!(!check.inconclusive);
    }

    #[test]
    fn schema_depth_and_finding_budgets_make_results_partial() {
        let mut deep_schema = json!({ "type": "string" });
        let mut deep_value = json!("leaf");
        for _ in 0..=MAX_SCHEMA_VALIDATION_DEPTH {
            deep_schema = json!({ "type": "array", "items": deep_schema });
            deep_value = json!([deep_value]);
        }
        let deep = validate_exchange(
            &[endpoint_with_json_response(deep_schema)],
            &json_response_entry(deep_value),
        );
        assert!(deep.inconclusive);
        assert!(
            deep.violations
                .iter()
                .any(|violation| { violation.message.contains("supported depth") })
        );

        let many = validate_exchange(
            &[endpoint_with_json_response(json!({
                "type": "array",
                "items": { "type": "integer" }
            }))],
            &json_response_entry(Value::Array((0..150).map(|_| json!("wrong")).collect())),
        );
        assert!(many.inconclusive);
        assert_eq!(many.violations.len(), MAX_CONTRACT_FINDINGS);
        assert!(
            many.violations
                .iter()
                .any(|violation| { violation.code == "validation_budget_exceeded" })
        );
    }

    #[test]
    fn schema_traversal_budget_is_shared_across_arrays_branches_and_parameters() {
        let large_array = validate_exchange(
            &[endpoint_with_json_response(json!({
                "type": "array",
                "items": { "type": "integer" }
            }))],
            &json_response_entry(Value::Array(
                (0..MAX_SCHEMA_VALIDATION_STEPS)
                    .map(|index| json!(index))
                    .collect(),
            )),
        );
        assert!(large_array.inconclusive);
        assert!(large_array.violations.iter().any(|violation| {
            violation.code == "validation_budget_exceeded"
                && violation.message.contains("schema traversal")
        }));

        let branches: Vec<_> = (0..MAX_SCHEMA_VALIDATION_STEPS)
            .map(|_| json!({ "type": "string" }))
            .collect();
        let many_branches = validate_exchange(
            &[endpoint_with_json_response(json!({ "anyOf": branches }))],
            &json_response_entry(json!(true)),
        );
        assert!(many_branches.inconclusive);
        assert!(
            many_branches
                .violations
                .iter()
                .any(|violation| { violation.code == "validation_budget_exceeded" })
        );
        assert!(
            !many_branches
                .violations
                .iter()
                .any(|violation| { violation.code == "schema_composition_mismatch" })
        );

        let endpoint = Endpoint {
            parameters: (0..=MAX_SCHEMA_VALIDATION_STEPS)
                .map(|index| ApiParameter {
                    name: format!("optional-{index}"),
                    location: "query".into(),
                    ..ApiParameter::default()
                })
                .collect(),
            responses: BTreeMap::from([("204".into(), ResponseSpec::default())]),
            ..Endpoint::new("GET", "/value")
        };
        let parameter_check = validate_exchange(
            &[endpoint],
            &LogEntry {
                method: "GET".into(),
                path: "/value".into(),
                status: 204,
                ..LogEntry::default()
            },
        );
        assert!(parameter_check.inconclusive);
        assert!(
            parameter_check
                .violations
                .iter()
                .any(|violation| { violation.code == "validation_budget_exceeded" })
        );
    }

    #[test]
    fn nullable_only_relaxes_the_type_assertion_for_null() {
        let endpoint = endpoint_with_json_response(json!({
            "type": "string",
            "nullable": true,
            "minLength": 3,
            "enum": ["okay"]
        }));
        let non_null = validate_exchange(
            std::slice::from_ref(&endpoint),
            &json_response_entry(json!("x")),
        );
        assert!(
            non_null
                .violations
                .iter()
                .any(|violation| { violation.code == "schema_size_mismatch" })
        );

        let null = validate_exchange(&[endpoint], &json_response_entry(Value::Null));
        assert!(
            null.violations
                .iter()
                .any(|violation| { violation.code == "schema_value_mismatch" })
        );
    }

    #[test]
    fn integer_bounds_above_f64_precision_are_compared_exactly() {
        let endpoint = endpoint_with_json_response(json!({
            "type": "integer",
            "maximum": 9_007_199_254_740_992_u64
        }));
        let check = validate_exchange(
            &[endpoint],
            &json_response_entry(json!(9_007_199_254_740_993_u64)),
        );
        assert!(!check.inconclusive);
        assert!(
            check
                .violations
                .iter()
                .any(|violation| { violation.code == "schema_range_mismatch" })
        );
    }

    #[test]
    fn unsupported_parameter_serialization_is_partial_not_false_valid() {
        let endpoint = Endpoint {
            parameters: vec![ApiParameter {
                name: "ids".into(),
                location: "query".into(),
                style: Some("pipeDelimited".into()),
                schema: Some(json!({
                    "type": "array",
                    "items": { "type": "integer" }
                })),
                ..ApiParameter::default()
            }],
            responses: BTreeMap::from([("204".into(), ResponseSpec::default())]),
            ..Endpoint::new("GET", "/value")
        };
        let entry = LogEntry {
            method: "GET".into(),
            path: "/value".into(),
            query: Some("ids=1%7C2".into()),
            status: 204,
            ..LogEntry::default()
        };
        let check = validate_exchange(&[endpoint], &entry);
        assert!(check.inconclusive);
        assert!(check.violations.iter().any(|violation| {
            violation.code == "validation_inconclusive"
                && violation.message.contains("pipeDelimited")
        }));
    }

    #[test]
    fn validates_documented_status_and_content_type() {
        let endpoint = Endpoint {
            responses: BTreeMap::from([(
                "2XX".into(),
                ResponseSpec {
                    content: BTreeMap::from([(
                        "application/json".into(),
                        MediaTypeSpec::default(),
                    )]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("GET", "/users")
        };
        let entry = LogEntry {
            method: "GET".into(),
            path: "/users".into(),
            status: 204,
            response: ExchangePart::default(),
            ..LogEntry::default()
        };
        assert!(validate_exchange(std::slice::from_ref(&endpoint), &entry).is_valid());

        let invalid = LogEntry {
            status: 404,
            ..entry
        };
        assert_eq!(
            validate_exchange(&[endpoint], &invalid).violations[0].code,
            "undocumented_status"
        );
    }

    #[test]
    fn builds_mock_response_from_example_or_schema() {
        let endpoint = Endpoint {
            responses: BTreeMap::from([(
                "201".into(),
                ResponseSpec {
                    content: BTreeMap::from([(
                        "application/json".into(),
                        MediaTypeSpec {
                            schema: Some(json!({
                                "type": "object",
                                "properties": {
                                    "id": { "type": "integer", "example": 7 },
                                    "name": { "type": "string" }
                                }
                            })),
                            ..MediaTypeSpec::default()
                        },
                    )]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("POST", "/users")
        };

        let mock = endpoint.mock_response().unwrap();
        assert_eq!(mock.status, 201);
        assert_eq!(mock.content_type.as_deref(), Some("application/json"));
        assert!(mock.body.contains("\"id\": 7"));
        assert!(mock.body.contains("\"name\": \"string\""));
    }

    #[test]
    fn mocks_prefer_success_and_omit_write_only_response_fields() {
        let endpoint = Endpoint {
            responses: BTreeMap::from([
                (
                    "404".into(),
                    ResponseSpec {
                        content: BTreeMap::from([(
                            "application/json".into(),
                            MediaTypeSpec {
                                example: Some(json!({ "error": "missing" })),
                                ..MediaTypeSpec::default()
                            },
                        )]),
                        ..ResponseSpec::default()
                    },
                ),
                (
                    "2XX".into(),
                    ResponseSpec {
                        content: BTreeMap::from([(
                            "application/json".into(),
                            MediaTypeSpec {
                                schema: Some(json!({
                                    "type": "object",
                                    "default": { "id": 7, "password": "secret" },
                                    "properties": {
                                        "id": { "type": "integer", "example": 7 },
                                        "password": { "type": "string", "writeOnly": true }
                                    }
                                })),
                                ..MediaTypeSpec::default()
                            },
                        )]),
                        ..ResponseSpec::default()
                    },
                ),
            ]),
            ..Endpoint::new("GET", "/mock")
        };

        let mock = endpoint.mock_response().unwrap();
        assert_eq!(mock.status, 200);
        assert!(mock.body.contains("\"id\": 7"));
        assert!(!mock.body.contains("password"));
    }

    #[test]
    fn non_json_schema_mocks_do_not_emit_json_objects() {
        let endpoint = Endpoint {
            responses: BTreeMap::from([(
                "200".into(),
                ResponseSpec {
                    content: BTreeMap::from([(
                        "application/xml".into(),
                        MediaTypeSpec {
                            schema: Some(json!({
                                "type": "object",
                                "properties": { "id": { "type": "integer" } }
                            })),
                            ..MediaTypeSpec::default()
                        },
                    )]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("GET", "/xml")
        };

        let mock = endpoint.mock_response().unwrap();
        assert_eq!(mock.content_type.as_deref(), Some("application/xml"));
        assert!(mock.body.is_empty());
    }

    #[test]
    fn synthesized_mocks_satisfy_supported_size_and_range_constraints() {
        let endpoint = endpoint_with_json_response(json!({
            "type": "object",
            "minProperties": 3,
            "maxProperties": 3,
            "required": ["label", "score", "values"],
            "properties": {
                "label": { "type": "string", "minLength": 8, "maxLength": 8 },
                "score": {
                    "type": "integer",
                    "minimum": 5,
                    "maximum": 10,
                    "multipleOf": 2
                },
                "values": {
                    "type": "array",
                    "minItems": 3,
                    "maxItems": 3,
                    "items": {
                        "type": "integer",
                        "minimum": 5,
                        "maximum": 10,
                        "multipleOf": 2
                    }
                }
            }
        }));
        let mock = endpoint.mock_response().unwrap();
        let value: Value = serde_json::from_str(&mock.body).unwrap();
        let check = validate_exchange(&[endpoint], &json_response_entry(value));
        assert!(check.is_valid(), "mock violations: {:?}", check.violations);
    }

    #[test]
    fn contract_check_is_backward_compatible_in_serialized_logs() {
        let entry: LogEntry = serde_json::from_value(json!({
            "method": "GET",
            "path": "/health",
            "status": 200,
            "timestamp": "",
            "request": { "headers": {}, "body": "" },
            "response": { "headers": {}, "body": "" },
            "latencyMs": 1
        }))
        .unwrap();
        assert!(!entry.contract.checked);
        assert!(!entry.contract.inconclusive);
        assert!(entry.contract.violations.is_empty());
    }

    fn endpoint_with_json_response(schema: Value) -> Endpoint {
        Endpoint {
            responses: BTreeMap::from([(
                "200".into(),
                ResponseSpec {
                    content: BTreeMap::from([(
                        "application/json".into(),
                        MediaTypeSpec {
                            schema: Some(schema),
                            ..MediaTypeSpec::default()
                        },
                    )]),
                    ..ResponseSpec::default()
                },
            )]),
            ..Endpoint::new("GET", "/value")
        }
    }

    fn json_response_entry(value: Value) -> LogEntry {
        let body = serde_json::to_string(&value).unwrap();
        LogEntry {
            method: "GET".into(),
            path: "/value".into(),
            status: 200,
            response: ExchangePart {
                headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
                size: body.len(),
                body,
                ..ExchangePart::default()
            },
            ..LogEntry::default()
        }
    }
}
