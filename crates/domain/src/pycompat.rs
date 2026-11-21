//! Поведение Python, от которого зависит паритет с Python-версией.
//!
//! * `content_hash` хранится в общей БД: Rust обязан считать его байт в байт как
//!   `json.dumps(tracked, sort_keys=True, ensure_ascii=False, separators=(",", ":"))` —
//!   иначе после переключения версий каждая запись пойдёт по медленному пути сравнения;
//! * `str.strip()`/`str.split()` Python считают пробелами и `\x1c`–`\x1f`, а Rust — нет;
//! * сравнение значений `==` в Python: `1 == 1.0 == True`;
//! * `repr(float)` и `repr(value)` встречаются в текстах ошибок и в хэше.

use std::fmt::Write as _;

use serde_json::{Map, Number, Value};

// ------------------------------------------------------------------ строки

/// `str.isspace()` для одного символа: `White_Space` Unicode плюс `\x1c`–`\x1f`.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()` без аргументов.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `str.split()` без аргументов.
pub fn split_whitespace(s: &str) -> impl Iterator<Item = &str> {
    s.split(is_space).filter(|p| !p.is_empty())
}

/// `str.casefold()`: `to_lowercase` плюс полные свёртки, которых в нём нет (ß → ss и т. п.).
/// Для названий групп (кириллица, латиница) совпадает с Python.
pub fn casefold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'ß' | 'ẞ' => out.push_str("ss"),
            'ς' => out.push('σ'),
            'ﬀ' => out.push_str("ff"),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            'ﬃ' => out.push_str("ffi"),
            'ﬄ' => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            'İ' => out.push_str("i\u{307}"),
            _ => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// Первые `n` символов (срез `text[:n]` по кодовым точкам).
pub fn take_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

// ------------------------------------------------------------------ цифры

/// Первые символы (ноль) всех блоков десятичных цифр Unicode (категория Nd, Unicode 15):
/// каждый блок — 10 подряд идущих цифр 0–9. `int()` Python принимает любые из них.
const DECIMAL_ZEROS: [u32; 68] = [
    0x30, 0x660, 0x6F0, 0x7C0, 0x966, 0x9E6, 0xA66, 0xAE6, 0xB66, 0xBE6, 0xC66, 0xCE6, 0xD66, 0xDE6, 0xE50,
    0xED0, 0xF20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40,
    0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x11066,
    0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0, 0x11730, 0x118E0, 0x11950,
    0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16A60, 0x16AC0, 0x16B50, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC,
    0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E950, 0x1FBF0,
];

/// Значение десятичной цифры (`unicodedata.decimal`), как её понимает `int()`.
pub fn decimal_digit(c: char) -> Option<u32> {
    let code = c as u32;
    let i = DECIMAL_ZEROS.partition_point(|z| *z <= code);
    let zero = *DECIMAL_ZEROS.get(i.checked_sub(1)?)?;
    (code - zero < 10).then_some(code - zero)
}

// ------------------------------------------------------------------ числа

/// `repr(float)` (он же `str(float)` и представление в `json.dumps`).
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sci = shortest_sci(x.abs());
    let (mantissa, exp) = sci.split_once('e').expect("формат {:e}");
    let exp: i32 = exp.parse().expect("показатель {:e}");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let n = digits.len() as i32;
    let mut out = String::new();
    if x.is_sign_negative() {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        let decpt = exp + 1;
        if decpt <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-decpt) as usize));
            out.push_str(&digits);
        } else if decpt >= n {
            out.push_str(&digits);
            out.extend(std::iter::repeat_n('0', (decpt - n) as usize));
            out.push_str(".0");
        } else {
            out.push_str(&digits[..decpt as usize]);
            out.push('.');
            out.push_str(&digits[decpt as usize..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if n > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let _ = write!(out, "e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs());
    }
    out
}

/// Кратчайшие цифры, восстанавливающие число, как `repr` Python (`_Py_dg_dtoa`, режим 0): из
/// подходящих записей наименьшей длины — ближайшая к точному значению, при равенстве — с чётной
/// последней цифрой. `{:e}` Rust при равенстве выбирает другую (`664742861425528.25` →
/// `…528.3`, Python — `…528.2`), поэтому берётся корректно округлённая запись `{:.p$e}`
/// (точная, ничьи — к чётной) наименьшей точности, которая восстанавливает число.
fn shortest_sci(x: f64) -> String {
    let fast = format!("{x:e}");
    let digits = fast
        .split_once('e')
        .map_or(0, |(m, _)| m.chars().filter(char::is_ascii_digit).count());
    if digits > 1 {
        let exact = format!("{:.*e}", digits - 1, x);
        if exact.parse::<f64>().ok() == Some(x) {
            return exact;
        }
    }
    fast
}

/// `str(n)` для числа из JSON: целое — цифрами, дробное — как `repr(float)`.
pub fn number_str(n: &Number) -> String {
    if let Some(f) = n.as_f64().filter(|_| n.is_f64()) {
        float_repr(f)
    } else {
        n.to_string()
    }
}

/// Целое значение числа JSON, если это «int» Python (не float).
pub fn number_as_int(n: &Number) -> Option<i128> {
    if n.is_f64() {
        None
    } else {
        n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from))
    }
}

