//! Где и почему `json.loads` CPython 3.12 отклонил бы текст — для сообщения
//! `некорректный JSON: строка L, позиция C (…)` слово в слово как у Python-версии.
//!
//! Разбор выполняет `serde_json`; этот модуль вызывается только после его отказа и повторяет
//! порядок проверок C-сканера CPython (`Modules/_json.c`: `scan_once_unicode`,
//! `_parse_object_unicode`, `_parse_array_unicode`, `scanstring_unicode`,
//! `_match_number_unicode`) и `json.decoder.JSONDecoder.decode` («Extra data», BOM).
//! Позиции — в символах (кодовых точках), как индексы `str` Python; `serde_json` считает байты
//! и при обрыве файла указывает на последний символ, а не на конец.
//!
//! Версия 3.12 — та, что в образе `backend` (`python:3.12-slim`). В 3.13 появились отдельные
//! тексты для висячей запятой (`Illegal trailing comma before end of object/array`).

/// Первая ошибка, которую встретил бы `json.loads(text, parse_constant=_reject_constant)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PyJsonError {
    /// `json.JSONDecodeError(msg, doc, pos)`; `pos` — индекс символа.
    Decode { msg: &'static str, pos: usize },
    /// `NaN`, `Infinity`, `-Infinity`: `parse_constant` бросает `ValueError`.
    Constant(&'static str),
}

impl PyJsonError {
    /// `(lineno, colno)` из `JSONDecodeError.__init__` для текста `text`.
    pub fn line_col(text: &str, pos_chars: usize) -> (usize, usize) {
        let mut line = 1;
        let mut col = 1;
        for c in text.chars().take(pos_chars) {
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }
}

struct Scanner<'a> {
    s: &'a [u8],
}

type Scan<T> = Result<T, Fail>;

/// Ошибка с позицией в байтах (переводится в символы в конце).
enum Fail {
    /// `StopIteration(idx)` → «Expecting value» на верхнем уровне.
    Stop(usize),
    Decode(&'static str, usize),
    Constant(&'static str),
}

const fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

impl Scanner<'_> {
    fn len(&self) -> usize {
        self.s.len()
    }

    fn at(&self, idx: usize) -> Option<u8> {
        self.s.get(idx).copied()
    }

    fn skip_ws(&self, mut idx: usize) -> usize {
        while self.at(idx).is_some_and(is_ws) {
            idx += 1;
        }
        idx
    }

    fn starts_with(&self, idx: usize, lit: &[u8]) -> bool {
        self.s.get(idx..idx + lit.len()) == Some(lit)
    }

    /// `scan_once_unicode`: значение с позиции `idx`, возвращает позицию после него.
    fn scan_once(&self, idx: usize) -> Scan<usize> {
        let Some(c) = self.at(idx) else {
            return Err(Fail::Stop(idx));
        };
        match c {
            b'"' => self.scanstring(idx + 1),
            b'{' => self.parse_object(idx + 1),
            b'[' => self.parse_array(idx + 1),
            b'n' if self.starts_with(idx, b"null") => Ok(idx + 4),
            b't' if self.starts_with(idx, b"true") => Ok(idx + 4),
            b'f' if self.starts_with(idx, b"false") => Ok(idx + 5),
            b'N' if self.starts_with(idx, b"NaN") => Err(Fail::Constant("NaN")),
            b'I' if self.starts_with(idx, b"Infinity") => Err(Fail::Constant("Infinity")),
            b'-' if self.starts_with(idx, b"-Infinity") => Err(Fail::Constant("-Infinity")),
            _ => self.match_number(idx),
        }
    }

    fn parse_object(&self, idx: usize) -> Scan<usize> {
        let mut idx = self.skip_ws(idx);
        if self.at(idx) != Some(b'}') {
            loop {
                if self.at(idx) != Some(b'"') {
                    return Err(Fail::Decode(
                        "Expecting property name enclosed in double quotes",
                        idx,
                    ));
                }
                idx = self.skip_ws(self.scanstring(idx + 1)?);
                if self.at(idx) != Some(b':') {
                    return Err(Fail::Decode("Expecting ':' delimiter", idx));
                }
                idx = self.skip_ws(idx + 1);
                idx = self.skip_ws(self.scan_once(idx)?);
                if self.at(idx) == Some(b'}') {
                    break;
                }
                if self.at(idx) != Some(b',') {
                    return Err(Fail::Decode("Expecting ',' delimiter", idx));
                }
                idx = self.skip_ws(idx + 1);
            }
        }
        Ok(idx + 1)
    }

    fn parse_array(&self, idx: usize) -> Scan<usize> {
        let mut idx = self.skip_ws(idx);
        if self.at(idx) != Some(b']') {
            loop {
                idx = self.skip_ws(self.scan_once(idx)?);
                if self.at(idx) == Some(b']') {
                    break;
                }
                if self.at(idx) != Some(b',') {
                    return Err(Fail::Decode("Expecting ',' delimiter", idx));
                }
                idx = self.skip_ws(idx + 1);
            }
        }
        Ok(idx + 1)
    }

    /// `scanstring_unicode(strict=True)`; `end` — позиция после открывающей кавычки.
    fn scanstring(&self, end: usize) -> Scan<usize> {
        let begin = end - 1;
        let len = self.len();
        let unterminated = Fail::Decode("Unterminated string starting at", begin);
        let mut next = end;
        loop {
            // Обычные символы до кавычки или обратной косой черты.
            loop {
                let Some(c) = self.at(next) else {
                    return Err(unterminated);
                };
                if c == b'"' || c == b'\\' {
                    break;
                }
                if c < 0x20 {
                    return Err(Fail::Decode("Invalid control character at", next));
                }
                next += 1;
            }
            if self.at(next) == Some(b'"') {
                return Ok(next + 1);
            }
            // Экранирование: next — позиция `\`.
            next += 1;
            let Some(esc) = self.at(next) else {
                return Err(unterminated);
            };
            if esc != b'u' {
                if !matches!(esc, b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') {
                    return Err(Fail::Decode("Invalid \\escape", next - 1));
                }
                next += 1;
                continue;
            }
            // \uXXXX: CPython требует хотя бы один символ после четырёх цифр.
            next += 1;
            let hex_end = next + 4;
            if hex_end >= len {
                return Err(Fail::Decode("Invalid \\uXXXX escape", next - 1));
            }
            let Some(code) = self.hex4(next) else {
                return Err(Fail::Decode("Invalid \\uXXXX escape", hex_end - 5));
            };
            next = hex_end;
            // Суррогатная пара: вторая половина разбирается, только если за ней ещё есть символ.
            if (0xD800..=0xDBFF).contains(&code)
                && hex_end + 6 < len
                && self.at(next) == Some(b'\\')
                && self.at(next + 1) == Some(b'u')
            {
                let low_end = hex_end + 6;
                if self.hex4(next + 2).is_none() {
                    return Err(Fail::Decode("Invalid \\uXXXX escape", low_end - 5));
                }
                let low = self.hex4(next + 2).unwrap_or(0);
                if (0xDC00..=0xDFFF).contains(&low) {
                    next = low_end;
                }
            }
        }
    }

    fn hex4(&self, idx: usize) -> Option<u32> {
        let digits = self.s.get(idx..idx + 4)?;
        digits
            .iter()
            .try_fold(0u32, |acc, &d| (d as char).to_digit(16).map(|v| (acc << 4) | v))
    }

    /// `_match_number_unicode`: грамматика числа JSON; ошибка — `StopIteration(start)`.
    fn match_number(&self, start: usize) -> Scan<usize> {
        let digit = |i: usize| self.at(i).is_some_and(|b| b.is_ascii_digit());
        let end_idx = self.len().wrapping_sub(1);
        let mut idx = start;
        if self.at(idx) == Some(b'-') {
            idx += 1;
            if idx >= self.len() {
                return Err(Fail::Stop(start));
            }
        }
        match self.at(idx) {
            Some(b'1'..=b'9') => {
                idx += 1;
                while digit(idx) {
                    idx += 1;
                }
            }
            Some(b'0') => idx += 1,
            _ => return Err(Fail::Stop(start)),
        }
        if idx < end_idx && self.at(idx) == Some(b'.') && digit(idx + 1) {
            idx += 2;
            while digit(idx) {
                idx += 1;
            }
        }
        if idx < end_idx && matches!(self.at(idx), Some(b'e' | b'E')) {
            let e_start = idx;
            idx += 1;
            if idx < end_idx && matches!(self.at(idx), Some(b'-' | b'+')) {
                idx += 1;
            }
            while digit(idx) {
                idx += 1;
            }
            if !digit(idx - 1) {
                idx = e_start;
            }
        }
        Ok(idx)
    }
}

/// Первая ошибка `json.loads(text)` (после снятия BOM `utf-8-sig`); `None` — CPython принял бы
/// текст (например, одиночный суррогат `\ud800` или `1e400`, которые отвергает `serde_json`).
pub fn locate_error(text: &str) -> Option<PyJsonError> {
    let to_chars = |byte_pos: usize| text[..byte_pos.min(text.len())].chars().count();
    if text.starts_with('\u{feff}') {
        return Some(PyJsonError::Decode {
            msg: "Unexpected UTF-8 BOM (decode using utf-8-sig)",
            pos: 0,
        });
    }
    let scanner = Scanner { s: text.as_bytes() };
    let start = scanner.skip_ws(0);
    let result = scanner.scan_once(start).and_then(|end| {
        let end = scanner.skip_ws(end);
        if end == scanner.len() {
            Ok(end)
        } else {
            Err(Fail::Decode("Extra data", end))
        }
    });
    match result {
        Ok(_) => None,
        Err(Fail::Stop(pos)) => Some(PyJsonError::Decode {
            msg: "Expecting value",
            pos: to_chars(pos),
        }),
        Err(Fail::Decode(msg, pos)) => Some(PyJsonError::Decode {
            msg,
            pos: to_chars(pos),
        }),
        Err(Fail::Constant(name)) => Some(PyJsonError::Constant(name)),
    }
}
