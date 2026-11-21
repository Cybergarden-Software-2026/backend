//! Разбор и проверка параметров запроса так, как это делают FastAPI и pydantic 2.
//!
//! * query: `parse_qsl(keep_blank_values=True)` Starlette, при повторе ключа берётся последнее
//!   значение (`+` — пробел, `%XX` — UTF-8 с заменой некорректных байтов);
//! * заголовки: первое значение, байты как latin-1;
//! * ошибки копятся в порядке FastAPI (path → query → header, внутри — в порядке объявления
//!   параметров) и отдаются одним ответом 400 `VALIDATION_ERROR` с `errors: [{loc, msg, type}]` —
//!   тексты `msg` и `type` как у pydantic.

use std::collections::HashMap;

use axum::http::{HeaderMap, Uri};
use dashboard_domain::pycompat;
use serde_json::{Value, json};

use crate::error::AppError;

pub const VALIDATION_MESSAGE: &str = "Некорректные параметры запроса";

/// Байты заголовка как строка latin-1 (так их отдаёт Starlette).
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// Первое значение заголовка (`request.headers.get(name)`).
pub fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|v| latin1(v.as_bytes()))
}

/// Процентное декодирование сегмента пути (`unquote`, UTF-8 с заменой).
pub fn decode_path_segment(segment: &str) -> String {
    let bytes: Vec<u8> = percent_decode(segment.as_bytes());
    String::from_utf8_lossy(&bytes).into_owned()
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let decoded = (input[i] == b'%')
            .then(|| Some((hex(*input.get(i + 1)?)?, hex(*input.get(i + 2)?)?)))
            .flatten();
        match decoded {
            Some((h, l)) => {
                out.push((h << 4) | l);
                i += 3;
            }
            None => {
                out.push(input[i]);
                i += 1;
            }
        }
    }
    out
}

fn enum_message(values: &[&str]) -> String {
    let quoted: Vec<String> = values.iter().map(|v| format!("'{v}'")).collect();
    match quoted.split_last() {
        Some((last, [])) => format!("Input should be {last}"),
        Some((last, rest)) => format!("Input should be {} or {last}", rest.join(", ")),
        None => "Input should be".to_owned(),
    }
}

/// Накопитель ошибок проверки одного запроса.
pub struct Params {
    query: HashMap<String, String>,
    errors: Vec<Value>,
}

impl Params {
    pub fn new(uri: &Uri) -> Self {
        let query = form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
        Self {
            query,
            errors: Vec::new(),
        }
    }

    fn error(&mut self, location: &str, name: &str, msg: String, kind: &str) {
        self.errors
            .push(json!({ "loc": [location, name], "msg": msg, "type": kind }));
    }

    pub fn query_raw(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }

    fn check_int(
        &mut self,
        location: &str,
        name: &str,
        raw: &str,
        ge: i128,
        le: Option<i128>,
    ) -> Option<i128> {
        let Some(value) = pycompat::pydantic_int(raw) else {
            self.error(
                location,
                name,
                "Input should be a valid integer, unable to parse string as an integer".into(),
                "int_parsing",
            );
            return None;
        };
        if value < ge {
            self.error(
                location,
                name,
                format!("Input should be greater than or equal to {ge}"),
                "greater_than_equal",
            );
            return None;
        }
        if let Some(le) = le.filter(|le| value > *le) {
            self.error(
                location,
                name,
                format!("Input should be less than or equal to {le}"),
                "less_than_equal",
            );
            return None;
        }
        Some(value)
    }

    fn check_str(
        &mut self,
        location: &str,
        name: &str,
        raw: String,
        max_len: Option<usize>,
    ) -> Option<String> {
        if let Some(max) = max_len.filter(|max| raw.chars().count() > *max) {
            let unit = if max == 1 { "character" } else { "characters" };
            self.error(
                location,
                name,
                format!("String should have at most {max} {unit}"),
                "string_too_long",
            );
            return None;
        }
        Some(raw)
    }

