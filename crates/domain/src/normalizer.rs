//! Нормализация одной записи выгрузки МТС — порт `app/importer/normalizer.py`.
//!
//! Одна таблица полей (макрос `fields!`) описывает каждое известное поле: имя в выгрузке,
//! колонку `incidents` и тип. Из неё получаются:
//! * [`Columns`] — типизированные значения колонок;
//! * `tracked` — нормализованные значения всех полей записи (известных и новых) для сравнения
//!   версий: пробелы по краям, `""` vs `null`, формат даты не создают ложных изменений.
//!
//! Исходная запись (`raw`) сохраняется без изменений. `content_hash` совпадает с Python байт в байт.

use std::fmt;

use chrono::{DateTime, NaiveDateTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::constants::NormalizedState;
use crate::pycompat::{self, number_as_int};
use crate::pydatetime::fromisoformat;

/// Тип поля выгрузки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Key,
    Str,
    Int,
    Bool,
    Datetime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    pub source: &'static str,
    pub column: &'static str,
    pub kind: FieldKind,
}

/// Ошибка типа в известном поле — снимок отклоняется.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{field}: {message}")]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

macro_rules! column_type {
    (Key) => { Option<String> };
    (Str) => { Option<String> };
    (Int) => { Option<i64> };
    (Bool) => { Option<bool> };
    (Datetime) => { Option<DateTime<Utc>> };
}

/// Значение колонки `incidents` для привязки к SQL-параметру.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnValue<'a> {
    Text(Option<&'a str>),
    Int(Option<i64>),
    Bool(Option<bool>),
    Time(Option<DateTime<Utc>>),
}

macro_rules! column_value {
    (Key, $v:expr) => {
        ColumnValue::Text($v.as_deref())
    };
    (Str, $v:expr) => {
        ColumnValue::Text($v.as_deref())
    };
    (Int, $v:expr) => {
        ColumnValue::Int(*$v)
    };
    (Bool, $v:expr) => {
        ColumnValue::Bool(*$v)
    };
    (Datetime, $v:expr) => {
        ColumnValue::Time(*$v)
    };
}

macro_rules! fields {
    ($( $source:literal => $column:ident : $kind:ident ),+ $(,)?) => {
        /// Известные поля выгрузки в порядке `FIELDS` Python-версии.
        pub const FIELDS: &[FieldSpec] = &[
            $( FieldSpec { source: $source, column: stringify!($column), kind: FieldKind::$kind } ),+
        ];

        /// Значения колонок `incidents` (словарь `columns` Python-версии).
        #[derive(Debug, Clone, PartialEq)]
        pub struct Columns {
            $( pub $column: column_type!($kind), )+
            pub normalized_state: NormalizedState,
            pub status_mismatch: bool,
        }

        impl Columns {
            /// Все колонки NULL (отсутствующие в записи поля).
            pub fn empty() -> Self {
                Self {
                    $( $column: None, )+
                    normalized_state: NormalizedState::Unknown,
                    status_mismatch: false,
                }
            }

            /// Значения колонок в порядке `FIELDS` — для SQL импортёра (`store`).
            pub fn values(&self) -> Vec<(&'static str, ColumnValue<'_>)> {
                vec![ $( (stringify!($column), column_value!($kind, &self.$column)) ),+ ]
            }

            /// Нормализует известное поле и записывает колонку; возвращает `tracked`-значение.
            /// `None` — поле не из таблицы.
            fn apply(&mut self, source: &str, value: &Value, tz: Tz) -> Option<Result<Value, String>> {
                match source {
                    $( $source => Some(apply_value!($kind, self.$column, value, tz)), )+
                    _ => None,
                }
            }
        }
    };
}

