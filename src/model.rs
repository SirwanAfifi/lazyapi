use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Endpoint {
    pub method: String,
    pub path: String,
}

impl Endpoint {
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
}

fn split_path(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ExchangePart {
    pub headers: BTreeMap<String, String>,
    pub body: String,
    #[serde(default)]
    pub size: usize,
    #[serde(default)]
    pub truncated: bool,
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
}

#[cfg(test)]
mod tests {
    use super::Endpoint;

    #[test]
    fn endpoint_matches_literal_and_template_paths() {
        let endpoint = Endpoint {
            method: "GET".into(),
            path: "/users/{userId}".into(),
        };

        assert!(endpoint.matches("get", "/users/42?expand=team"));
        assert!(!endpoint.matches("POST", "/users/42"));
        assert!(!endpoint.matches("GET", "/users/42/settings"));
        assert!(!endpoint.matches("GET", "/teams/42"));
    }

    #[test]
    fn root_path_matches_root_only() {
        let endpoint = Endpoint {
            method: "GET".into(),
            path: "/".into(),
        };
        assert!(endpoint.matches("GET", "/"));
        assert!(!endpoint.matches("GET", "/health"));
    }
}