// ------------------------------------------------------------------ сравнение `==`

enum Num {
    Int(i128),
    Float(f64),
}

fn as_num(v: &Value) -> Option<Num> {
    match v {
        Value::Bool(b) => Some(Num::Int(i128::from(*b))),
        Value::Number(n) => Some(match number_as_int(n) {
            Some(i) => Num::Int(i),
            None => Num::Float(n.as_f64().unwrap_or(f64::NAN)),
        }),
        _ => None,
    }
}

fn int_eq_float(i: i128, f: f64) -> bool {
    // Точное сравнение, как в Python (2**53 + 1 != 2.0**53).
    f.fract() == 0.0 && f.abs() < 1e38 && f as i128 == i
}

/// `a == b` для значений, полученных из `json.loads`.
pub fn py_eq(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (as_num(a), as_num(b)) {
        return match (x, y) {
            (Num::Int(x), Num::Int(y)) => x == y,
            (Num::Float(x), Num::Float(y)) => x == y,
            (Num::Int(i), Num::Float(f)) | (Num::Float(f), Num::Int(i)) => int_eq_float(i, f),
        };
    }
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| py_eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| py_eq(v, w)))
        }
        _ => false,
    }
}

// ------------------------------------------------------------------ json.dumps

/// `json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":"))`.
///
/// Ключи сортируются по кодовым точкам (порядок байтов UTF-8 совпадает с ним), рекурсивно.
pub fn dumps_canonical(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value, true);
    out
}

/// `json.dumps(value, ensure_ascii=False, separators=(",", ":"))` — порядок ключей сохраняется.
/// Так тело ответа пишет `JSONResponse` Starlette (`1e+16`, а не `1e16` у `serde_json`).
pub fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value, false);
    out
}

/// `BaseModel.model_dump_json()` (pydantic-core 2.50, как в образе Python-версии): то же, что
/// [`dumps`], кроме записи чисел с плавающей точкой — см. [`float_pydantic`]. Так пишется `data:`
/// события `snapshot` в SSE.
pub fn dumps_pydantic(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, false, Floats::Pydantic);
    out
}

/// Число с плавающей точкой в `model_dump_json` (pydantic-core ≥ 2.42): как `repr(float)`, но
/// отрицательный показатель без ведущего нуля (`1e-6`, а не `1e-06`), а показатель −5 —
/// десятичной записью (`0.00001`). Старые pydantic-core (2.33–2.41) писали ещё и `1e16` вместо
/// `1e+16` (запись `serde_json`); сверено на 300 тыс. случайных чисел с pydantic-core 2.50.5.
pub fn float_pydantic(x: f64) -> String {
    let repr = float_repr(x);
    let Some((mantissa, exp)) = repr.split_once("e-") else {
        return repr;
    };
    let exp: u32 = exp.parse().expect("показатель repr");
    if exp != 5 {
        return format!("{mantissa}e-{exp}");
    }
    let (sign, digits) = match mantissa.strip_prefix('-') {
        Some(m) => ("-", m),
        None => ("", mantissa),
    };
    format!("{sign}0.0000{}", digits.replace('.', ""))
}

#[derive(Clone, Copy)]
enum Floats {
    Repr,
    Pydantic,
}

/// [`dumps_canonical`] для объекта без копирования в `Value`.
pub fn dumps_canonical_object(map: &Map<String, Value>) -> String {
    let mut out = String::new();
    write_object(&mut out, map, true);
    out
}

fn write_object(out: &mut String, map: &Map<String, Value>, sort: bool) {
    write_object_with(out, map, sort, Floats::Repr);
}

fn write_object_with(out: &mut String, map: &Map<String, Value>, sort: bool, floats: Floats) {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    if sort {
        entries.sort_by(|a, b| a.0.cmp(b.0));
    }
    out.push('{');
    for (i, (k, v)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(out, k);
        out.push(':');
        write_value(out, v, sort, floats);
    }
    out.push('}');
}

