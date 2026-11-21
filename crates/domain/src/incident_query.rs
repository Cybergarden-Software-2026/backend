//! Фильтры, сортировка и курсор `GET /incidents` (API.md §2) — порт
//! `app/services/incident_query.py`.
//!
//! Курсор связан с ревизией снимка: если снимок изменился между страницами, клиент получает
//! 409 `CURSOR_EXPIRED` и запрашивает первую страницу заново (API.md §7).

use std::cmp::Ordering;

use chrono::{DateTime, Utc};

use crate::constants::{GroupKey, NormalizedState};
use crate::grouping::normalize_group_name;
use crate::projection::{IncidentProjection, IncidentRow};
use crate::pycompat;

pub const SORT_KEYS: [&str; 7] = [
    "deadline",
    "-deadline",
    "submitDate",
    "-submitDate",
    "priority",
    "-priority",
    "id",
];
pub const MAX_LIMIT: i64 = 500;
pub const DEFAULT_LIMIT: i64 = 100;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncidentFilters {
    pub q: Option<String>,
    pub group: Option<GroupKey>,
    pub state: Option<NormalizedState>,
    pub priority_id: Option<i64>,
    pub team: Option<String>,
}

/// Ошибки `query_incidents` (в API: 400 `INVALID_SORT` / `INVALID_CURSOR`, 409 `CURSOR_EXPIRED`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("sort: допустимо {}", SORT_KEYS.join(", "))]
    InvalidSort,
    #[error("Некорректный курсор")]
    InvalidCursor,
    #[error("Снимок изменился, запросите первую страницу заново")]
    CursorExpired { current_revision: i64 },
}

impl QueryError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidSort => "INVALID_SORT",
            Self::InvalidCursor => "INVALID_CURSOR",
            Self::CursorExpired { .. } => "CURSOR_EXPIRED",
        }
    }
}

fn search_columns(r: &IncidentRow) -> [Option<&str>; 9] {
    [
        Some(&r.id),
        Some(&r.sys_id),
        r.header.as_deref(),
        r.short_description.as_deref(),
        r.description.as_deref(),
        r.product_name.as_deref(),
        r.service.as_deref(),
        r.assigned_group_name.as_deref(),
        r.assigned_employee.as_deref(),
    ]
}

pub fn matches(p: &IncidentProjection<'_>, f: &IncidentFilters) -> bool {
    let r = p.row;
    if f.group.is_some_and(|g| !p.groups.contains(&g)) {
        return false;
    }
    if f.state.is_some_and(|s| p.state() != s) {
        return false;
    }
    if f.priority_id.is_some_and(|id| i64::from(r.priority_id) != id) {
        return false;
    }
    if f.team.as_deref().is_some_and(|team| {
        normalize_group_name(r.assigned_group_name.as_deref()) != normalize_group_name(Some(team))
    }) {
        return false;
    }
    // `if f.q:` — пустая строка фильтра не задаёт; `"  "` после strip совпадает со всем.
    if let Some(q) = f.q.as_deref().filter(|q| !q.is_empty()) {
        let needle = pycompat::casefold(q);
        let needle = pycompat::strip(&needle);
        if !search_columns(r)
            .iter()
            .any(|v| pycompat::casefold(v.unwrap_or("")).contains(needle))
        {
            return false;
        }
    }
    true
}