macro_rules! apply_value {
    (Key, $slot:expr, $value:expr, $tz:expr) => {
        apply_value!(Str, $slot, $value, $tz)
    };
    (Str, $slot:expr, $value:expr, $tz:expr) => {
        normalize_text($value).map(|v| {
            let tracked = v.clone().map_or(Value::Null, Value::String);
            $slot = v;
            tracked
        })
    };
    (Int, $slot:expr, $value:expr, $tz:expr) => {
        normalize_int($value).map(|v| {
            $slot = v;
            v.map_or(Value::Null, Value::from)
        })
    };
    (Bool, $slot:expr, $value:expr, $tz:expr) => {
        normalize_bool($value).map(|v| {
            $slot = v;
            v.map_or(Value::Null, Value::Bool)
        })
    };
    (Datetime, $slot:expr, $value:expr, $tz:expr) => {
        parse_source_datetime($value, $tz).map(|v| {
            $slot = v;
            v.map_or(Value::Null, |d| Value::String(iso_z(&d)))
        })
    };
}

fields! {
    "sysId" => sys_id: Key,
    "id" => id: Key,
    "state" => state_label: Str,
    "stateEnum" => state_enum: Str,
    "stateId" => state_id: Int,
    "stateReason" => state_reason: Str,
    "stateReasonId" => state_reason_id: Key,
    "submitDate" => submit_date: Datetime,
    "deadline" => deadline: Datetime,
    "completionDate" => completion_date: Datetime,
    "lastAssignmentToTeamDateTime" => last_assignment_to_team_at: Datetime,
    "header" => header: Str,
    "description" => description: Str,
    "shortDescription" => short_description: Str,
    "assignedGroupName" => assigned_group_name: Str,
    "assignedEmployee" => assigned_employee: Str,
    "assignedEmployeeLogin" => assigned_employee_login: Str,
    "initiatorName" => initiator_name: Str,
    "initiatorLogin" => initiator_login: Str,
    "isEffect" => is_effect: Bool,
    "isReason" => is_reason: Bool,
    "isDouble" => is_double: Bool,
    "isMaster" => is_master: Bool,
    "incType" => inc_type: Str,
    "serviceTypeId" => service_type_id: Int,
    "serviceType" => service_type: Str,
    "priorityId" => priority_id: Int,
    "priority" => priority: Str,
    "urgencyId" => urgency_id: Int,
    "urgency" => urgency: Str,
    "impactId" => impact_id: Int,
    "impact" => impact: Str,
    "waitingExitDate" => waiting_exit_date: Datetime,
    "isRootIncident" => is_root_incident: Bool,
    "productName" => product_name: Str,
    "productId" => product_id: Key,
    "productMasterId" => product_master_id: Str,
    "productUrl" => product_url: Str,
    "service" => service: Str,
    "serviceId" => service_id: Key,
    "scenarioName" => scenario_name: Str,
    "isNeedUp" => is_need_up: Bool,
    "eliminationType" => elimination_type: Str,
    "eliminationTypeId" => elimination_type_id: Key,
    "receivingChannel" => receiving_channel: Str,
    "solution" => solution: Str,
}

/// Поля-даты (`DATETIME_FIELDS`).
pub fn datetime_fields() -> impl Iterator<Item = &'static str> {
    FIELDS
        .iter()
        .filter(|f| f.kind == FieldKind::Datetime)
        .map(|f| f.source)
}

/// Спецификация поля по имени в выгрузке (`FIELDS_BY_SOURCE`).
pub fn field_by_source(source: &str) -> Option<&'static FieldSpec> {
    FIELDS.iter().find(|f| f.source == source)
}

const TRUE_WORDS: [&str; 4] = ["true", "да", "yes", "1"];
const FALSE_WORDS: [&str; 4] = ["false", "нет", "no", "0"];

// ---------------------------------------------------------------- примитивы

/// Строка/ключ: обрезка пробелов, пустая строка → `None`; числа и логические — их `str()`.
pub fn normalize_text(value: &Value) -> Result<Option<String>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(if *b { "true" } else { "false" }.to_owned())),
        Value::Number(n) => Ok(Some(pycompat::number_str(n))),
        Value::String(s) => {
            let s = pycompat::strip(s);
            Ok((!s.is_empty()).then(|| s.to_owned()))
        }
        _ => Err("ожидалась строка".into()),
    }
}