fn write_canonical(out: &mut String, value: &Value, sort: bool) {
    write_value(out, value, sort, Floats::Repr);
}

fn write_value(out: &mut String, value: &Value, sort: bool, floats: Floats) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if n.is_f64() {
                let f = n.as_f64().unwrap_or(f64::NAN);
                // json.dumps пишет бесконечности/NaN так; из json.loads они к нам не попадают.
                match () {
                    _ if f.is_nan() => out.push_str("NaN"),
                    _ if f == f64::INFINITY => out.push_str("Infinity"),
                    _ if f == f64::NEG_INFINITY => out.push_str("-Infinity"),
                    _ => out.push_str(&match floats {
                        Floats::Repr => float_repr(f),
                        Floats::Pydantic => float_pydantic(f),
                    }),
                }
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_json_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item, sort, floats);
            }
            out.push(']');
        }
        Value::Object(map) => write_object_with(out, map, sort, floats),
    }
}

/// Экранирование `json.dumps(..., ensure_ascii=False)`: только `"`, `\` и символы < 0x20.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ------------------------------------------------------------------ repr / type

/// `type(value).__name__` для значения из `json.loads`.
pub fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `repr(value)` для значения из `json.loads` (используется в текстах ошибок).
pub fn py_repr(value: &Value) -> String {
    let mut out = String::new();
    write_repr(&mut out, value);
    out
}

fn write_repr(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("None"),
        Value::Bool(true) => out.push_str("True"),
        Value::Bool(false) => out.push_str("False"),
        Value::Number(n) => out.push_str(&number_str(n)),
        Value::String(s) => write_str_repr(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_repr(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_str_repr(out, k);
                out.push_str(": ");
                write_repr(out, v);
            }
            out.push('}');
        }
    }
}

/// Непечатаемые символы по `str.isprintable()`: управляющие, разделители (кроме пробела)
/// и распространённые символы форматирования. Остальные категории Cf/Cn/Co встречаются
/// в выгрузке ITSM разве что случайно и отображаются как есть.
fn is_unprintable(c: char) -> bool {
    c.is_control()
        || (c != ' ' && c.is_whitespace())
        || matches!(c, '\u{ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}' | '\u{feff}')
}