/// `dt(value)`: без значения — `datetime.max` (после любого времени).
fn cmp_dt(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn sort_projections<'p, 'a>(
    mut items: Vec<&'p IncidentProjection<'a>>,
    sort: &str,
) -> Vec<&'p IncidentProjection<'a>> {
    let desc = sort.starts_with('-');
    let key = sort.trim_start_matches('-');
    let by_id = |a: &&IncidentProjection<'_>, b: &&IncidentProjection<'_>| a.row.id.cmp(&b.row.id);
    // `sorted(..., reverse=True)` с уникальным `id` в ключе — просто обратный порядок ключа.
    let directed = |o: Ordering| if desc { o.reverse() } else { o };
    match key {
        "deadline" => {
            let (mut with_dl, mut without): (Vec<_>, Vec<_>) =
                items.into_iter().partition(|p| p.row.deadline.is_some());
            with_dl.sort_by(|a, b| directed(a.row.deadline.cmp(&b.row.deadline).then_with(|| by_id(a, b))));
            without.sort_by(by_id); // без срока всегда в конце
            with_dl.extend(without);
            with_dl
        }
        "submitDate" => {
            items.sort_by(|a, b| {
                directed(
                    a.row
                        .submit_date
                        .cmp(&b.row.submit_date)
                        .then_with(|| by_id(a, b)),
                )
            });
            items
        }
        "priority" => {
            // По умолчанию — от самого высокого; `-priority` — от низкого. Без `reverse`.
            items.sort_by(|a, b| {
                let (pa, pb) = (a.row.priority_id, b.row.priority_id);
                (if desc { pa.cmp(&pb) } else { pb.cmp(&pa) })
                    .then_with(|| cmp_dt(a.row.deadline, b.row.deadline))
                    .then_with(|| by_id(a, b))
            });
            items
        }
        _ => {
            items.sort_by(by_id);
            items
        }
    }
}

/// `encode_cursor`: `urlsafe_b64encode(f"{revision}:{offset}")` без `=`.
pub fn encode_cursor(revision: i64, offset: i128) -> String {
    pycompat::urlsafe_b64encode_nopad(format!("{revision}:{offset}").as_bytes())
}

/// `decode_cursor`: смещение страницы. Повторяет нестрогий разбор Python: `urlsafe_b64decode`
/// пропускает посторонние символы, `int()` принимает пробелы, `_` и цифры Unicode.
pub fn decode_cursor(cursor: Option<&str>, revision: i64) -> Result<i128, QueryError> {
    let cursor = match cursor {
        None | Some("") => return Ok(0),
        Some(c) => c,
    };
    let pad = (4 - cursor.chars().count() % 4) % 4;
    let padded = format!("{cursor}{}", "=".repeat(pad));
    let bytes = pycompat::urlsafe_b64decode(padded.as_bytes()).map_err(|_| QueryError::InvalidCursor)?;
    let text = String::from_utf8(bytes).map_err(|_| QueryError::InvalidCursor)?;
    let mut parts = text.split(':');
    let (Some(rev_text), Some(offset_text), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(QueryError::InvalidCursor);
    };
    let rev = pycompat::py_int(rev_text).ok_or(QueryError::InvalidCursor)?;
    let offset = pycompat::py_int(offset_text).ok_or(QueryError::InvalidCursor)?;
    if offset < 0 {
        return Err(QueryError::InvalidCursor);
    }
    if rev != i128::from(revision) {
        return Err(QueryError::CursorExpired {
            current_revision: revision,
        });
    }
    Ok(offset)
}

/// Страница и курсор следующей (`None` — страниц больше нет).
pub fn query_incidents<'p, 'a>(
    items: &'p [IncidentProjection<'a>],
    filters: &IncidentFilters,
    sort: &str,
    limit: i64,
    cursor: Option<&str>,
    revision: i64,
) -> Result<(Vec<&'p IncidentProjection<'a>>, Option<String>), QueryError> {
    if !SORT_KEYS.contains(&sort) {
        return Err(QueryError::InvalidSort);
    }
    let limit = i128::from(limit.clamp(1, MAX_LIMIT));
    let offset = decode_cursor(cursor, revision)?;
    let selected = sort_projections(items.iter().filter(|p| matches(p, filters)).collect(), sort);
    let total = selected.len() as i128;
    let next_offset = offset + limit;
    let page = if offset >= total {
        Vec::new()
    } else {
        let end = next_offset.min(total) as usize;
        selected[offset as usize..end].to_vec()
    };
    let next = (next_offset < total).then(|| encode_cursor(revision, next_offset));
    Ok((page, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trip_and_python_quirks() {
        let c = encode_cursor(7, 10);
        assert_eq!(decode_cursor(Some(&c), 7), Ok(10));
        assert_eq!(decode_cursor(None, 7), Ok(0));
        assert_eq!(decode_cursor(Some(""), 7), Ok(0));
        assert_eq!(
            decode_cursor(Some(&c), 8),
            Err(QueryError::CursorExpired { current_revision: 8 })
        );
        // "7: 1_0" — int() принимает пробел и подчёркивание.
        let odd = pycompat::urlsafe_b64encode_nopad("7: 1_0".as_bytes());
        assert_eq!(decode_cursor(Some(&odd), 7), Ok(10));
        // Сверено с Python: дополнение `=` считается по символам, не по байтам; не-ASCII
        // пропускается декодером.
        assert_eq!(decode_cursor(Some("Nz\u{e9}oxMA=="), 7), Ok(10));
        assert_eq!(decode_cursor(Some("NzogMV8w"), 7), Ok(10));
        for bad in [
            "not-base64!",
            "Zm9v",
            "%%%",
            &encode_cursor(7, -1),
            "NzoxMA\u{e9}",
            "NzoxMA\u{e9}\u{e9}",
            "NzoxMA=junk",
            "N=zoxMA",
            "NzoxMA%3D%3D",
        ] {
            assert_eq!(
                decode_cursor(Some(bad), 7),
                Err(QueryError::InvalidCursor),
                "{bad}"
            );
        }
    }

    #[test]
    fn sort_error_text() {
        assert_eq!(
            QueryError::InvalidSort.to_string(),
            "sort: допустимо deadline, -deadline, submitDate, -submitDate, priority, -priority, id"
        );
    }
}
