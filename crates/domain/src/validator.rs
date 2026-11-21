//! Проверка полного снимка — порт `app/importer/validator.py`.
//! Любая ошибка отклоняет снимок целиком (ARCHITECTURE.md).
//!
//! Проверяется: корректный JSON-массив объектов; непустые строковые `id` и `sysId`, уникальные
//! в снимке; корректная `submitDate`; наличие статуса (`stateEnum` или `stateId`);
//! `priorityId` ∈ {1, 2, 3, 4}; типы известных полей и формат всех непустых дат (решение C8).
//! Нераспознанный, но присутствующий статус снимок не отклоняет (C13/C25) — это предупреждение.
//!
//! Тексты ошибок совпадают с Python, включая синтаксические ошибки JSON: позицию и описание
//! (`некорректный JSON: строка 1, позиция 15 (Expecting value)`) восстанавливает
//! [`crate::pyjson`] — порт сканера `json` CPython 3.12. Исключение — тексты, которые CPython
//! принимает, а `serde_json` нет (одиночный суррогат `\ud800`, `1e400`): там описание `serde_json`.

use std::collections::HashMap;
use std::fmt;

use chrono_tz::Tz;

use crate::pyjson::{self, PyJsonError};
use serde_json::{Map, Value};

use crate::constants::VALID_PRIORITIES;
use crate::normalizer::{NormalizedIncident, normalize_record};
use crate::pycompat::{self, number_as_int, py_repr, type_name};

/// Сколько ошибок перечисляется в сводке.
pub const MAX_REPORTED_ERRORS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ValidationIssue {
    pub message: String,
    /// Номер записи с нуля.
    pub index: Option<usize>,
    pub incident_id: Option<String>,
    pub field: Option<String>,
}

impl ValidationIssue {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            ..Self::default()
        }
    }

    fn at(message: impl Into<String>, index: usize, incident_id: Option<&str>, field: &str) -> Self {
        Self {
            message: message.into(),
            index: Some(index),
            incident_id: incident_id.map(str::to_owned),
            field: Some(field.to_owned()),
        }
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut where_ = Vec::new();
        if let Some(index) = self.index {
            where_.push(format!("запись #{}", index + 1));
        }
        if let Some(id) = self.incident_id.as_deref().filter(|s| !s.is_empty()) {
            where_.push(id.to_owned());
        }
        if let Some(field) = self.field.as_deref().filter(|s| !s.is_empty()) {
            where_.push(field.to_owned());
        }
        if where_.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", where_.join(", "), self.message)
        }
    }
}

/// Снимок отклонён. `Display` — сводка для `source_state.last_error`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", self.summary())]
pub struct SnapshotRejected {
    pub issues: Vec<ValidationIssue>,
}

impl SnapshotRejected {
    fn one(issue: ValidationIssue) -> Self {
        Self { issues: vec![issue] }
    }