fn write_str_repr(out: &mut String, s: &str) {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if is_unprintable(c) => {
                let code = c as u32;
                let _ = match code {
                    0..=0xff => write!(out, "\\x{code:02x}"),
                    0x100..=0xffff => write!(out, "\\u{code:04x}"),
                    _ => write!(out, "\\U{code:08x}"),
                };
            }
            c => out.push(c),
        }
    }
    out.push(quote);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    #[allow(clippy::excessive_precision)] // точные значения «ничьих»
    fn float_repr_matches_python() {
        for (x, s) in [
            (1.0, "1.0"),
            (100.0, "100.0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (-0.0, "-0.0"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1.25e-7, "1.25e-07"),
            (1e100, "1e+100"),
            (123.456, "123.456"),
            (2.0f64.powi(60), "1.152921504606847e+18"),
            // Ничьи двух кратчайших записей: Python берёт чётную цифру, `{:e}` Rust — нет.
            (2194626559988537.25, "2194626559988537.2"),
            (1452990394049841.25, "1452990394049841.2"),
            (-664742861425528.25, "-664742861425528.2"),
            (123456789012345.625, "123456789012345.62"),
            (5e-324, "5e-324"),
            (1e23, "1e+23"),
        ] {
            assert_eq!(float_repr(x), s, "{x}");
        }
    }

    #[test]
    #[allow(clippy::excessive_precision)]
    fn float_pydantic_matches_pydantic_core() {
        for (x, s) in [
            (1e-5, "0.00001"),
            (-1.5e-5, "-0.000015"),
            (1e-6, "1e-6"),
            (2.5e-7, "2.5e-7"),
            (1e16, "1e+16"),
            (0.0001, "0.0001"),
            (5e-324, "5e-324"),
            (1.0, "1.0"),
            (-664742861425528.25, "-664742861425528.2"),
        ] {
            assert_eq!(float_pydantic(x), s, "{x}");
        }
    }

    #[test]
    fn strip_and_split_use_python_whitespace() {
        assert_eq!(strip("\u{1f}\u{a0} a b \u{3000}\n"), "a b");
        assert_eq!(
            split_whitespace(" a\u{1c}b  c ").collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(casefold("КЦ Straße"), "кц strasse");
        assert_eq!(take_chars("абвгд", 3), "абв");
        assert_eq!(take_chars("аб", 3), "аб");
        assert_eq!(decimal_digit('7'), Some(7));
        assert_eq!(decimal_digit('٣'), Some(3));
        assert_eq!(decimal_digit('９'), Some(9));
        assert_eq!(decimal_digit('²'), None);
        assert_eq!(decimal_digit('/'), None);
    }

    #[test]
    fn eq_follows_python_numeric_tower() {
        assert!(py_eq(&json!(1), &json!(1.0)));
        assert!(py_eq(&json!(true), &json!(1)));
        assert!(!py_eq(&json!(true), &json!("true")));
        assert!(py_eq(&json!({"a": [1, 2.0]}), &json!({"a": [1.0, 2]})));
        assert!(!py_eq(&json!(9007199254740993_i64), &json!(9007199254740992.0)));
        assert!(!py_eq(&json!(null), &json!(0)));
    }

    #[test]
    fn dumps_matches_python_json() {
        let v = json!({"b": [1, 1.0, 1e16, null, true], "a": "кот\n\"\u{1}\u{7f}", "Я": {"z": 1, "y": 2}});
        assert_eq!(
            dumps_canonical(&v),
            "{\"a\":\"кот\\n\\\"\\u0001\u{7f}\",\"b\":[1,1.0,1e+16,null,true],\"Я\":{\"y\":2,\"z\":1}}"
        );
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(py_repr(&json!("4")), "'4'");
        assert_eq!(py_repr(&json!("it's")), "\"it's\"");
        assert_eq!(
            py_repr(&json!(["a\u{a0}", 1.0, null, false])),
            "['a\\xa0', 1.0, None, False]"
        );
        assert_eq!(py_repr(&json!({"k": "привет"})), "{'k': 'привет'}");
        assert_eq!(type_name(&json!(4.0)), "float");
    }
}

// ------------------------------------------------------------------ разбор целых (R5)

/// Предел `sys.set_int_max_str_digits` по умолчанию: длиннее `int()` не разбирает.
pub const INT_MAX_STR_DIGITS: usize = 4300;

/// Цифры с одиночными `_` между ними (как литерал Python и `int()`), значение с насыщением.
fn digits_with_underscores(body: &str, digit: impl Fn(char) -> Option<u32>) -> Option<i128> {
    if body.is_empty() || body.starts_with('_') || body.ends_with('_') || body.contains("__") {
        return None;
    }
    let mut count = 0usize;
    let mut value: i128 = 0;
    for c in body.chars() {
        if c == '_' {
            continue;
        }
        let d = digit(c)?;
        count += 1;
        value = value.saturating_mul(10).saturating_add(i128::from(d));
    }
    (count <= INT_MAX_STR_DIGITS).then_some(value)
}

fn split_sign(s: &str) -> (bool, &str) {
    match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    }
}

/// `int(s)` (основание 10): пробелы по краям (`str.strip`), знак, десятичные цифры Unicode,
/// одиночные `_` между цифрами. Значения вне `i128` насыщаются (для сравнений этого достаточно).
pub fn py_int(s: &str) -> Option<i128> {
    let (negative, body) = split_sign(strip(s));
    let value = digits_with_underscores(body, decimal_digit)?;
    Some(if negative { -value } else { value })
}

/// Разбор целого из строки в pydantic (lax): пробелы Unicode по краям, знак, цифры ASCII с
/// одиночными `_`, допускается дробная часть из одних нулей (`"3.0"`, но не `"3."`).
pub fn pydantic_int(s: &str) -> Option<i128> {
    let (negative, body) = split_sign(s.trim());
    let body = match body.split_once('.') {
        Some((int_part, frac)) if !frac.is_empty() && frac.bytes().all(|b| b == b'0') => int_part,
        Some(_) => return None,
        None => body,
    };
    let value = digits_with_underscores(body, |c| c.to_digit(10).filter(|_| c.is_ascii_digit()))?;
    Some(if negative { -value } else { value })
}

// ------------------------------------------------------------------ URL и base64 (R5)

/// `urllib.parse.quote(s, safe="")`: всё, кроме `A-Za-z0-9_.-~`, кодируется `%XX` (UTF-8).
pub fn url_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~') {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `base64.urlsafe_b64encode(data).decode().rstrip("=")`.
pub fn urlsafe_b64encode_nopad(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.len();
        let v = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..=n {
            let c = B64[((v >> (18 - 6 * i)) & 63) as usize];
            out.push(match c {
                b'+' => '-',
                b'/' => '_',
                c => c as char,
            });
        }
    }
    out
}

