//! SLA: время до дедлайна — порт `app/services/sla.py`. Обычная бизнес-логика, без ИИ (ТЗ §9).
//!
//! Исходный дедлайн не изменяется. Расчёт привязан к `now`; клиент ведёт таймер сам по
//! `deadlineAt` и `serverTime`, поэтому оставшееся время меняется и без обновления файла.

use chrono::{DateTime, TimeDelta, Utc};

use crate::constants::NormalizedState;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlaStatus {
    NoDeadline,
    Ok,
    Warning,
    Overdue,
    NotApplicable,
}

impl SlaStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoDeadline => "no_deadline",
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Overdue => "overdue",
            Self::NotApplicable => "not_applicable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlaResult {
    pub status: SlaStatus,
    pub label: String,
    pub deadline_at: Option<DateTime<Utc>>,
    pub remaining_seconds: Option<i64>,
    pub computed_at: DateTime<Utc>,
}

/// `1 д 3 ч`, `2 ч 5 мин`, `25 мин`, `меньше минуты` (знак не важен: `int(abs(seconds))`).
pub fn format_duration(seconds: i64) -> String {
    let seconds = seconds.unsigned_abs();
    if seconds < 60 {
        return "меньше минуты".to_owned();
    }
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (hours, rest) = (rest / 3_600, rest % 3_600);
    let minutes = rest / 60;
    match (days, hours, minutes) {
        (0, 0, m) => format!("{m} мин"),
        (0, h, 0) => format!("{h} ч"),
        (0, h, m) => format!("{h} ч {m} мин"),
        (d, 0, _) => format!("{d} д"),
        (d, h, _) => format!("{d} д {h} ч"),
    }
}

/// `int(timedelta.total_seconds())`: деление микросекунд на 10⁶ с округлением до `float`,
/// затем отбрасывание дробной части к нулю. Совпадает с Python, пока |Δ| < 2⁵³ мкс (≈ 285 лет).
fn python_int_seconds(delta: TimeDelta) -> i64 {
    match delta.num_microseconds() {
        Some(us) => (us as f64 / 1_000_000.0).trunc() as i64,
        None => delta.num_seconds(),
    }
}

pub fn compute_sla(
    deadline: Option<DateTime<Utc>>,
    state: NormalizedState,
    now: DateTime<Utc>,
    warning_window: TimeDelta,
    completion_date: Option<DateTime<Utc>>,
) -> SlaResult {
    let result = |status, label: String, deadline_at, remaining_seconds| SlaResult {
        status,
        label,
        deadline_at,
        remaining_seconds,
        computed_at: now,
    };

    if state.is_resolved_phase() {
        // Срок решения уже не тикает: инцидент устранён.
        let label = match (deadline, completion_date) {
            (Some(deadline), Some(completed)) if completed <= deadline => "Решён в срок",
            (Some(_), Some(_)) => "Решён после срока",
            _ => "Инцидент решён",
        };
        return result(SlaStatus::NotApplicable, label.to_owned(), deadline, None);
    }

    let Some(deadline_at) = deadline else {
        return result(SlaStatus::NoDeadline, "Срок не задан".to_owned(), None, None);
    };

    let remaining = deadline_at - now;
    let seconds = python_int_seconds(remaining);
    let duration = format_duration(seconds);
    // Как в Python: «просрочен» — по целым секундам (−0,5 с ещё не просрочка),
    // «предупреждение» — по точному остатку.
    if seconds < 0 {
        result(
            SlaStatus::Overdue,
            format!("Просрочен на {duration}"),
            deadline,
            Some(seconds),
        )
    } else if remaining <= warning_window {
        result(
            SlaStatus::Warning,
            format!("Осталось {duration}"),
            deadline,
            Some(seconds),
        )
    } else {
        result(
            SlaStatus::Ok,
            format!("Осталось {duration}"),
            deadline,
            Some(seconds),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_seconds_truncates_towards_zero() {
        assert_eq!(python_int_seconds(TimeDelta::milliseconds(-500)), 0);
        assert_eq!(python_int_seconds(TimeDelta::milliseconds(-1500)), -1);
        assert_eq!(python_int_seconds(TimeDelta::microseconds(59_999_999)), 59);
    }

    #[test]
    fn half_second_overdue_is_still_warning() {
        let now = DateTime::<Utc>::UNIX_EPOCH + TimeDelta::days(20_000);
        let r = compute_sla(
            Some(now - TimeDelta::milliseconds(500)),
            NormalizedState::Assigned,
            now,
            TimeDelta::minutes(60),
            None,
        );
        assert_eq!(
            (r.status, r.label.as_str(), r.remaining_seconds),
            (SlaStatus::Warning, "Осталось меньше минуты", Some(0))
        );
    }
}
