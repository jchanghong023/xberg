//! Response validation and merging for structured extraction batches.
//!
//! Validates each batch's vision-LLM JSON response against the schema,
//! then merges by the configured strategy (object merge, array concat, or
//! take-first). Produces merged output with per-batch error tracking.
//!
//! The merge strategy is the crate's own [`crate::core::config::MergeMode`]; the
//! caller decides which mode to apply. No preset or policy type crosses this
//! boundary.

use serde::{Deserialize, Serialize};

use crate::core::config::MergeMode;

/// Outcome of validating and merging a set of batch responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// All batches validated and merged.
    Success,
    /// At least one batch validated; at least one failed.
    PartialSuccess,
    /// No batch validated against the schema.
    SchemaInvalid,
    /// No batches were supplied, or the schema itself was invalid.
    Error,
}

/// Merged structured output plus validation bookkeeping.
pub struct MergedOutput {
    /// The merged JSON value.
    pub merged: serde_json::Value,
    /// Aggregate outcome across all batches.
    pub outcome: Outcome,
    /// Top-level error message, when the whole operation failed.
    pub error_message: Option<String>,
    #[allow(dead_code)]
    /// Per-batch validation/parse errors, surfaced by the orchestrator event payload.
    pub per_batch_errors: Vec<String>,
    /// Confidence score computed after merging (when available from the orchestrator).
    /// Set by the orchestrator after calling `validate_and_merge` + computing signals.
    pub confidence: Option<crate::heuristics::confidence::ExtractionConfidence>,
}

/// Validate each raw response and merge by the configured strategy.
///
/// # Arguments
/// * `raw_responses` - Vision-LLM JSON responses (already parsed as `serde_json::Value`)
/// * `schema` - JSON Schema for validation; its declared `$schema` draft is honored. ~keep
/// * `merge_mode` - Merging strategy
///
/// # Behavior
/// - Each response is validated against the schema using the `jsonschema` validator
/// - Merge strategy applied to all validated batches
/// - Per-batch validation failures collected in `per_batch_errors`
/// - Outcome determined by validation/merge success rate
pub fn validate_and_merge(
    raw_responses: Vec<serde_json::Value>,
    schema: &serde_json::Value,
    merge_mode: MergeMode,
) -> MergedOutput {
    if raw_responses.is_empty() {
        return MergedOutput {
            merged: serde_json::Value::Null,
            outcome: Outcome::Error,
            error_message: Some("no batches returned".to_string()),
            per_batch_errors: vec![],
            confidence: None,
        };
    }

    let validator = match jsonschema::options().offline().build(schema) {
        Ok(v) => v,
        Err(e) => {
            return MergedOutput {
                merged: serde_json::Value::Null,
                outcome: Outcome::Error,
                error_message: Some(format!("schema is invalid: {}", e)),
                per_batch_errors: vec![],
                confidence: None,
            };
        }
    };

    let mut validated_batches = Vec::new();
    let mut per_batch_errors = Vec::new();

    for (idx, raw) in raw_responses.into_iter().enumerate() {
        let value = match normalize_response(raw) {
            Ok(v) => v,
            Err(e) => {
                per_batch_errors.push(format!("batch {}: {}", idx, e));
                continue;
            }
        };

        match validator.validate(&value).err() {
            None => {
                validated_batches.push(value);
            }
            Some(e) => {
                let err_msg = format!("batch {}: schema validation failed: {}", idx, e);
                per_batch_errors.push(err_msg);
            }
        }
    }

    if validated_batches.is_empty() {
        return MergedOutput {
            merged: serde_json::Value::Null,
            outcome: Outcome::SchemaInvalid,
            error_message: Some("all batches failed schema validation".to_string()),
            per_batch_errors,
            confidence: None,
        };
    }

    let merged = merge_validated(validated_batches, merge_mode);

    let outcome = if per_batch_errors.is_empty() {
        Outcome::Success
    } else {
        Outcome::PartialSuccess
    };

    MergedOutput {
        merged,
        outcome,
        error_message: None,
        per_batch_errors,
        confidence: None,
    }
}

/// Normalize a response value: if it's a string, try to parse as JSON.
fn normalize_response(value: serde_json::Value) -> Result<serde_json::Value, String> {
    match value {
        serde_json::Value::String(s) => {
            serde_json::from_str(&s).map_err(|e| format!("failed to parse string as JSON: {}", e))
        }
        other => Ok(other),
    }
}

