//! Подбор групп массовых сбоев по текущему реестру и общая разметка координаторов.
//!
//! Автоматические кандидаты вычисляются заново по каждому снимку; в БД хранятся только ручные
//! группы и исключения (`mass_problem_workspace`). HTTP-слой — в `dashboard-api`, проверка групп
//! моделью — в `ai-worker`.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::json;
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory};

use crate::analysis::state_of;
use crate::projection::IncidentRow;

pub const SAME_SERVICE: &str = "Один затронутый сервис; регистрации в пределах 24 часов";
pub const SAME_SYMPTOMS: &str = "Похожие симптомы в заголовках; регистрации в пределах 24 часов";
pub const SAME_CONTEXT: &str =
    "Один продукт и сценарий, похожее описание проблемы; регистрации в пределах 24 часов";
pub const MAX_GAP_MS: u64 = 24 * 60 * 60 * 1000;
pub const STOP_WORDS: &[&str] = &[
    "ошибка",
    "ошибки",
    "проблема",
    "проблемы",
    "неисправность",
    "отказ",
    "сбой",
    "аномалия",
    "на",
    "с",
    "в",
    "и",
    "при",
    "для",
    "не",
    "ситуация",
];

#[derive(Deserialize, Serialize)]
pub struct Workspace {
    pub groups: Vec<ManualGroup>,
    pub exclusions: Vec<Exclusion>,
}

#[derive(Deserialize, Serialize)]
pub struct ManualGroup {
    pub id: String,
    pub title: String,
    pub reason: String,
    pub members: Vec<Member>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub sys_id: String,
    pub context_key: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Exclusion {
    pub sys_id: String,
    pub incident_id: String,
    pub header: String,
    pub group_title: String,
    pub context_key: String,
    pub excluded_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Cluster {
    pub id: String,
    pub title: String,
    pub reason: String,
    pub sys_ids: Vec<String>,
    pub incident_ids: Vec<String>,
}

impl Cluster {
    pub fn from_rows(id: String, title: String, reason: String, rows: &[&IncidentRow]) -> Self {
        Self {
            id,
            title,
            reason,
            sys_ids: rows.iter().map(|row| row.sys_id.clone()).collect(),
            incident_ids: rows.iter().map(|row| row.id.clone()).collect(),
        }
    }
}

impl Workspace {
    /// Ручные группы (с актуальными участниками), затем автоматические кандидаты из остальных.
    pub fn clusters(&self, rows: &[IncidentRow]) -> Vec<Cluster> {
        let excluded: HashMap<_, _> = self
            .exclusions
            .iter()
            .map(|row| (row.sys_id.as_str(), row.context_key.as_str()))
            .collect();
        let available: HashMap<_, _> = rows
            .iter()
            .filter(|row| {
                excluded
                    .get(row.sys_id.as_str())
                    .is_none_or(|key| *key != context_key(row))
            })
            .map(|row| (row.sys_id.as_str(), row))
            .collect();
        let mut claimed = HashSet::new();
        let mut groups: Vec<_> = self
            .groups
            .iter()
            .filter_map(|group| {
                let members: Vec<_> = group
                    .members
                    .iter()
                    .filter_map(|member| {
                        available.get(member.sys_id.as_str()).copied().filter(|row| {
                            state_of(row).is_attention()
                                && context_key(row) == member.context_key
                                && claimed.insert(row.sys_id.as_str())
                        })
                    })
                    .collect();
                (!members.is_empty()).then(|| {
                    Cluster::from_rows(
                        group.id.clone(),
                        group.title.clone(),
                        group.reason.clone(),
                        &members,
                    )
                })
            })
            .collect();
        groups.extend(automatic_clusters(
            available
                .into_values()
                .filter(|row| !claimed.contains(row.sys_id.as_str())),
        ));
        groups
    }

    /// Только автоматические кандидаты (без ручных групп и исключённых записей).
    pub fn automatic(&self, rows: &[IncidentRow]) -> Vec<Cluster> {
        self.clusters(rows)
            .into_iter()
            .filter(|c| !c.id.starts_with(MANUAL_PREFIX))
            .collect()
    }
}

/// Префикс идентификатора ручной группы.
pub const MANUAL_PREFIX: &str = "manual:";

pub fn index(rows: &[IncidentRow]) -> HashMap<&str, &IncidentRow> {
    rows.iter().map(|row| (row.sys_id.as_str(), row)).collect()
}

pub fn service(row: &IncidentRow) -> &str {
    row.service.as_deref().unwrap_or_default().trim()
}

pub fn header(row: &IncidentRow) -> &str {
    row.header.as_deref().unwrap_or_default().trim()
}

pub fn context_key(row: &IncidentRow) -> String {
    json!([service(row), header(row), row.submit_date]).to_string()
}

pub fn member(row: &IncidentRow) -> Member {
    Member {
        sys_id: row.sys_id.clone(),
        context_key: context_key(row),
    }
}

struct Candidate<'a> {
    incident: &'a IncidentRow,
    /// Слова заголовка.
    terms: HashSet<String>,
    /// Слова заголовка, краткого и полного описания.
    text: HashSet<String>,
}

/// Длина описания, по которой сравниваются записи (начало описания содержит суть проблемы).
const TEXT_CHARS: usize = 1500;

fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|ch: char| {
        !matches!(
            ch.general_category_group(),
            GeneralCategoryGroup::Letter | GeneralCategoryGroup::Number
        )
    })
    .filter(|word| word.encode_utf16().count() > 2)
    .map(str::to_lowercase)
    .filter(|word| !STOP_WORDS.contains(&word.as_str()))
}