    pub fn summary(&self) -> String {
        let shown: Vec<String> = self
            .issues
            .iter()
            .take(MAX_REPORTED_ERRORS)
            .map(ToString::to_string)
            .collect();
        let mut text = shown.join("; ");
        if self.issues.len() > MAX_REPORTED_ERRORS {
            text.push_str(&format!(
                "; и ещё ошибок: {}",
                self.issues.len() - MAX_REPORTED_ERRORS
            ));
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedSnapshot {
    pub incidents: Vec<NormalizedIncident>,
    pub warnings: Vec<String>,
}

/// Разбор JSON с понятным сообщением об ошибке. BOM допускается; NaN/Infinity запрещены.
pub fn parse_json(content: &[u8]) -> Result<Value, SnapshotRejected> {
    let text = std::str::from_utf8(content).map_err(|e| {
        SnapshotRejected::one(ValidationIssue::new(format!(
            "файл не в кодировке UTF-8 (байт {})",
            e.valid_up_to()
        )))
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if pycompat::strip(text).is_empty() {
        return Err(SnapshotRejected::one(ValidationIssue::new("файл пуст")));
    }
    serde_json::from_str(text).map_err(|e| {
        // Текст и позиция — как у `json.loads` CPython (позиции в символах, не в байтах).
        let message = match pyjson::locate_error(text) {
            Some(PyJsonError::Decode { msg, pos }) => {
                let (line, col) = PyJsonError::line_col(text, pos);
                format!("некорректный JSON: строка {line}, позиция {col} ({msg})")
            }
            Some(PyJsonError::Constant(name)) => {
                format!("некорректный JSON: недопустимое значение {name}")
            }
            // CPython принял бы текст (одиночный суррогат, 1e400, вложенность > 128):
            // остаётся описание serde_json.
            None => {
                let full = e.to_string();
                let msg = full.rsplit_once(" at line ").map_or(full.as_str(), |(m, _)| m);
                format!(
                    "некорректный JSON: строка {}, позиция {} ({msg})",
                    e.line(),
                    e.column()
                )
            }
        };
        SnapshotRejected::one(ValidationIssue::new(message))
    })
}

fn required_issues(index: usize, record: &Map<String, Value>) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let rid = record.get("id").and_then(Value::as_str);

    for key in ["id", "sysId"] {
        let ok = record
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !pycompat::strip(s).is_empty());
        if !ok {
            issues.push(ValidationIssue::at(
                "обязательное поле: непустая строка",
                index,
                rid,
                key,
            ));
        }
    }

    if matches!(record.get("submitDate"), None | Some(Value::Null))
        || record.get("submitDate").and_then(Value::as_str) == Some("")
    {
        issues.push(ValidationIssue::at(
            "обязательное поле отсутствует",
            index,
            rid,
            "submitDate",
        ));
    }

    let enum_missing = match record.get("stateEnum") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => pycompat::strip(s).is_empty(),
        Some(_) => false,
    };
    if enum_missing && matches!(record.get("stateId"), None | Some(Value::Null)) {
        issues.push(ValidationIssue::at(
            "статус отсутствует (нет stateEnum и stateId)",
            index,
            rid,
            "stateEnum",
        ));
    }

    let priority = record.get("priorityId").unwrap_or(&Value::Null);
    let valid = match priority {
        Value::Number(n) => {
            number_as_int(n).is_some_and(|p| VALID_PRIORITIES.iter().any(|v| i128::from(*v) == p))
        }
        _ => false,
    };
    if !valid {
        issues.push(ValidationIssue::at(
            format!("допустимы значения 1–4, получено {}", py_repr(priority)),
            index,
            rid,
            "priorityId",
        ));
    }
    issues
}

/// Проверяет разобранный снимок и нормализует записи.
pub fn validate_snapshot(data: Value, naive_tz: Tz) -> Result<ValidatedSnapshot, SnapshotRejected> {
    let records = match data {
        Value::Array(records) => records,
        other => {
            return Err(SnapshotRejected::one(ValidationIssue::new(format!(
                "ожидался JSON-массив инцидентов, получен {}",
                type_name(&other)
            ))));
        }
    };

    let mut issues = Vec::new();
    let mut normalized: Vec<NormalizedIncident> = Vec::with_capacity(records.len());

    for (index, record) in records.into_iter().enumerate() {
        let record = match record {
            Value::Object(map) => map,
            other => {
                issues.push(ValidationIssue {
                    index: Some(index),
                    ..ValidationIssue::new(format!("ожидался объект, получен {}", type_name(&other)))
                });
                continue;
            }
        };
        let record_issues = required_issues(index, &record);
        if !record_issues.is_empty() {
            issues.extend(record_issues);
            continue;
        }
        let raw_id = record.get("id").and_then(Value::as_str).map(str::to_owned);
        let incident = match normalize_record(record, naive_tz) {
            Ok(incident) => incident,
            Err(e) => {
                issues.push(ValidationIssue::at(e.message, index, raw_id.as_deref(), &e.field));
                continue;
            }
        };
        if incident.columns.submit_date.is_none() {
            issues.push(ValidationIssue::at(
                "дата-заглушка недопустима",
                index,
                Some(&incident.id),
                "submitDate",
            ));
            continue;
        }
        normalized.push(incident);
    }

    for (key, get) in [
        (
            "id",
            (|i: &NormalizedIncident| i.id.as_str()) as fn(&NormalizedIncident) -> &str,
        ),
        ("sysId", |i| i.sys_id.as_str()),
    ] {
        // Counter: порядок — по первому появлению.
        let mut order: Vec<&str> = Vec::new();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for incident in &normalized {
            let value = get(incident);
            let count = counts.entry(value).or_insert(0);
            if *count == 0 {
                order.push(value);
            }
            *count += 1;
        }
        for value in order {
            let count = counts[value];
            if count > 1 {
                issues.push(ValidationIssue {
                    field: Some(key.to_owned()),
                    ..ValidationIssue::new(format!("значение «{value}» повторяется {count} раз(а)"))
                });
            }
        }
    }

    if !issues.is_empty() {
        return Err(SnapshotRejected { issues });
    }

    let warnings = normalized
        .iter()
        .flat_map(|i| i.warnings.iter().map(move |w| format!("{}: {w}", i.id)))
        .collect();
    Ok(ValidatedSnapshot {
        incidents: normalized,
        warnings,
    })
}

pub fn parse_and_validate(content: &[u8], naive_tz: Tz) -> Result<ValidatedSnapshot, SnapshotRejected> {
    validate_snapshot(parse_json(content)?, naive_tz)
}
