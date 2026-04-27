use std::time::Duration;
use std::time::SystemTime;

use opentelemetry::Context;
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::trace::Span as _;
use opentelemetry::trace::SpanContext;
use opentelemetry::trace::SpanId;
use opentelemetry::trace::SpanKind;
use opentelemetry::trace::TraceContextExt;
use opentelemetry::trace::TraceFlags;
use opentelemetry::trace::TraceId;
use opentelemetry::trace::TraceState;
use opentelemetry::trace::Tracer;
use serde_json::Map;
use serde_json::Value;

const NODE_REPL_TELEMETRY_TRACER_NAME: &str = "codex-node-repl-stderr";
const CURRENT_SCHEMA_VERSION: u64 = 1;
const SPAN_RECORD_TYPE: &str = "span";
const MAX_ATTRIBUTE_STRING_BYTES: usize = 1024;

const ALLOWED_SPAN_NAMES: &[&str] = &[
    "node_repl.js",
    "browser_use.playwright.dom_snapshot",
    "browser_use.tab.goto",
    "browser_use.tab.click",
    "browser_use.tab.type",
    "browser_use.tab.screenshot",
    "browser_use.cdp.execute",
    "browser_use.tab.wait_for_load_state",
];

const ALLOWED_ATTRIBUTE_PREFIXES: &[&str] = &["browser_use.", "node_repl.", "js."];
const ALLOWED_ATTRIBUTE_KEYS: &[&str] = &["error.type", "error.message"];