/// Целое: `int`, целочисленный `float` или строка из цифр (с минусом).
///
/// Отличие от Python: строки вроде `"--5"` или `"²"` Python пропускал проверкой `isdigit()`
/// и падал на `int()` необработанным `ValueError`; здесь это обычная ошибка поля.
/// Значения вне `i64` тоже ошибка (Python отклонил бы их уже при записи в БД).
pub fn normalize_int(value: &Value) -> Result<Option<i64>, String> {
    const MSG: &str = "ожидалось целое число";
    match value {
        Value::Null => Ok(None),
        Value::String(s) if pycompat::strip(s).is_empty() => Ok(None),
        Value::Bool(_) => Err("ожидалось целое число, получено логическое значение".into()),
        Value::Number(n) => {
            if let Some(i) = number_as_int(n) {
                return i64::try_from(i).map(Some).map_err(|_| MSG.into());
            }
            let f = n.as_f64().unwrap_or(f64::NAN);
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18 {
                Ok(Some(f as i64))
            } else {
                Err(MSG.into())
            }
        }
        Value::String(s) => {
            // `value.strip().lstrip("-").isdigit()` → `int(value.strip())`; цифры — любые
            // десятичные Unicode (`int("١٢") == 12`).
            let s = pycompat::strip(s);
            let body = s.trim_start_matches('-');
            if body.is_empty() || s.len() - body.len() > 1 {
                return Err(MSG.into());
            }
            let mut value: i128 = 0;
            for c in body.chars() {
                let digit = pycompat::decimal_digit(c).ok_or_else(|| MSG.to_owned())?;
                value = value
                    .checked_mul(10)
                    .and_then(|v| v.checked_add(i128::from(digit)))
                    .ok_or_else(|| MSG.to_owned())?;
            }
            let value = if s.starts_with('-') { -value } else { value };
            i64::try_from(value).map(Some).map_err(|_| MSG.into())
        }
        _ => Err(MSG.into()),
    }
}

/// Логическое: `true/false` или строки «да/нет», «yes/no», «1/0» (без учёта регистра).
pub fn normalize_bool(value: &Value) -> Result<Option<bool>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        Value::String(s) => {
            let low = pycompat::strip(s).to_lowercase();
            if low.is_empty() {
                Ok(None)
            } else if TRUE_WORDS.contains(&low.as_str()) {
                Ok(Some(true))
            } else if FALSE_WORDS.contains(&low.as_str()) {
                Ok(Some(false))
            } else {
                Err("ожидалось true/false или «Да»/«Нет»".into())
            }
        }
        _ => Err("ожидалось true/false или «Да»/«Нет»".into()),
    }
}

/// Дата-время в UTC. ISO 8601 (`2026-11-08T12:02:29Z`, микросекунды, смещение) или
/// `YYYY-MM-DD HH:MM:SS` — всё, что принимает `datetime.fromisoformat` Python 3.12.
///
/// Дата-заглушка `0001-01-01…` и любая дата 1-го года → `None`. Дата без зоны трактуется в `naive_tz`.
pub fn parse_source_datetime(value: &Value, naive_tz: Tz) -> Result<Option<DateTime<Utc>>, String> {
    let text = match value {
        Value::Null => return Ok(None),
        Value::String(s) => pycompat::strip(s),
        _ => return Err("ожидалась дата строкой ISO 8601".into()),
    };
    if text.is_empty() || text.starts_with("0001-01-01") {
        return Ok(None);
    }
    let bad = || format!("нераспознаваемая дата «{}»", pycompat::take_chars(text, 40));
    let parsed = fromisoformat(text).map_err(|_| bad())?;
    if chrono::Datelike::year(&parsed.naive) <= 1 {
        return Ok(None);
    }
    let utc = match parsed.offset_us {
        Some(offset) => parsed.naive - TimeDelta::microseconds(offset),
        None => localize(parsed.naive, naive_tz),
    };
    // Python: OverflowError при выходе за 1..9999 после перевода в UTC (там — необработанный).
    if !(1..=9999).contains(&chrono::Datelike::year(&utc)) {
        return Err(bad());
    }
    Ok(Some(utc.and_utc()))
}

