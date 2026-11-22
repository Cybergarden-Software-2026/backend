//! Eligibility and clock boundaries shared by the mobile snapshot and bell notification producer.
//! The web header evaluates timestamps at JavaScript Date (millisecond) precision.

use crate::change_detector::DEFAULT_COORDINATOR_GROUP;
use crate::constants::{NormalizedState, PRIORITY_CRITICAL};

/// Match ECMAScript String.trim on the effective source name.
pub fn header_team(name: &str) -> Option<&str> {
    let name = name.trim_matches(|c: char| (c.is_whitespace() && c != '\u{0085}') || c == '\u{feff}');
    (!name.is_empty()).then_some(name)
}

pub struct Header<'a> {
    pub state: NormalizedState,
    pub priority: i16,
    pub team: Option<&'a str>,
    pub submitted: i64,
    pub assignment: Option<i64>,
    pub deadline: Option<i64>,
}

impl Header<'_> {
    fn waiting_from(&self) -> Option<i64> {
        (self.team == Some(DEFAULT_COORDINATOR_GROUP))
            .then(|| self.assignment.unwrap_or(self.submitted) + 8 * 60_000 + 1)
    }

    fn deadline_window(&self) -> Option<(i64, i64)> {
        let end = self.deadline.filter(|end| *end > self.submitted)?;
        // First millisecond with strictly less than one fifth of the total duration remaining.
        Some((end - (end - self.submitted - 1) / 5, end))
    }

    pub fn kind_at(&self, now: i64) -> Option<&'static str> {
        if !self.state.is_attention() {
            return None;
        }
        if i64::from(self.priority) == PRIORITY_CRITICAL {
            return Some("critical");
        }
        if self.waiting_from().is_some_and(|at| now >= at) {
            return Some("waiting");
        }
        let (from, end) = self.deadline_window()?;
        (now >= from && (now < end || self.team.is_none() || self.team == Some(DEFAULT_COORDINATOR_GROUP)))
            .then_some("deadline")
    }

    /// Next instant when this same source record changes header kind, including leaving the header.
    pub fn next_change(&self, now: i64) -> Option<i64> {
        let deadline = self.deadline_window();
        let current = self.kind_at(now);
        [
            self.waiting_from(),
            deadline.map(|(from, _)| from),
            deadline.map(|(_, end)| end),
        ]
        .into_iter()
        .flatten()
        .filter(|at| *at > now && self.kind_at(*at) != current)
        .min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_wait_starts_at_the_last_assignment_and_requires_more_than_eight_minutes() {
        let team = Some(DEFAULT_COORDINATOR_GROUP);
        let alert = |now, assignment, priority, deadline| {
            Header {
                state: NormalizedState::Research,
                priority,
                team,
                submitted: 0,
                assignment,
                deadline,
            }
            .kind_at(now)
        };
        assert_eq!(alert(480_000, None, 2, None), None);
        assert_eq!(alert(480_001, None, 2, None), Some("waiting"));
        assert_eq!(alert(500_000, Some(100_000), 2, None), None);
        assert_eq!(alert(600_000, Some(100_000), 2, Some(600_001)), Some("waiting"));
        assert_eq!(alert(600_000, None, 4, Some(600_001)), Some("critical"));
    }

    #[test]
    fn deadline_crosses_twenty_percent_then_drops_at_expiry_for_resolution_teams() {
        let alert = |now, team, submit, deadline| {
            Header {
                state: NormalizedState::WorkInProgress,
                priority: 1,
                team,
                submitted: submit,
                assignment: None,
                deadline,
            }
            .kind_at(now)
        };
        assert_eq!(alert(799, Some("Team A"), 0, Some(1000)), None);
        assert_eq!(alert(800, Some("Team A"), 0, Some(1000)), None);
        assert_eq!(alert(801, Some("Team A"), 0, Some(1000)), Some("deadline"));
        assert_eq!(alert(1000, Some("Team A"), 0, Some(1000)), None);
        assert_eq!(alert(1000, None, 0, Some(1000)), Some("deadline"));
        assert_eq!(
            alert(1001, Some(DEFAULT_COORDINATOR_GROUP), 0, Some(1000)),
            Some("deadline")
        );
        assert_eq!(alert(1000, None, 1000, Some(1000)), None);
        assert_eq!(alert(900, None, 1000, Some(1100)), None);
        assert_eq!(alert(1000, None, 0, None), None);
    }

    #[test]
    fn schedules_only_boundaries_that_change_the_displayed_kind() {
        let mut header = Header {
            state: NormalizedState::Research,
            priority: 2,
            team: Some(DEFAULT_COORDINATOR_GROUP),
            submitted: 0,
            assignment: Some(30_000),
            deadline: Some(600_000),
        };
        assert_eq!(header.next_change(0), Some(480_001));
        assert_eq!(header.next_change(480_001), Some(510_001));
        assert_eq!(header.next_change(510_001), None);
        header.team = Some("Team A");
        assert_eq!(header.next_change(480_001), Some(600_000));
        assert_eq!(header.next_change(600_000), None);
        header.priority = 4;
        assert_eq!(header.next_change(0), None);
    }
}
