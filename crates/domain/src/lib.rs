//! Чистая бизнес-логика дашборда: без БД, без async, без ввода-вывода.
//!
//! Порт модулей Python-версии «без зависимостей» (см. `PLAN.md`, раздел 3):
//! * R2 — `constants`, `normalizer`, `validator`, `change_detector` и нужные плану импорта части
//!   `grouping` (отметка «Решён») и `notification_rules`;
//! * R3 — остальное из `grouping` (группы, сортировка, `describe_resolution`), `sla`, `cursors`;
//! * R5 — `incident_query`, `projection` (вместе с REST).
//!
//! [`pycompat`] и [`pydatetime`] воспроизводят поведение Python, от которого зависит паритет:
//! `content_hash` в общей БД, набор принимаемых дат, тексты ошибок.

pub mod analysis;
pub mod change_detector;
pub mod constants;
pub mod cursors;
pub mod grouping;
pub mod incident_query;
pub mod llm;
pub mod normalizer;
pub mod notification_rules;
pub mod projection;
pub mod pycompat;
pub mod pydatetime;
pub mod pyjson;
pub mod sla;
pub mod validator;