    /// Целое в пути (`Path(ge=…)`).
    pub fn path_int(&mut self, name: &str, raw: &str, ge: i128) -> Option<i128> {
        self.check_int("path", name, raw, ge, None)
    }

    /// Строка в пути (`Path(min_length=1, max_length=…)`).
    pub fn path_str(&mut self, name: &str, raw: String, max_len: usize) -> Option<String> {
        self.check_str("path", name, raw, Some(max_len))
    }

    /// `int` в query с ограничениями `ge`/`le`; без параметра — `default`.
    pub fn query_int(&mut self, name: &str, ge: i128, le: i128, default: i64) -> i64 {
        match self.query.get(name).cloned() {
            None => default,
            Some(raw) => self
                .check_int("query", name, &raw, ge, Some(le))
                .map_or(default, |v| v as i64),
        }
    }

    pub fn query_opt_int(&mut self, name: &str, ge: i128, le: i128) -> Option<i64> {
        let raw = self.query.get(name).cloned()?;
        self.check_int("query", name, &raw, ge, Some(le))
            .map(|v| v as i64)
    }

    pub fn query_str(&mut self, name: &str, max_len: Option<usize>) -> Option<String> {
        let raw = self.query.get(name).cloned()?;
        self.check_str("query", name, raw, max_len)
    }

    /// Значение из перечня (`Enum`): точное совпадение.
    pub fn query_enum<T: Copy>(&mut self, name: &str, all: &[T], as_str: fn(T) -> &'static str) -> Option<T> {
        let raw = self.query.get(name).cloned()?;
        if let Some(v) = all.iter().copied().find(|v| as_str(*v) == raw) {
            return Some(v);
        }
        let values: Vec<&str> = all.iter().map(|v| as_str(*v)).collect();
        self.error("query", name, enum_message(&values), "enum");
        None
    }

    /// `Literal[...]` в query: как [`Self::query_enum`], но тип ошибки `literal_error`.
    pub fn query_literal(&mut self, name: &str, all: &[&'static str]) -> Option<&'static str> {
        let raw = self.query.get(name).cloned()?;
        if let Some(v) = all.iter().copied().find(|v| *v == raw) {
            return Some(v);
        }
        self.error("query", name, enum_message(all), "literal_error");
        None
    }

    /// Строковый заголовок (`Header(alias=…, max_length=…)`).
    pub fn header_str(&mut self, headers: &HeaderMap, alias: &str, max_len: usize) -> Option<String> {
        let raw = header(headers, alias)?;
        self.check_str("header", alias, raw, Some(max_len))
    }

    /// 400 `VALIDATION_ERROR`, если что-то не прошло проверку.
    #[allow(clippy::result_large_err)] // AppError — тело ответа; путь ошибки редкий
    pub fn finish(self) -> Result<(), AppError> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(AppError::validation(VALIDATION_MESSAGE).with_detail("errors", Value::Array(self.errors)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_messages_like_pydantic() {
        assert_eq!(
            enum_message(&["critical", "coordinators", "high", "research"]),
            "Input should be 'critical', 'coordinators', 'high' or 'research'"
        );
        assert_eq!(enum_message(&["a", "b"]), "Input should be 'a' or 'b'");
        assert_eq!(enum_message(&["a"]), "Input should be 'a'");
    }

    #[test]
    fn query_last_value_wins_and_plus_is_space() {
        let uri: Uri = "/x?limit=0&limit=5&q=a+b%20c&e=&bad=%ZZ".parse().unwrap();
        let p = Params::new(&uri);
        assert_eq!(p.query_raw("limit"), Some("5"));
        assert_eq!(p.query_raw("q"), Some("a b c"));
        assert_eq!(p.query_raw("e"), Some(""));
        assert_eq!(p.query_raw("bad"), Some("%ZZ"));
    }

    #[test]
    fn path_decoding() {
        assert_eq!(decode_path_segment("%20INC%31"), " INC1");
        assert_eq!(decode_path_segment("a%ZZ%"), "a%ZZ%");
        assert_eq!(decode_path_segment("%D0%96%FF"), "Ж\u{fffd}");
    }
}
