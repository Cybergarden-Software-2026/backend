//! Порт `datetime.fromisoformat` CPython 3.12 (версия образа backend) — C-реализации из `Modules/_datetimemodule.c`
//! (`_find_isoformat_datetime_separator`, `parse_isoformat_date`, `parse_isoformat_time`,
//! `parse_hh_mm_ss_ff`), а не чистого `_pydatetime`: в работающем Python действует C-код,
//! и он принимает больше (например, `15:40,5` — дробь после минут).
//!
//! Нормализатор обязан принимать ровно те даты, что и Python-версия: иначе один и тот же файл
//! одна версия примет, а другая отклонит (C8). Разбор идёт по байтам UTF-8 с нулём в конце,
//! как в C; совпадение проверяется на корпусе строк, разобранных настоящим Python
//! (`tests/fixtures/stage5_python.json`).

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Weekday};

/// Результат разбора: локальные дата-время и смещение зоны в микросекундах (`None` — без зоны).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsoDateTime {
    pub naive: NaiveDateTime,
    pub offset_us: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidIso;

type R<T> = Result<T, InvalidIso>;

/// Строка C: байты UTF-8 и `\0` за концом.
struct CStr<'a>(&'a [u8]);

impl CStr<'_> {
    fn at(&self, i: usize) -> u8 {
        self.0.get(i).copied().unwrap_or(0)
    }

    /// `parse_digits`: ровно `n` цифр с позиции `p` (границу не проверяет — как в C).
    fn digits(&self, p: usize, n: usize) -> Option<(usize, u32)> {
        let mut value = 0u32;
        for i in 0..n {
            let c = self.at(p + i);
            if !c.is_ascii_digit() {
                return None;
            }
            value = value * 10 + u32::from(c - b'0');
        }
        Some((p + n, value))
    }
}

fn find_separator(s: &CStr<'_>, len: usize) -> Option<usize> {
    if len == 7 {
        return Some(7);
    }
    if s.at(4) == b'-' {
        if s.at(5) == b'W' {
            if len < 8 {
                return None;
            }
            if len > 8 && s.at(8) == b'-' {
                if len == 9 {
                    return None;
                }
                if len > 10 && s.at(10).is_ascii_digit() {
                    return Some(8);
                }
                return Some(10);
            }
            return Some(8);
        }
        return Some(10);
    }
    if s.at(4) == b'W' {
        let mut idx = 7;
        while idx < len && s.at(idx).is_ascii_digit() {
            idx += 1;
        }
        if idx < 9 {
            return Some(idx);
        }
        return Some(if idx % 2 == 0 { 7 } else { 8 });
    }
    Some(8)
}

fn iso_to_date(year: u32, week: u32, day: u32) -> R<NaiveDate> {
    if !(1..=9999).contains(&year) || !(1..=7).contains(&day) {
        return Err(InvalidIso);
    }
    let weekday = Weekday::try_from((day - 1) as u8).map_err(|_| InvalidIso)?;
    // chrono, как и CPython, принимает 53-ю неделю только у «длинных» ISO-лет.
    NaiveDate::from_isoywd_opt(year as i32, week, weekday).ok_or(InvalidIso)
}

fn parse_date(s: &CStr<'_>, len: usize) -> R<NaiveDate> {
    let (mut p, year) = s.digits(0, 4).ok_or(InvalidIso)?;
    let uses_sep = s.at(p) == b'-';
    if uses_sep {
        p += 1;
    }
    if s.at(p) == b'W' {
        p += 1;
        let (q, week) = s.digits(p, 2).ok_or(InvalidIso)?;
        p = q;
        let mut day = 1;
        if p < len {
            if uses_sep {
                if s.at(p) != b'-' {
                    return Err(InvalidIso);
                }
                p += 1;
            }
            day = s.digits(p, 1).ok_or(InvalidIso)?.1;
        }
        return iso_to_date(year, week, day);
    }
    let (q, month) = s.digits(p, 2).ok_or(InvalidIso)?;
    p = q;
    if uses_sep {
        if s.at(p) != b'-' {
            return Err(InvalidIso);
        }
        p += 1;
    }
    let (_, day) = s.digits(p, 2).ok_or(InvalidIso)?;
    if !(1..=9999).contains(&year) {
        return Err(InvalidIso);
    }
    NaiveDate::from_ymd_opt(year as i32, month, day).ok_or(InvalidIso)
}

/// `parse_hh_mm_ss_ff`: `Ok((h, m, s, us), rest)`, `rest` — в строке остались символы
/// (возврат `1` в C; вызывающий решает, ошибка ли это).
fn parse_hh_mm_ss_ff(s: &CStr<'_>, start: usize, end: usize) -> R<([u32; 4], bool)> {
    let mut vals = [0u32; 4];
    let mut p = start;
    let mut has_sep = true;
    for i in 0..3 {
        let (q, v) = s.digits(p, 2).ok_or(InvalidIso)?;
        vals[i] = v;
        p = q;
        let c = s.at(p);
        p += 1;
        if i == 0 {
            has_sep = c == b':';
        }
        if p >= end {
            return Ok((vals, c != 0));
        } else if has_sep && c == b':' {
            continue;
        } else if c == b'.' || c == b',' {
            break;
        } else if !has_sep {
            p -= 1;
        } else {
            return Err(InvalidIso);
        }
    }
    // Дробная часть: до 6 цифр, лишние цифры отбрасываются.
    let to_parse = end.saturating_sub(p).min(6);
    let (q, mut us) = s.digits(p, to_parse).ok_or(InvalidIso)?;
    p = q;
    for _ in to_parse..6 {
        us *= 10;
    }
    vals[3] = us;
    while s.at(p).is_ascii_digit() {
        p += 1;
    }
    Ok((vals, s.at(p) != 0))
}

