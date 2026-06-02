use std::collections::BTreeSet;

use serde_json::Value;

use crate::protocol::ReasoningEffort;

pub fn collect_effort_strings(values: &[String], out: &mut Vec<ReasoningEffort>) {
    for value in values {
        if let Some(effort) = ReasoningEffort::parse(value) {
            out.push(effort);
        }
    }
}

pub fn collect_effort_values(value: &Value, out: &mut Vec<ReasoningEffort>) {
    match value {
        Value::String(value) => {
            if let Some(effort) = ReasoningEffort::parse(value) {
                out.push(effort);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_effort_values(value, out);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_effort_values(value, out);
            }
        }
        _ => {}
    }
}

pub fn collect_parameter_names(value: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    collect_parameter_names_into(value, &mut out);
    out
}

fn collect_parameter_names_into(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::String(value) => {
            out.insert(value.to_ascii_lowercase());
        }
        Value::Array(values) => {
            for value in values {
                collect_parameter_names_into(value, out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                if !matches!(value, Value::Bool(false) | Value::Null) {
                    out.insert(key.to_ascii_lowercase());
                }
                collect_parameter_names_into(value, out);
            }
        }
        _ => {}
    }
}

pub fn collect_nested_efforts(value: &Value, out: &mut Vec<ReasoningEffort>) {
    match value {
        Value::String(value) => {
            if let Some(effort) = ReasoningEffort::parse(value) {
                out.push(effort);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_nested_efforts(value, out);
            }
        }
        Value::Object(map) => {
            for (key, value) in map {
                let key = key.to_ascii_lowercase();
                if key.contains("effort") || key.contains("level") {
                    collect_nested_efforts(value, out);
                }
            }
        }
        _ => {}
    }
}

pub fn value_signals_reasoning(value: &Value) -> bool {
    match value {
        Value::Bool(_) => false,
        Value::String(value) => {
            let value = value.to_ascii_lowercase();
            value == "reasoning" || value == "reasoning_effort" || value == "thinking"
        }
        Value::Array(values) => values.iter().any(value_signals_reasoning),
        Value::Object(map) => map.iter().any(|(key, value)| {
            let key = key.to_ascii_lowercase();
            let key_signals_reasoning = (key.contains("reasoning") || key.contains("thinking"))
                && !matches!(value, Value::Bool(false) | Value::Null);
            key_signals_reasoning || value_signals_reasoning(value)
        }),
        _ => false,
    }
}

pub fn value_signals_reasoning_summary(value: &Value) -> bool {
    match value {
        Value::String(value) => value.to_ascii_lowercase().contains("summary"),
        Value::Array(values) => values.iter().any(value_signals_reasoning_summary),
        Value::Object(map) => map.iter().any(|(key, value)| {
            let key_signals_summary = key.to_ascii_lowercase().contains("summary")
                && !matches!(value, Value::Bool(false) | Value::Null);
            key_signals_summary || value_signals_reasoning_summary(value)
        }),
        _ => false,
    }
}

pub fn dedupe_efforts(efforts: &mut Vec<ReasoningEffort>) {
    let mut seen = BTreeSet::new();
    efforts.retain(|effort| seen.insert(effort.ordinal()));
}