/// Merge validated batches by merge mode.
fn merge_validated(batches: Vec<serde_json::Value>, merge_mode: MergeMode) -> serde_json::Value {
    use crate::core::config::MergeMode::*;

    match merge_mode {
        ObjectMerge => {
            let mut result = serde_json::Map::new();
            for batch in batches {
                if let serde_json::Value::Object(obj) = batch {
                    for (k, v) in obj {
                        result.insert(k, v);
                    }
                }
            }
            serde_json::Value::Object(result)
        }
        ArrayConcat => {
            let mut result = Vec::new();
            for batch in batches {
                if let serde_json::Value::Array(arr) = batch {
                    result.extend(arr);
                }
            }
            serde_json::Value::Array(result)
        }
        ObjectFirst => batches.into_iter().next().unwrap_or(serde_json::Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    const SCHEMA_SERVER_TIMEOUT: Duration = Duration::from_secs(2);
    const SCHEMA_SERVER_POLL_INTERVAL: Duration = Duration::from_millis(5);

    fn serve_schema_once() -> (String, std::net::SocketAddr, thread::JoinHandle<bool>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback schema server");
        listener.set_nonblocking(true).expect("make schema server nonblocking");
        let address = listener.local_addr().expect("read schema server address");
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + SCHEMA_SERVER_TIMEOUT;
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // On BSD-derived systems (macOS) an accepted socket INHERITS O_NONBLOCK
                        // from its listener; on Linux it does not. Without this the read below
                        // returns EAGAIN immediately instead of honouring the timeout, so the
                        // server thread panics and the test fails only on macOS. ~keep
                        stream
                            .set_nonblocking(false)
                            .expect("make accepted schema stream blocking");
                        stream
                            .set_read_timeout(Some(SCHEMA_SERVER_TIMEOUT))
                            .expect("set schema request timeout");
                        let mut request_prefix = [0_u8; b"TEST SHUTDOWN".len()];
                        stream
                            .read_exact(&mut request_prefix)
                            .expect("read schema request prefix");
                        if request_prefix == *b"TEST SHUTDOWN" {
                            return false;
                        }

                        let body = r#"{"type":"object"}"#;
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .expect("write schema response");
                        return true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        thread::sleep(SCHEMA_SERVER_POLL_INTERVAL);
                    }
                    Err(error) => panic!("accept schema request: {error}"),
                }
            }
        });
        (format!("http://{address}/schema.json"), address, handle)
    }

    #[test]
    fn object_merge_happy_path() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "age": { "type": "number" }
            }
        });

        let batch1 = serde_json::json!({"name": "Alice"});
        let batch2 = serde_json::json!({"age": 30});

        let result = validate_and_merge(vec![batch1, batch2], &schema, MergeMode::ObjectMerge);

        assert_eq!(result.outcome, Outcome::Success);
        assert_eq!(result.per_batch_errors.len(), 0);
        assert_eq!(result.merged.get("name").map(|v| v.as_str()), Some(Some("Alice")));
        assert_eq!(result.merged.get("age").map(|v| v.as_u64()), Some(Some(30)));
    }

    #[test]
    fn object_merge_with_one_invalid_batch() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });

        let batch1 = serde_json::json!({"name": "Alice"});
        let batch2 = serde_json::json!({"name": 123});

        let result = validate_and_merge(vec![batch1, batch2], &schema, MergeMode::ObjectMerge);

        assert_eq!(result.outcome, Outcome::PartialSuccess);
        assert_eq!(result.per_batch_errors.len(), 1);
        assert!(result.per_batch_errors[0].contains("batch 1"));
        assert_eq!(result.merged.get("name").map(|v| v.as_str()), Some(Some("Alice")));
    }

    #[test]
    fn array_concat_happy_path() {
        let schema = serde_json::json!({
            "type": "array",
            "items": { "type": "object" }
        });

        let batch1 = serde_json::json!([{"id": 1}]);
        let batch2 = serde_json::json!([{"id": 2}]);

        let result = validate_and_merge(vec![batch1, batch2], &schema, MergeMode::ArrayConcat);

        assert_eq!(result.outcome, Outcome::Success);
        assert_eq!(result.per_batch_errors.len(), 0);
        assert!(result.merged.is_array());
        let arr = result.merged.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn object_first_ignores_later_batches() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "value": { "type": "string" }
            }
        });

        let batch1 = serde_json::json!({"value": "first"});
        let batch2 = serde_json::json!({"value": "second"});

        let result = validate_and_merge(vec![batch1, batch2], &schema, MergeMode::ObjectFirst);

        assert_eq!(result.outcome, Outcome::Success);
        assert_eq!(result.merged.get("value").map(|v| v.as_str()), Some(Some("first")));
    }

    #[test]
    fn all_invalid_returns_schema_invalid() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": { "type": "number" }
            },
            "required": ["count"]
        });

        let batch1 = serde_json::json!({"count": "not a number"});
        let batch2 = serde_json::json!({"count": "also not a number"});

        let result = validate_and_merge(vec![batch1, batch2], &schema, MergeMode::ObjectMerge);

        assert_eq!(result.outcome, Outcome::SchemaInvalid);
        assert_eq!(
            result.error_message,
            Some("all batches failed schema validation".to_string())
        );
        assert_eq!(result.merged, serde_json::Value::Null);
        assert_eq!(result.per_batch_errors.len(), 2);
    }

    #[test]
    fn empty_batches_returns_error() {
        let schema = serde_json::json!({"type": "object"});
        let result = validate_and_merge(vec![], &schema, MergeMode::ObjectMerge);

        assert_eq!(result.outcome, Outcome::Error);
        assert_eq!(result.error_message, Some("no batches returned".to_string()));
        assert_eq!(result.merged, serde_json::Value::Null);
    }

    #[test]
    fn string_wrapped_json_is_normalized() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "key": { "type": "string" }
            }
        });

        let batch_as_string = serde_json::json!(r#"{"key": "value"}"#);
        let result = validate_and_merge(vec![batch_as_string], &schema, MergeMode::ObjectMerge);

        assert_eq!(result.outcome, Outcome::Success);
        assert_eq!(result.merged.get("key").map(|v| v.as_str()), Some(Some("value")));
    }

    #[test]
    fn should_honor_the_schema_declared_draft() {
        let schema = serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "array",
            "items": [
                { "type": "string" },
                { "type": "integer" }
            ],
            "additionalItems": false
        });

        let result = validate_and_merge(vec![serde_json::json!(["entry", 7])], &schema, MergeMode::ArrayConcat);

        assert_eq!(result.outcome, Outcome::Success);
        assert_eq!(result.merged, serde_json::json!(["entry", 7]));
    }

    #[test]
    fn should_reject_remote_schema_references_without_http_access() {
        let (schema_url, address, server) = serve_schema_once();
        let schema = serde_json::json!({"$ref": schema_url});
        let result = validate_and_merge(vec![serde_json::json!({})], &schema, MergeMode::ObjectMerge);
        if let Ok(mut shutdown) = TcpStream::connect(address) {
            shutdown.write_all(b"TEST SHUTDOWN").expect("stop schema server");
        }
        let was_accessed = server.join().expect("join schema server");

        assert!(!was_accessed, "schema compilation must not access remote HTTP targets");
        assert_eq!(result.outcome, Outcome::Error);
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("Retrieval is disabled")),
            "offline rejection must explain that retrieval is disabled: {:?}",
            result.error_message
        );
    }

    #[test]
    fn should_reject_file_schema_references_when_the_target_exists() {
        let mut target = tempfile::NamedTempFile::new().expect("create referenced schema file");
        target
            .write_all(br#"{"type":"object"}"#)
            .expect("write referenced schema");
        let schema_url = url::Url::from_file_path(target.path())
            .expect("convert referenced schema path to file URL")
            .to_string();
        let schema = serde_json::json!({"$ref": schema_url});

        let result = validate_and_merge(vec![serde_json::json!({})], &schema, MergeMode::ObjectMerge);

        assert_eq!(result.outcome, Outcome::Error);
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("Retrieval is disabled")),
            "offline rejection must explain that retrieval is disabled: {:?}",
            result.error_message
        );
    }

    #[test]
    fn should_allow_in_document_schema_references_offline() {
        let schema = serde_json::json!({
            "$defs": { "identifier": { "type": "string" } },
            "type": "object",
            "properties": { "id": { "$ref": "#/$defs/identifier" } },
            "required": ["id"]
        });

        let result = validate_and_merge(
            vec![serde_json::json!({"id": "document-1"})],
            &schema,
            MergeMode::ObjectMerge,
        );

        assert_eq!(result.outcome, Outcome::Success);

        let invalid_result = validate_and_merge(vec![serde_json::json!({"id": 42})], &schema, MergeMode::ObjectMerge);
        assert_eq!(invalid_result.outcome, Outcome::SchemaInvalid);
    }
}