/// `naive.replace(tzinfo=ZoneInfo(...))` → UTC: `fold=0` — при неоднозначности первое из двух
/// времён, в «пропущенном» часе — смещение до перехода.
fn localize(naive: NaiveDateTime, tz: Tz) -> NaiveDateTime {
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.naive_utc(),
        chrono::LocalResult::Ambiguous(first, _) => first.naive_utc(),
        chrono::LocalResult::None => {
            let before = tz.offset_from_utc_datetime(&(naive - TimeDelta::days(1)));
            naive - TimeDelta::seconds(i64::from(chrono::Offset::fix(&before).local_minus_utc()))
        }
    }
}

/// `datetime.isoformat().replace("+00:00", "Z")` для UTC: микросекунды — только если не ноль.
pub fn iso_z(value: &DateTime<Utc>) -> String {
    let micros = value.timestamp_subsec_micros();
    if micros == 0 {
        value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    } else {
        format!("{}.{micros:06}Z", value.format("%Y-%m-%dT%H:%M:%S"))
    }
}

/// Для полей, которых нет в таблице: только обрезка пробелов, рекурсивно.
pub fn normalize_generic(value: &Value) -> Value {
    match value {
        Value::String(s) => {
            let s = pycompat::strip(s);
            if s.is_empty() {
                Value::Null
            } else {
                Value::String(s.to_owned())
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize_generic).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), normalize_generic(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------- статус

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateResolution {
    pub state: NormalizedState,
    pub recognized: bool,
    pub mismatch: bool,
    pub warning: Option<String>,
}

/// Python-представление `None` в f-строках.
struct Py<'a, T>(&'a Option<T>);

impl<T: fmt::Display> fmt::Display for Py<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(v) => v.fmt(f),
            None => f.write_str("None"),
        }
    }
}

/// `stateEnum` — основной источник; при его отсутствии — таблица кодов, сверенная с выгрузкой.
pub fn resolve_state(state_enum: Option<&str>, state_id: Option<i64>) -> StateResolution {
    let by_code = state_id.and_then(NormalizedState::from_state_id);
    let by_enum = state_enum.and_then(|e| {
        let low = e.to_lowercase();
        NormalizedState::ALL
            .iter()
            .copied()
            .filter(|s| *s != NormalizedState::Unknown)
            .find(|s| s.as_str().to_lowercase() == low)
    });
    let enum_text = Py(&state_enum.map(str::to_owned));
    let id_text = Py(&state_id);
    if let Some(state) = by_enum {
        let mismatch = by_code.is_some_and(|c| c != state);
        let warning = mismatch.then(|| {
            format!("stateEnum «{enum_text}» не соответствует stateId {id_text}; использован stateEnum")
        });
        return StateResolution {
            state,
            recognized: true,
            mismatch,
            warning,
        };
    }
    if let Some(state) = by_code {
        let warning = state_enum
            .map(|_| format!("неизвестный stateEnum «{enum_text}», статус определён по stateId {id_text}"));
        return StateResolution {
            state,
            recognized: true,
            mismatch: false,
            warning,
        };
    }
    StateResolution {
        state: NormalizedState::Unknown,
        recognized: false,
        mismatch: false,
        warning: Some(format!(
            "нераспознанный статус (stateEnum «{enum_text}», stateId {id_text}) — исключён из групп"
        )),
    }
}

// ---------------------------------------------------------------- запись

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedIncident {
    /// `id` после обрезки пробелов. Непустой после проверки валидатором.
    pub id: String,
    pub sys_id: String,
    pub raw: Map<String, Value>,
    pub columns: Columns,
    pub tracked: Map<String, Value>,
    pub content_hash: String,
    pub state: StateResolution,
    pub warnings: Vec<String>,
}

impl NormalizedIncident {
    pub fn priority_id(&self) -> Option<i64> {
        self.columns.priority_id
    }

    pub fn header(&self) -> Option<&str> {
        self.columns.header.as_deref()
    }
}

/// SHA-256 канонического JSON `tracked` (как `json.dumps(sort_keys=True, ensure_ascii=False, …)`).
pub fn tracked_hash(tracked: &Map<String, Value>) -> String {
    hex::encode(Sha256::digest(
        pycompat::dumps_canonical_object(tracked).as_bytes(),
    ))
}