fn trimmed(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or_default().trim()
}

/// Продукт записи (пустая строка, если не указан).
pub fn product(row: &IncidentRow) -> &str {
    trimmed(&row.product_name)
}

/// Сценарий использования (пустая строка, если не указан).
pub fn scenario(row: &IncidentRow) -> &str {
    trimmed(&row.scenario_name)
}

/// Доля общих слов: `common / union`, не меньше `min_common` общих слов.
fn similar(
    a: &HashSet<String>,
    b: &HashSet<String>,
    min_common: usize,
    numerator: usize,
    denominator: usize,
) -> bool {
    let common = a.intersection(b).count();
    let union = a.len() + b.len() - common;
    common >= min_common && denominator * common >= numerator * union
}

impl<'a> Candidate<'a> {
    fn new(incident: &'a IncidentRow) -> Self {
        let terms: HashSet<String> = words(header(incident)).collect();
        let mut text = terms.clone();
        for field in [&incident.short_description, &incident.description] {
            let value: String = trimmed(field).chars().take(TEXT_CHARS).collect();
            text.extend(words(&value));
        }
        Self {
            incident,
            terms,
            text,
        }
    }

    fn related(&self, other: &Self) -> Option<&'static str> {
        let a = self.incident;
        let b = other.incident;
        if a.submit_date
            .timestamp_millis()
            .abs_diff(b.submit_date.timestamp_millis())
            > MAX_GAP_MS
        {
            return None;
        }
        if !service(a).is_empty() && service(a) == service(b) {
            return Some(SAME_SERVICE);
        }
        if similar(&self.terms, &other.terms, 2, 3, 5) {
            return Some(SAME_SYMPTOMS);
        }
        // Один продукт сам по себе не признак массового сбоя: нужны тот же сценарий
        // и заметно совпадающее описание проблемы.
        let same_context = !product(a).is_empty()
            && product(a) == product(b)
            && !scenario(a).is_empty()
            && scenario(a) == scenario(b);
        (same_context && similar(&self.text, &other.text, 3, 2, 5)).then_some(SAME_CONTEXT)
    }
}

