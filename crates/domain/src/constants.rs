//! Доменные константы — порт `app/core/constants.py`.
//!
//! Строковые значения перечислений совпадают с Python (`StrEnum`) и с JSON-контрактом API.md;
//! порядок вариантов `ChangeEventType` — тот же, что в Python: по нему сортируются типы события.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Namespace for incidents created from the web form.
pub const FORM_INCIDENT_SYS_ID_PREFIX: &str = "web-form-";

/// Ошибка разбора строкового значения перечисления (аналог `ValueError` у `StrEnum("…")`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("'{value}' is not a valid {enum_name}")]
pub struct UnknownVariant {
    pub enum_name: &'static str,
    pub value: String,
}

/// Перечисление со строковыми значениями, как `StrEnum` в Python.
macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident { $( $(#[$vmeta:meta])* $variant:ident = $value:literal ),+ $(,)? }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name {
            $( $(#[$vmeta])* #[serde(rename = $value)] $variant ),+
        }

        impl $name {
            /// Все значения в порядке объявления (как итерация по `StrEnum`).
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $value),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = UnknownVariant;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($value => Ok($name::$variant),)+
                    _ => Err(UnknownVariant { enum_name: stringify!($name), value: s.to_owned() }),
                }
            }
        }
    };
}

str_enum! {
    /// Нормализованный статус. Значения совпадают с `stateEnum` выгрузки (API.md, `normalizedState`).
    pub enum NormalizedState {
        Assigned = "Assigned",
        WorkInProgress = "WorkInProgress",
        Resolved = "Resolved",
        Closed = "Closed",
        Research = "Research",
        Unknown = "Unknown",
    }
}

impl NormalizedState {
    /// Fallback по `stateId`, сверенный с фактической выгрузкой (в справочнике 7 и 10 перепутаны).
    pub fn from_state_id(state_id: i64) -> Option<Self> {
        match state_id {
            0 => Some(Self::Assigned),
            2 => Some(Self::WorkInProgress),
            7 => Some(Self::Resolved),
            10 => Some(Self::Closed),
            20 => Some(Self::Research),
            _ => None,
        }
    }

    /// Требует внимания координатора (API.md §3 «Группы дашборда»): `ATTENTION_STATES`.
    pub fn is_attention(self) -> bool {
        matches!(self, Self::Assigned | Self::WorkInProgress | Self::Research)
    }

    /// Активная работа над инцидентом (до устранения): `ACTIVE_STATES`.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Assigned | Self::WorkInProgress)
    }

    /// «Проблема устранена» — для отметки «Решён» у критического (C16): `RESOLVED_PHASE_STATES`.
    pub fn is_resolved_phase(self) -> bool {
        matches!(self, Self::Resolved | Self::Closed | Self::Research)
    }
}

str_enum! {
    pub enum ResolutionPhase {
        Research = "research",
        Resolved = "resolved",
        Closed = "closed",
    }
}

str_enum! {
    pub enum GroupKey {
        Critical = "critical",
        Coordinators = "coordinators",
        High = "high",
        Research = "research",
    }
}

impl GroupKey {
    /// `GROUP_TITLES`.
    pub const fn title(self) -> &'static str {
        match self {
            Self::Critical => "Критические",
            Self::Coordinators => "На координаторах",
            Self::High => "Высокий приоритет",
            Self::Research => "Исследование ситуации",
        }
    }
}

pub const PRIORITY_CRITICAL: i64 = 4;
pub const PRIORITY_HIGH: i64 = 3;
/// `VALID_PRIORITIES`.
pub const VALID_PRIORITIES: [i64; 4] = [1, 2, 3, 4];

str_enum! {
    /// Тип строки истории `incident_changes`.
    pub enum ChangeType {
        Created = "created",
        Updated = "updated",
        Removed = "removed",
        Restored = "restored",
    }
}

str_enum! {
    /// Типы `ChangeEvent` из API.md §5. Одно событие может иметь несколько типов.
    /// Порядок вариантов задаёт порядок типов в событии (`Ord`).
    pub enum ChangeEventType {
        Created = "created",
        Critical = "critical",
        Assigned = "assigned",
        Resolved = "resolved",
        Closed = "closed",
        Reopened = "reopened",
        Research = "research",
        Deadline = "deadline",
        Team = "team",
        Priority = "priority",
        Changed = "changed",
        Removed = "removed",
        TeamContacted = "team_contacted",
    }
}

str_enum! {
    pub enum NotificationType {
        CriticalRaised = "critical.raised",
        CoordinatorAssigned = "coordinator.assigned",
        CriticalResolved = "critical.resolved",
        SourceError = "source.error",
        SourceRecovered = "source.recovered",
    }
}

str_enum! {
    pub enum SourceStatus {
        Initializing = "initializing",
        Ok = "ok",
        Error = "error",
    }
}

str_enum! {
    pub enum DashboardEventType {
        DashboardChanged = "dashboard.changed",
    }
}

str_enum! {
    pub enum DashboardEventReason {
        Import = "import",
        Source = "source",
        Expiry = "expiry",
        Assignment = "assignment",
        MassProblems = "mass_problems",
        TeamContacted = "team_contacted",
    }
}

str_enum! {
    pub enum AnalysisKind {
        Summary = "summary",
    }
}

str_enum! {
    pub enum AnalysisJobStatus {
        Queued = "queued",
        Running = "running",
        Completed = "completed",
        Failed = "failed",
    }
}

str_enum! {
    pub enum AnalysisProvider {
        Llm = "llm",
        Rules = "rules",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_and_serialize_as_python() {
        for s in NormalizedState::ALL {
            assert_eq!(s.as_str().parse::<NormalizedState>().unwrap(), *s);
        }
        assert_eq!(
            serde_json::to_string(&NotificationType::CriticalRaised).unwrap(),
            "\"critical.raised\""
        );
        assert!("assigned".parse::<NormalizedState>().is_err()); // регистр важен, как у StrEnum
    }

    #[test]
    fn state_sets_match_python() {
        use NormalizedState::*;
        let attention: Vec<_> = NormalizedState::ALL.iter().filter(|s| s.is_attention()).collect();
        assert_eq!(attention, [&Assigned, &WorkInProgress, &Research]);
        assert!(Research.is_resolved_phase() && !Research.is_active() && !Unknown.is_attention());
        assert_eq!(NormalizedState::from_state_id(7), Some(Resolved));
        assert_eq!(NormalizedState::from_state_id(1), None);
    }

    #[test]
    fn change_event_type_order_is_declaration_order() {
        let mut v = vec![
            ChangeEventType::Removed,
            ChangeEventType::Created,
            ChangeEventType::Team,
        ];
        v.sort();
        assert_eq!(
            v,
            [
                ChangeEventType::Created,
                ChangeEventType::Team,
                ChangeEventType::Removed
            ]
        );
    }
}
