//! Курсоры пагинации — порт `app/services/cursors.py`: непрозрачная строка снаружи, id
//! последнего элемента внутри.

use crate::pycompat;

pub const MAX_CURSOR_DIGITS: usize = 18;

/// `ValidationAppError(f"Некорректный параметр {name}", code="INVALID_CURSOR")` — 400 в API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Некорректный параметр {name}")]
pub struct InvalidCursor {
    pub name: String,
}

impl InvalidCursor {
    pub const CODE: &'static str = "INVALID_CURSOR";
}

/// `None` и `""` — без курсора. Цифры — десятичные цифры Unicode, как `str.isdigit()` + `int()`
/// (`"١٢"` → 12). Отличие от Python, оставленное намеренно: `"²"` и другие цифры без десятичного
/// значения `isdigit()` пропускает, а `int()` на них падает (500); здесь — обычная ошибка 400.
pub fn parse_cursor(cursor: Option<&str>, name: &str) -> Result<Option<i64>, InvalidCursor> {
    let cursor = match cursor {
        None | Some("") => return Ok(None),
        Some(c) => c,
    };
    let invalid = || InvalidCursor {
        name: name.to_owned(),
    };
    if cursor.chars().count() > MAX_CURSOR_DIGITS {
        return Err(invalid());
    }
    // 18 цифр всегда помещаются в i64.
    cursor
        .chars()
        .try_fold(0i64, |acc, c| {
            let digit = pycompat::decimal_digit(c).ok_or_else(invalid)?;
            Ok(acc * 10 + i64::from(digit))
        })
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_decimal_digits_like_python_int() {
        assert_eq!(parse_cursor(Some("\u{661}\u{662}"), "cursor"), Ok(Some(12)));
        assert_eq!(
            parse_cursor(Some(&"9".repeat(18)), "cursor"),
            Ok(Some(999_999_999_999_999_999))
        );
        assert!(parse_cursor(Some("²"), "cursor").is_err());
        assert!(parse_cursor(Some(" 1"), "cursor").is_err());
        assert_eq!(
            parse_cursor(Some("x"), "after").unwrap_err().to_string(),
            "Некорректный параметр after"
        );
    }
}