/// Ошибка `binascii.Error` (неполная группа base64).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidBase64;

/// `binascii.a2b_base64(data)` в нестрогом режиме CPython 3.12: символы вне алфавита
/// пропускаются, `=` завершает разбор после двух-трёх символов группы; `Err` — неполная группа.
pub fn a2b_base64(data: &[u8]) -> Result<Vec<u8>, InvalidBase64> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let (mut quad_pos, mut left, mut pads) = (0u8, 0u8, 0u8);
    for &c in data {
        if c == b'=' {
            if quad_pos >= 2 {
                pads += 1;
                if quad_pos + pads >= 4 {
                    return Ok(out);
                }
            }
            continue;
        }
        let Some(v) = B64.iter().position(|&a| a == c) else {
            continue;
        };
        let v = v as u8;
        pads = 0;
        match quad_pos {
            0 => {
                quad_pos = 1;
                left = v;
            }
            1 => {
                quad_pos = 2;
                out.push((left << 2) | (v >> 4));
                left = v & 0x0f;
            }
            2 => {
                quad_pos = 3;
                out.push((left << 4) | (v >> 2));
                left = v & 0x03;
            }
            _ => {
                quad_pos = 0;
                out.push((left << 6) | v);
                left = 0;
            }
        }
    }
    if quad_pos == 0 {
        Ok(out)
    } else {
        Err(InvalidBase64)
    }
}

/// `base64.urlsafe_b64decode(data)` (без проверки алфавита, как в Python).
pub fn urlsafe_b64decode(data: &[u8]) -> Result<Vec<u8>, InvalidBase64> {
    let translated: Vec<u8> = data
        .iter()
        .map(|&b| match b {
            b'-' => b'+',
            b'_' => b'/',
            b => b,
        })
        .collect();
    a2b_base64(&translated)
}

#[cfg(test)]
mod r5_tests {
    use super::*;

    #[test]
    fn python_int() {
        assert_eq!(py_int(" 10 "), Some(10));
        assert_eq!(py_int("+1_0"), Some(10));
        assert_eq!(py_int("-\u{661}\u{662}"), Some(-12));
        for bad in ["", "1__0", "_1", "1_", "--1", "1.0", "+ 1", "0x1"] {
            assert_eq!(py_int(bad), None, "{bad:?}");
        }
        assert_eq!(py_int(&"9".repeat(50)), Some(i128::MAX));
        assert_eq!(py_int(&"9".repeat(4301)), None);
    }

    #[test]
    fn pydantic_int_matches_probe() {
        // Таблица снята с pydantic 2 (FastAPI, query-параметр int).
        let ok = [
            ("+3", 3),
            ("1_0", 10),
            ("3.0", 3),
            ("3.00", 3),
            (" 3.0 ", 3),
            ("0003", 3),
            ("-0", 0),
            ("\t3\n", 3),
            ("\u{a0}3", 3),
            ("\u{2003}3", 3),
            ("\u{b}3", 3),
            ("\u{85}3", 3),
            ("00", 0),
            ("0.0", 0),
            ("-0.0", 0),
            ("4.000", 4),
        ];
        for (s, v) in ok {
            assert_eq!(pydantic_int(s), Some(v), "{s:?}");
        }
        let bad = [
            "3.", "3.01", ".0", "--3", "+-3", "_1", "1__0", "1_", "3_.0", "3._0", "3.0_0", "0b11", "\u{1c}3",
            " ", "1e0", "\u{663}", "\u{ff13}", "3.0.0", "1 2", "3,0", "inf", "nan", "",
        ];
        for s in bad {
            assert_eq!(pydantic_int(s), None, "{s:?}");
        }
    }

    #[test]
    fn quote_and_base64() {
        assert_eq!(url_quote("INC 1/2~._-Ж"), "INC%201%2F2~._-%D0%96");
        assert_eq!(urlsafe_b64encode_nopad(b"7:10"), "NzoxMA");
        assert_eq!(urlsafe_b64decode(b"NzoxMA=="), Ok(b"7:10".to_vec()));
        // Посторонние символы пропускаются, `=` в середине группы игнорируется.
        assert_eq!(urlsafe_b64decode(b"N%z=oxMA=="), Ok(b"7:10".to_vec()));
        assert_eq!(urlsafe_b64decode(b"%%%="), Ok(vec![]));
        assert_eq!(urlsafe_b64decode(b"Zm9v"), Ok(b"foo".to_vec()));
        assert!(urlsafe_b64decode(b"Zm9vY").is_err());
        assert!(urlsafe_b64decode(b"Zm9vYm=").is_err());
    }
}