/// `parse_isoformat_time`: время и смещение зоны (секунды, микросекунды со знаком).
/// Время `[h, m, s, us]` и смещение зоны (секунды, микросекунды со знаком).
type TimeWithZone = ([u32; 4], Option<(i64, i64)>);

fn parse_time(s: &CStr<'_>, start: usize, end: usize) -> R<TimeWithZone> {
    let mut tz_pos = start;
    loop {
        if matches!(s.at(tz_pos), b'Z' | b'+' | b'-') {
            break;
        }
        tz_pos += 1;
        if tz_pos >= end {
            break;
        }
    }
    let (time, rest) = parse_hh_mm_ss_ff(s, start, tz_pos)?;
    if tz_pos == end {
        return if rest { Err(InvalidIso) } else { Ok((time, None)) };
    }
    if s.at(tz_pos) == b'Z' {
        return if s.at(tz_pos + 1) != 0 {
            Err(InvalidIso)
        } else {
            Ok((time, Some((0, 0))))
        };
    }
    let sign: i64 = if s.at(tz_pos) == b'-' { -1 } else { 1 };
    let ([th, tm, ts, tus], rest) = parse_hh_mm_ss_ff(s, tz_pos + 1, end)?;
    if rest {
        return Err(InvalidIso);
    }
    let seconds = sign * (i64::from(th) * 3600 + i64::from(tm) * 60 + i64::from(ts));
    Ok((time, Some((seconds, sign * i64::from(tus)))))
}

/// `datetime.fromisoformat(text)`.
pub fn fromisoformat(text: &str) -> R<IsoDateTime> {
    if text.chars().count() < 7 {
        return Err(InvalidIso);
    }
    let s = CStr(text.as_bytes());
    let len = text.len();
    let sep = find_separator(&s, len).ok_or(InvalidIso)?;
    let date = parse_date(&s, sep)?;

    let (time, tz) = if len > sep {
        // Разделитель — любой символ; в UTF-8 его длина видна по первому байту.
        let first = s.at(sep);
        let width = if first & 0x80 == 0 {
            1
        } else {
            match first & 0xf0 {
                0xe0 => 3,
                0xf0 => 4,
                _ => 2,
            }
        };
        parse_time(&s, sep + width, len)?
    } else {
        ([0; 4], None)
    };

    // timezone() допускает только |offset| < 24 ч.
    // `tzinfo_from_isoformat_results` CPython 3.12: целые секунды смещения равны 0 → `timezone.utc`,
    // и доли секунды теряются (`+00:00:00.5` → UTC, а `+05:30:15.5` — полностью). Проверено на 3.12.3
    // и 3.12.13; в 3.13 доли учитываются всегда — при переходе образа backend на 3.13 условие
    // убрать и пересобрать фикстуры.
    let offset_us = match tz {
        None => None,
        Some((seconds, micros)) => {
            let total = if seconds == 0 {
                0
            } else {
                seconds * 1_000_000 + micros
            };
            if total.abs() >= 86_400_000_000 {
                return Err(InvalidIso);
            }
            Some(total)
        }
    };
    let [h, m, sec, us] = time;
    let time = NaiveTime::from_hms_micro_opt(h, m, sec, us).ok_or(InvalidIso)?;
    if !(1..=9999).contains(&date.year()) {
        return Err(InvalidIso);
    }
    Ok(IsoDateTime {
        naive: date.and_time(time),
        offset_us,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> String {
        let p = fromisoformat(s).unwrap_or_else(|_| panic!("{s}"));
        format!("{} {:?}", p.naive.format("%Y-%m-%dT%H:%M:%S%.6f"), p.offset_us)
    }

    #[test]
    fn accepts_python_forms() {
        assert_eq!(ok("2026-11-08T12:02:29Z"), "2026-11-08T12:02:29.000000 Some(0)");
        assert_eq!(ok("2026-11-10 12:40:02"), "2026-11-10T12:40:02.000000 None");
        assert_eq!(
            ok("2026-11-10T12:40:02.4722639Z"),
            "2026-11-10T12:40:02.472263 Some(0)"
        );
        assert_eq!(ok("20261110T1240"), "2026-11-10T12:40:00.000000 None");
        assert_eq!(ok("2026-11-10"), "2026-11-10T00:00:00.000000 None");
        assert_eq!(ok("2026-W46-2"), "2026-11-10T00:00:00.000000 None");
        assert_eq!(ok("2026-W53"), "2026-12-28T00:00:00.000000 None");
        assert_eq!(
            ok("2026-11-10T15:40:02+03:00"),
            "2026-11-10T15:40:02.000000 Some(10800000000)"
        );
        assert_eq!(
            ok("2026-11-10T15:40,5-0130"),
            "2026-11-10T15:40:00.500000 Some(-5400000000)"
        );
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "вчера",
            "2026-13-40T00:00:00Z",
            "10.11.2026",
            "2026-11-10T25:00",
            "2026-11-1",
            "2026-11-10T12:3",
            "2026-11-10T12:00+24:00",
            "0000-01-01",
            "2025-W53",
            "2026-11-10T12:00Z05",
            "2026-11-10T",
        ] {
            assert!(fromisoformat(s).is_err(), "{s}");
        }
    }
}