#[derive(Debug, thiserror::Error)]
pub enum StderrSpanTelemetryError {
    #[error("telemetry payload must be a JSON object")]
    NotObject,
    #[error("unsupported telemetry schema version")]
    UnsupportedVersion,
    #[error("unsupported telemetry record type")]
    UnsupportedType,
    #[error("missing or invalid telemetry field `{0}`")]
    InvalidField(&'static str),
    #[error("unsupported telemetry span name")]
    UnsupportedSpanName,
}

#[derive(Debug, Clone, PartialEq)]
struct SpanTelemetryRecord {
    name: String,
    span_id: Option<SpanId>,
    trace_id: Option<TraceId>,
    parent_span_id: Option<SpanId>,
    trace_flags: Option<TraceFlags>,
    traceparent: Option<String>,
    tracestate: Option<String>,
    start_time: SystemTime,
    end_time: SystemTime,
    attributes: Vec<KeyValue>,
}

pub fn emit_node_repl_stderr_span_telemetry(
    payload: Value,
) -> Result<(), StderrSpanTelemetryError> {
    let record = parse_span_telemetry_record(payload)?;
    let tracer = global::tracer(NODE_REPL_TELEMETRY_TRACER_NAME);
    emit_span_telemetry_record_with_tracer(&tracer, &record)
}

fn emit_span_telemetry_record_with_tracer<T>(
    tracer: &T,
    record: &SpanTelemetryRecord,
) -> Result<(), StderrSpanTelemetryError>
where
    T: Tracer,
{
    let parent_context = record.parent_context()?;

    let mut builder = tracer
        .span_builder(record.name.clone())
        .with_kind(SpanKind::Internal)
        .with_start_time(record.start_time)
        .with_attributes(record.attributes.clone());
    if let Some(span_id) = record.span_id {
        builder = builder.with_span_id(span_id);
    }
    let mut span = tracer.build_with_context(builder, &parent_context);
    span.end_with_timestamp(record.end_time);
    Ok(())
}

impl SpanTelemetryRecord {
    fn parent_context(&self) -> Result<Context, StderrSpanTelemetryError> {
        if let (Some(trace_id), Some(parent_span_id), Some(trace_flags)) =
            (self.trace_id, self.parent_span_id, self.trace_flags)
        {
            let trace_state = trace_state_from_header(self.tracestate.as_deref())?;
            let span_context = SpanContext::new(
                trace_id,
                parent_span_id,
                trace_flags,
                /*is_remote*/ true,
                trace_state,
            );
            return Ok(Context::new().with_remote_span_context(span_context));
        }

        let Some(traceparent) = self.traceparent.as_deref() else {
            return Err(StderrSpanTelemetryError::InvalidField("traceparent"));
        };
        crate::trace_context::context_from_trace_headers(
            Some(traceparent),
            self.tracestate.as_deref(),
        )
        .ok_or(StderrSpanTelemetryError::InvalidField("traceparent"))
    }
}

fn parse_span_telemetry_record(
    payload: Value,
) -> Result<SpanTelemetryRecord, StderrSpanTelemetryError> {
    let object = payload
        .as_object()
        .ok_or(StderrSpanTelemetryError::NotObject)?;

    match object.get("v").and_then(Value::as_u64) {
        Some(CURRENT_SCHEMA_VERSION) => {}
        Some(_) => return Err(StderrSpanTelemetryError::UnsupportedVersion),
        None => return Err(StderrSpanTelemetryError::InvalidField("v")),
    }

    match object.get("type").and_then(Value::as_str) {
        Some(SPAN_RECORD_TYPE) => {}
        Some(_) => return Err(StderrSpanTelemetryError::UnsupportedType),
        None => return Err(StderrSpanTelemetryError::InvalidField("type")),
    }

    let name = required_string(object, "name")?.to_string();
    if !ALLOWED_SPAN_NAMES.contains(&name.as_str()) {
        return Err(StderrSpanTelemetryError::UnsupportedSpanName);
    }

    let traceparent = optional_string(object, "traceparent").map(str::to_string);
    let span_id = optional_span_id(object, "span_id")?;
    let trace_id = optional_trace_id(object, "trace_id")?;
    let parent_span_id = optional_span_id(object, "parent_span_id")?;
    let trace_flags = optional_trace_flags(object, "trace_flags")?;
    let tracestate = optional_string(object, "tracestate").map(str::to_string);
    if span_id.is_some() || trace_id.is_some() || parent_span_id.is_some() || trace_flags.is_some()
    {
        if span_id.is_none() {
            return Err(StderrSpanTelemetryError::InvalidField("span_id"));
        }
        if trace_id.is_none() {
            return Err(StderrSpanTelemetryError::InvalidField("trace_id"));
        }
        if parent_span_id.is_none() {
            return Err(StderrSpanTelemetryError::InvalidField("parent_span_id"));
        }
        if trace_flags.is_none() {
            return Err(StderrSpanTelemetryError::InvalidField("trace_flags"));
        }
    } else if traceparent.is_none() {
        return Err(StderrSpanTelemetryError::InvalidField("traceparent"));
    }

    let start_time = timestamp_from_unix_nanos(required_u64_alias(
        object,
        &[
            "start_unix_nanos",
            "startTimeUnixNanos",
            "startTimeUnixNano",
            "start_time_unix_nanos",
        ],
        "start_unix_nanos",
    )?)?;
    let end_time = timestamp_from_unix_nanos(required_u64_alias(
        object,
        &[
            "end_unix_nanos",
            "endTimeUnixNanos",
            "endTimeUnixNano",
            "end_time_unix_nanos",
        ],
        "end_unix_nanos",
    )?)?;
    if end_time < start_time {
        return Err(StderrSpanTelemetryError::InvalidField("end_unix_nanos"));
    }

    let attributes = object
        .get("attrs")
        .or_else(|| object.get("attributes"))
        .and_then(Value::as_object)
        .map(sanitized_attributes)
        .unwrap_or_default();

    Ok(SpanTelemetryRecord {
        name,
        span_id,
        trace_id,
        parent_span_id,
        trace_flags,
        traceparent,
        tracestate,
        start_time,
        end_time,
        attributes,
    })
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
) -> Result<&'a str, StderrSpanTelemetryError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(StderrSpanTelemetryError::InvalidField(key))
}