pub fn automatic_clusters<'a>(rows: impl Iterator<Item = &'a IncidentRow>) -> Vec<Cluster> {
    let mut rows: Vec<_> = rows.filter(|row| state_of(row).is_active()).collect();
    rows.sort_by(|a, b| a.sys_id.cmp(&b.sys_id));
    let mut buckets: Vec<Vec<Candidate<'a>>> = Vec::new();
    for incident in rows {
        let candidate = Candidate::new(incident);
        // Complete-link matching avoids grouping dissimilar endpoints of a chain.
        if let Some(bucket) = buckets
            .iter_mut()
            .find(|bucket| bucket.iter().all(|other| candidate.related(other).is_some()))
        {
            bucket.push(candidate);
        } else {
            buckets.push(vec![candidate]);
        }
    }
    let mut groups: Vec<_> = buckets
        .into_iter()
        .filter_map(|bucket| {
            let [first, second, ..] = bucket.as_slice() else {
                return None;
            };
            let rows: Vec<_> = bucket.iter().map(|candidate| candidate.incident).collect();
            let reason = first.related(second)?;
            let title = if rows.iter().all(|row| service(row) == service(first.incident)) {
                service(first.incident).to_owned()
            } else if reason == SAME_CONTEXT {
                format!("{}: {}", product(first.incident), scenario(first.incident))
            } else {
                "Похожие симптомы".to_owned()
            };
            let mut ids: Vec<_> = rows.iter().map(|row| row.sys_id.as_str()).collect();
            ids.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            Some(Cluster::from_rows(ids.join("-"), title, reason.to_owned(), &rows))
        })
        .collect();
    groups.sort_by(|a, b| {
        b.sys_ids
            .len()
            .cmp(&a.sys_ids.len())
            .then_with(|| a.id.cmp(&b.id))
    });
    groups
}

/// Задача проверки группы моделью: запись-якорь (наименьший `pk`) и вход.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewInput {
    pub anchor_pk: i64,
    pub anchor_id: String,
    pub anchor_sys_id: String,
    pub anchor_revision: i32,
    /// `{"total": n, "members": [...]}` — участники по возрастанию `sysId`, не больше
    /// [`crate::llm::CLUSTER_MAX_MEMBERS`]. Любое изменение полей или состава меняет вход.
    pub input: serde_json::Value,
}

fn clip(value: &str, max: usize) -> serde_json::Value {
    let value = value.trim();
    if value.is_empty() {
        return serde_json::Value::Null;
    }
    if value.chars().count() <= max {
        return json!(value);
    }
    let mut out: String = value.chars().take(max).collect();
    out.push('…');
    json!(out)
}

/// Вход проверки для автоматической группы. `None`, если участников нет в `rows`.
pub fn review_input(cluster: &Cluster, rows: &[IncidentRow]) -> Option<ReviewInput> {
    use crate::llm::{CLUSTER_FIELD_CHARS, CLUSTER_MAX_MEMBERS};
    let current = index(rows);
    let mut members: Vec<&IncidentRow> = cluster
        .sys_ids
        .iter()
        .filter_map(|id| current.get(id.as_str()).copied())
        .collect();
    if members.len() != cluster.sys_ids.len() || members.is_empty() {
        return None;
    }
    members.sort_by(|a, b| a.sys_id.cmp(&b.sys_id));
    let anchor = *members.iter().min_by_key(|row| row.pk)?;
    let field = |v: &Option<String>| clip(trimmed(v), CLUSTER_FIELD_CHARS);
    let listed: Vec<_> = members
        .iter()
        .take(CLUSTER_MAX_MEMBERS)
        .map(|row| {
            json!({
                "id": row.id,
                "header": field(&row.header),
                "shortDescription": field(&row.short_description),
                "description": field(&row.description),
                "product": field(&row.product_name),
                "service": field(&row.service),
                "scenario": field(&row.scenario_name),
            })
        })
        .collect();
    Some(ReviewInput {
        anchor_pk: anchor.pk,
        anchor_id: anchor.id.clone(),
        anchor_sys_id: anchor.sys_id.clone(),
        anchor_revision: anchor.incident_revision,
        input: json!({"total": members.len(), "members": listed}),
    })
}

/// Подставляет проверку модели в автоматическую группу: подтверждённая группа получает
/// название от модели, неподтверждённая — пометку в `reason`. Состав и `id` не меняются:
/// решение о группе остаётся за правилами и координатором.
pub fn apply_review(cluster: &mut Cluster, related: bool, title: &str, summary: &str) {
    if related {
        cluster.title = title.to_owned();
        cluster.reason = format!("{}. ИИ: {summary}", cluster.reason);
    } else {
        cluster.reason = format!("{}. ИИ не подтвердил общую проблему: {summary}", cluster.reason);
    }
}
