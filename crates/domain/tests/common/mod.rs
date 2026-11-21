//! Общие помощники тестов: исходная выгрузка из 44 инцидентов и правки записей.
#![allow(dead_code)]

use serde_json::{Map, Value};

/// Исходная выгрузка `testdata/character_problems.json`. Не `data/…`: ту копию меняет
/// `tools/demo.py`, и тесты зависели бы от шага демо.
pub const SAMPLE_JSON: &str = include_str!("../../../../testdata/character_problems.json");

pub fn sample() -> Vec<Map<String, Value>> {
    match serde_json::from_str(SAMPLE_JSON).expect("character_problems.json") {
        Value::Array(items) => items
            .into_iter()
            .map(|v| match v {
                Value::Object(m) => m,
                _ => panic!("ожидался объект"),
            })
            .collect(),
        _ => panic!("ожидался массив"),
    }
}

pub fn values(records: Vec<Map<String, Value>>) -> Value {
    Value::Array(records.into_iter().map(Value::Object).collect())
}

/// Запись из правки базовой (формат `gen_stage5.py::encode`).
pub fn decode(spec: &Value, sample: &[Map<String, Value>]) -> Value {
    if let Some(value) = spec.get("value") {
        return value.clone();
    }
    let base = spec["base"].as_u64().expect("base") as usize;
    let mut record = sample[base].clone();
    if let Some(unset) = spec.get("unset").and_then(Value::as_array) {
        for key in unset {
            record.shift_remove(key.as_str().unwrap());
        }
    }
    if let Some(set) = spec.get("set").and_then(Value::as_object) {
        for (k, v) in set {
            record.insert(k.clone(), v.clone());
        }
    }
    if let Some(order) = spec.get("order").and_then(Value::as_array) {
        let mut reordered = Map::new();
        for key in order {
            let key = key.as_str().unwrap();
            reordered.insert(key.to_owned(), record[key].clone());
        }
        record = reordered;
    }
    Value::Object(record)
}