/// Колонки и `tracked` без владения записью (для сравнения с сохранённой версией).
pub fn normalize_fields(
    raw: &Map<String, Value>,
    naive_tz: Tz,
) -> Result<(Columns, Map<String, Value>, StateResolution), FieldError> {
    let mut columns = Columns::empty();
    let mut tracked = Map::new();
    for (key, value) in raw {
        let normalized = match columns.apply(key, value, naive_tz) {
            None => normalize_generic(value),
            Some(Ok(v)) => v,
            Some(Err(message)) => {
                return Err(FieldError {
                    field: key.clone(),
                    message,
                });
            }
        };
        tracked.insert(key.clone(), normalized);
    }
    let state = resolve_state(columns.state_enum.as_deref(), columns.state_id);
    columns.normalized_state = state.state;
    columns.status_mismatch = state.mismatch;
    Ok((columns, tracked, state))
}

/// Нормализует запись. Ошибку типа в известном поле сообщает как [`FieldError`] (снимок отклоняется).
pub fn normalize_record(raw: Map<String, Value>, naive_tz: Tz) -> Result<NormalizedIncident, FieldError> {
    let (columns, tracked, state) = normalize_fields(&raw, naive_tz)?;
    let content_hash = tracked_hash(&tracked);
    let warnings = state.warning.iter().cloned().collect();
    Ok(NormalizedIncident {
        id: columns.id.clone().unwrap_or_default(),
        sys_id: columns.sys_id.clone().unwrap_or_default(),
        raw,
        columns,
        tracked,
        content_hash,
        state,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn fields_table_matches_python_order() {
        assert_eq!(FIELDS.len(), 46);
        assert_eq!(FIELDS[0].source, "sysId");
        assert_eq!(FIELDS[45].column, "solution");
        assert_eq!(
            datetime_fields().collect::<Vec<_>>(),
            [
                "submitDate",
                "deadline",
                "completionDate",
                "lastAssignmentToTeamDateTime",
                "waitingExitDate"
            ]
        );
        assert_eq!(field_by_source("priorityId").unwrap().kind, FieldKind::Int);
    }

    #[test]
    fn primitives() {
        assert_eq!(normalize_text(&json!("  a ")).unwrap().as_deref(), Some("a"));
        assert_eq!(normalize_text(&json!(1.5)).unwrap().as_deref(), Some("1.5"));
        assert_eq!(normalize_text(&json!(true)).unwrap().as_deref(), Some("true"));
        assert!(normalize_text(&json!([1])).is_err());
        assert_eq!(normalize_int(&json!(" -7 ")).unwrap(), Some(-7));
        assert_eq!(normalize_int(&json!(4.0)).unwrap(), Some(4));
        assert!(normalize_int(&json!("--5")).is_err());
        assert!(normalize_int(&json!(4.5)).is_err());
        assert_eq!(normalize_bool(&json!(" ДА ")).unwrap(), Some(true));
        assert!(normalize_bool(&json!(1)).is_err());
    }

    #[test]
    fn iso_z_drops_zero_micros() {
        let d = Utc.with_ymd_and_hms(2026, 11, 10, 12, 32, 14).unwrap();
        assert_eq!(iso_z(&d), "2026-11-10T12:32:14Z");
        assert_eq!(
            iso_z(&(d + TimeDelta::microseconds(5))),
            "2026-11-10T12:32:14.000005Z"
        );
    }

    #[test]
    fn gap_and_fold_follow_zoneinfo() {
        let berlin: Tz = "Europe/Berlin".parse().unwrap();
        // Пропущенный час 2026-03-29 02:30 — смещение до перехода (+01:00).
        let gap = parse_source_datetime(&json!("2026-03-29 02:30:00"), berlin)
            .unwrap()
            .unwrap();
        assert_eq!(iso_z(&gap), "2026-03-29T01:30:00Z");
        // Повторяющийся час 2026-10-25 02:30 — первое (летнее, +02:00).
        let fold = parse_source_datetime(&json!("2026-10-25 02:30:00"), berlin)
            .unwrap()
            .unwrap();
        assert_eq!(iso_z(&fold), "2026-10-25T00:30:00Z");
    }
}