fn optional_string<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn optional_trace_id(
    object: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<TraceId>, StderrSpanTelemetryError> {
    let Some(value) = optional_string(object, key) else {
        return Ok(None);
    };
    parse_trace_id(value)
        .map(Some)
        .map_err(|_| StderrSpanTelemetryError::InvalidField(key))
}

fn optional_span_id(
    object: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<SpanId>, StderrSpanTelemetryError> {
    let Some(value) = optional_string(object, key) else {
        return Ok(None);
    };
    parse_span_id(value)
        .map(Some)
        .map_err(|_| StderrSpanTelemetryError::InvalidField(key))
}

fn optional_trace_flags(
    object: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<TraceFlags>, StderrSpanTelemetryError> {
    let Some(value) = optional_string(object, key) else {
        return Ok(None);
    };
    parse_trace_flags(value)
        .map(Some)
        .map_err(|_| StderrSpanTelemetryError::InvalidField(key))
}

fn parse_trace_id(value: &str) -> Result<TraceId, ()> {
    if !is_exact_hex(value, 32) {
        return Err(());
    }
    let trace_id = TraceId::from_hex(value).map_err(|_| ())?;
    if trace_id == TraceId::INVALID {
        return Err(());
    }
    Ok(trace_id)
}

fn parse_span_id(value: &str) -> Result<SpanId, ()> {
    if !is_exact_hex(value, 16) {
        return Err(());
    }
    let span_id = SpanId::from_hex(value).map_err(|_| ())?;
    if span_id == SpanId::INVALID {
        return Err(());
    }
    Ok(span_id)
}

fn parse_trace_flags(value: &str) -> Result<TraceFlags, ()> {
    if !is_exact_hex(value, 2) {
        return Err(());
    }
    u8::from_str_radix(value, 16)
        .map(TraceFlags::new)
        .map_err(|_| ())
}

fn is_exact_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn trace_state_from_header(value: Option<&str>) -> Result<TraceState, StderrSpanTelemetryError> {
    let Some(value) = value else {
        return Ok(TraceState::default());
    };
    value
        .parse()
        .map_err(|_| StderrSpanTelemetryError::InvalidField("tracestate"))
}

fn required_u64_alias(
    object: &Map<String, Value>,
    keys: &[&'static str],
    error_key: &'static str,
) -> Result<u64, StderrSpanTelemetryError> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(u64_value))
        .ok_or(StderrSpanTelemetryError::InvalidField(error_key))
}

fn u64_value(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn timestamp_from_unix_nanos(nanos: u64) -> Result<SystemTime, StderrSpanTelemetryError> {
    let secs = nanos / 1_000_000_000;
    let sub_nanos = (nanos % 1_000_000_000) as u32;
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::new(secs, sub_nanos))
        .ok_or(StderrSpanTelemetryError::InvalidField("timestamp"))
}

fn sanitized_attributes(attrs: &Map<String, Value>) -> Vec<KeyValue> {
    attrs
        .iter()
        .filter(|(key, _)| is_allowed_attribute_key(key))
        .filter_map(|(key, value)| safe_attribute_value(key, value))
        .collect()
}

fn is_allowed_attribute_key(key: &str) -> bool {
    ALLOWED_ATTRIBUTE_KEYS.contains(&key)
        || ALLOWED_ATTRIBUTE_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

fn safe_attribute_value(key: &str, value: &Value) -> Option<KeyValue> {
    match value {
        Value::Bool(value) => Some(KeyValue::new(key.to_string(), *value)),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Some(KeyValue::new(key.to_string(), value))
            } else if let Some(value) = value.as_u64().and_then(|value| i64::try_from(value).ok()) {
                Some(KeyValue::new(key.to_string(), value))
            } else {
                value
                    .as_f64()
                    .map(|value| KeyValue::new(key.to_string(), value))
            }
        }
        Value::String(value) => Some(KeyValue::new(
            key.to_string(),
            truncate_attribute_string(value).to_string(),
        )),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn truncate_attribute_string(value: &str) -> &str {
    if value.len() <= MAX_ATTRIBUTE_STRING_BYTES {
        return value;
    }

    let mut end = MAX_ATTRIBUTE_STRING_BYTES;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::Value as OtelValue;
    use opentelemetry::trace::SpanId;
    use opentelemetry::trace::TraceId;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use opentelemetry_sdk::trace::SpanData;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;

    #[test]
    fn valid_span_telemetry_reconstructs_otel_span_with_sanitized_attrs() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("codex-otel-tests");
        let trace_id = "00000000000000000000000000000001";
        let parent_span_id = "0000000000000002";

        let record = parse_span_telemetry_record(serde_json::json!({
            "v": 1,
            "type": "span",
            "name": "browser_use.tab.goto",
            "traceparent": format!("00-{trace_id}-{parent_span_id}-01"),
            "start_unix_nanos": 1_000_000_123u64,
            "end_unix_nanos": 2_000_000_456u64,
            "attrs": {
                "browser_use.url": "https://example.com",
                "browser_use.timeout_ms": 2500,
                "unknown.secret": "drop me",
                "browser_use.object": {"drop": true}
            }
        }))
        .expect("valid record");

        emit_span_telemetry_record_with_tracer(&tracer, &record).expect("span emitted");
        provider.force_flush().expect("flush spans");
        let spans = exporter.get_finished_spans().expect("finished spans");
        assert_eq!(spans.len(), 1);
        let span = &spans[0];

        assert_eq!(span.name.as_ref(), "browser_use.tab.goto");
        assert_eq!(
            span.span_context.trace_id(),
            TraceId::from_hex(trace_id).unwrap()
        );
        assert_eq!(
            span.parent_span_id,
            SpanId::from_hex(parent_span_id).unwrap()
        );
        assert_eq!(
            span.start_time,
            SystemTime::UNIX_EPOCH + Duration::new(1, 123)
        );
        assert_eq!(
            span.end_time,
            SystemTime::UNIX_EPOCH + Duration::new(2, 456)
        );

        let attrs = span
            .attributes
            .iter()
            .map(|kv| (kv.key.as_str().to_string(), kv.value.clone()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            attrs.get("browser_use.url"),
            Some(&OtelValue::String("https://example.com".into()))
        );
        assert_eq!(
            attrs.get("browser_use.timeout_ms"),
            Some(&OtelValue::I64(2500))
        );
        assert!(!attrs.contains_key("unknown.secret"));
        assert!(!attrs.contains_key("browser_use.object"));
    }

    #[test]
    fn explicit_ids_reconstruct_span_and_parent_ids() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("codex-otel-tests");
        let trace_id = "00000000000000000000000000000001";
        let parent_span_id = "0000000000000002";
        let span_id = "0000000000000010";

        let record = parse_span_telemetry_record(serde_json::json!({
            "v": 1,
            "type": "span",
            "name": "node_repl.js",
            "trace_id": trace_id,
            "span_id": span_id,
            "parent_span_id": parent_span_id,
            "trace_flags": "01",
            "tracestate": "vendor=value",
            "start_unix_nanos": 1_000_000_123u64,
            "end_unix_nanos": 2_000_000_456u64,
        }))
        .expect("valid record");

        emit_span_telemetry_record_with_tracer(&tracer, &record).expect("span emitted");
        provider.force_flush().expect("flush spans");
        let spans = exporter.get_finished_spans().expect("finished spans");
        assert_eq!(spans.len(), 1);
        let span = &spans[0];

        assert_eq!(
            span.span_context.trace_id(),
            TraceId::from_hex(trace_id).unwrap()
        );
        assert_eq!(
            span.span_context.span_id(),
            SpanId::from_hex(span_id).unwrap()
        );
        assert_eq!(
            span.parent_span_id,
            SpanId::from_hex(parent_span_id).unwrap()
        );
        assert!(span.span_context.trace_flags().is_sampled());
    }

    #[test]
    fn child_span_can_parent_to_previous_reconstructed_span_id() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("codex-otel-tests");
        let trace_id = "00000000000000000000000000000001";
        let mcp_span_id = "0000000000000002";
        let node_span_id = "0000000000000010";
        let child_span_id = "0000000000000011";

        for payload in [
            serde_json::json!({
                "v": 1,
                "type": "span",
                "name": "node_repl.js",
                "trace_id": trace_id,
                "span_id": node_span_id,
                "parent_span_id": mcp_span_id,
                "trace_flags": "01",
                "start_unix_nanos": 1_000_000_000u64,
                "end_unix_nanos": 3_000_000_000u64,
            }),
            serde_json::json!({
                "v": 1,
                "type": "span",
                "name": "browser_use.tab.goto",
                "trace_id": trace_id,
                "span_id": child_span_id,
                "parent_span_id": node_span_id,
                "trace_flags": "01",
                "start_unix_nanos": 1_500_000_000u64,
                "end_unix_nanos": 2_000_000_000u64,
            }),
        ] {
            let record = parse_span_telemetry_record(payload).expect("valid record");
            emit_span_telemetry_record_with_tracer(&tracer, &record).expect("span emitted");
        }

        provider.force_flush().expect("flush spans");
        let spans = exporter.get_finished_spans().expect("finished spans");
        let node_span = find_span(&spans, "node_repl.js");
        let child_span = find_span(&spans, "browser_use.tab.goto");

        assert_eq!(
            node_span.span_context.span_id(),
            SpanId::from_hex(node_span_id).unwrap()
        );
        assert_eq!(
            child_span.span_context.span_id(),
            SpanId::from_hex(child_span_id).unwrap()
        );
        assert_eq!(
            child_span.parent_span_id,
            SpanId::from_hex(node_span_id).unwrap()
        );
    }

    #[test]
    fn invalid_span_telemetry_is_rejected_without_emitting_span() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("codex-otel-tests");

        let error = parse_span_telemetry_record(serde_json::json!({
            "v": 99,
            "type": "span",
        }))
        .expect_err("unsupported version");
        assert!(matches!(
            error,
            StderrSpanTelemetryError::UnsupportedVersion
        ));

        assert!(
            emit_span_telemetry_record_with_tracer(
                &tracer,
                &SpanTelemetryRecord {
                    name: "browser_use.tab.goto".to_string(),
                    span_id: None,
                    trace_id: None,
                    parent_span_id: None,
                    trace_flags: None,
                    traceparent: Some("not-a-traceparent".to_string()),
                    tracestate: None,
                    start_time: SystemTime::UNIX_EPOCH,
                    end_time: SystemTime::UNIX_EPOCH,
                    attributes: Vec::new(),
                },
            )
            .is_err()
        );
        provider.force_flush().expect("flush spans");
        assert!(
            exporter
                .get_finished_spans()
                .expect("finished spans")
                .is_empty()
        );
    }

    #[test]
    fn invalid_explicit_ids_are_rejected_without_emitting_span() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let tracer = provider.tracer("codex-otel-tests");

        for payload in [
            serde_json::json!({
                "v": 1,
                "type": "span",
                "name": "node_repl.js",
                "trace_id": "00000000000000000000000000000000",
                "span_id": "0000000000000010",
                "parent_span_id": "0000000000000002",
                "trace_flags": "01",
                "start_unix_nanos": 1_000_000_000u64,
                "end_unix_nanos": 2_000_000_000u64,
            }),
            serde_json::json!({
                "v": 1,
                "type": "span",
                "name": "node_repl.js",
                "trace_id": "00000000000000000000000000000001",
                "span_id": "0000000000000000",
                "parent_span_id": "0000000000000002",
                "trace_flags": "01",
                "start_unix_nanos": 1_000_000_000u64,
                "end_unix_nanos": 2_000_000_000u64,
            }),
            serde_json::json!({
                "v": 1,
                "type": "span",
                "name": "node_repl.js",
                "trace_id": "00000000000000000000000000000001",
                "span_id": "0000000000000010",
                "parent_span_id": "0002",
                "trace_flags": "01",
                "start_unix_nanos": 1_000_000_000u64,
                "end_unix_nanos": 2_000_000_000u64,
            }),
            serde_json::json!({
                "v": 1,
                "type": "span",
                "name": "node_repl.js",
                "trace_id": "00000000000000000000000000000001",
                "span_id": "0000000000000010",
                "parent_span_id": "0000000000000002",
                "trace_flags": "001",
                "start_unix_nanos": 1_000_000_000u64,
                "end_unix_nanos": 2_000_000_000u64,
            }),
        ] {
            let error = parse_span_telemetry_record(payload).expect_err("invalid id");
            assert!(matches!(error, StderrSpanTelemetryError::InvalidField(_)));
        }

        assert!(
            emit_node_repl_stderr_span_telemetry(serde_json::json!({
                "v": 1,
                "type": "span",
                "name": "node_repl.js",
                "trace_id": "00000000000000000000000000000001",
                "span_id": "0000000000000010",
                "parent_span_id": "0000000000000002",
                "trace_flags": "zz",
                "start_unix_nanos": 1_000_000_000u64,
                "end_unix_nanos": 2_000_000_000u64,
            }))
            .is_err()
        );
        provider.force_flush().expect("flush spans");
        assert!(
            exporter
                .get_finished_spans()
                .expect("finished spans")
                .is_empty()
        );
        drop(tracer);
    }

    fn find_span<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
        spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("missing span {name}"))
    }
}
