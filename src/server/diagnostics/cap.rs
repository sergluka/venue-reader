//! The per-document diagnostics cap, filled by severity: a less severe
//! finding never keeps a more severe one out.
//!
//! The walk and the main loop each hold a [`Quota`] and feed it the same
//! findings in the same order, so they reach the same decisions without the
//! walk ever retracting what it sent: a batch is only dropped together with
//! its whole generation.

use crate::config::Severity;
use crate::document::LineNo;
use crate::rules::Diag;

/// How many findings of each severity the cap holds.
#[derive(Debug, Clone)]
pub(super) struct Quota {
    cap: usize,
    held: [usize; Severity::ALL.len()],
}

/// What [`Quota::admit`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admit {
    Room,
    /// Taken in place of the last-arrived finding of this severity.
    Displacing(Severity),
    Refused,
}

impl Quota {
    pub(super) fn new(cap: usize) -> Quota {
        Quota {
            cap,
            held: [0; Severity::ALL.len()],
        }
    }

    pub(super) fn admit(&mut self, severity: Severity) -> Admit {
        let rank = severity.rank();
        if !self.full() {
            self.held[rank] += 1;
            return Admit::Room;
        }
        match self.least_severe() {
            Some(least_severe) if least_severe > rank => {
                self.held[least_severe] -= 1;
                self.held[rank] += 1;
                Admit::Displacing(Severity::ALL[least_severe])
            }
            _ => Admit::Refused,
        }
    }

    /// The severity from which every finding is refused from now on. Once set it
    /// only ever grows more severe, so the walk can stop rendering what it would
    /// only count.
    pub(super) fn refused_from(&self) -> Option<Severity> {
        if !self.full() {
            return None;
        }
        // A zero cap holds nothing and refuses every severity.
        Some(Severity::ALL[self.least_severe().unwrap_or(0)])
    }

    fn full(&self) -> bool {
        self.held.iter().sum::<usize>() >= self.cap
    }

    fn least_severe(&self) -> Option<usize> {
        self.held.iter().rposition(|&count| count > 0)
    }
}

/// The findings the cap holds, kept by severity so the one to displace is at
/// hand.
pub(super) struct CappedFindings {
    quota: Quota,
    arrived: u64,
    by_severity: [Vec<(u64, LineNo, Diag)>; Severity::ALL.len()],
}

impl CappedFindings {
    pub(super) fn new(cap: usize) -> CappedFindings {
        CappedFindings {
            quota: Quota::new(cap),
            arrived: 0,
            by_severity: Default::default(),
        }
    }

    pub(super) fn insert(&mut self, line: LineNo, diag: Diag) {
        match self.quota.admit(diag.severity) {
            Admit::Refused => return,
            Admit::Displacing(least_severe) => {
                self.by_severity[least_severe.rank()].pop();
            }
            Admit::Room => {}
        }
        self.arrived += 1;
        self.by_severity[diag.severity.rank()].push((self.arrived, line, diag));
    }

    pub(super) fn len(&self) -> usize {
        self.by_severity.iter().map(Vec::len).sum()
    }

    /// Arrival order, which is line order: the walk goes top to bottom.
    pub(super) fn in_line_order(&self) -> Vec<(LineNo, &Diag)> {
        let mut all: Vec<&(u64, LineNo, Diag)> = self.by_severity.iter().flatten().collect();
        all.sort_unstable_by_key(|(arrived, ..)| *arrived);
        all.into_iter()
            .map(|(_, line, diag)| (*line, diag))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use Severity::*;

    fn diag(severity: Severity, start: u32) -> Diag {
        Diag {
            code: "test",
            rule: "test",
            start: crate::codec::ByteOffset::new(start),
            end: crate::codec::ByteOffset::new(start + 1),
            message: String::new(),
            severity,
            document_key: None,
        }
    }

    #[test]
    fn room_first_then_the_least_severe_gives_way() {
        let mut quota = Quota::new(2);
        assert_eq!(quota.admit(Info), Admit::Room);
        assert_eq!(quota.admit(Hint), Admit::Room);
        assert_eq!(quota.refused_from(), Some(Hint));
        assert_eq!(quota.admit(Error), Admit::Displacing(Hint));
        assert_eq!(quota.refused_from(), Some(Info));
        assert_eq!(quota.admit(Warning), Admit::Displacing(Info));
        assert_eq!(quota.admit(Info), Admit::Refused);
        assert_eq!(quota.refused_from(), Some(Warning));
    }

    #[test]
    fn a_full_cap_of_one_severity_refuses_more_of_it() {
        let mut quota = Quota::new(1);
        assert_eq!(quota.refused_from(), None);
        assert_eq!(quota.admit(Error), Admit::Room);
        assert_eq!(
            quota.admit(Error),
            Admit::Refused,
            "the first errors in line order stay"
        );
        assert_eq!(quota.refused_from(), Some(Error));
    }

    #[test]
    fn a_zero_cap_refuses_everything() {
        let mut quota = Quota::new(0);
        assert_eq!(quota.admit(Error), Admit::Refused);
        assert_eq!(quota.refused_from(), Some(Error));
    }

    #[test]
    fn capped_findings_come_back_in_line_order() {
        let mut capped = CappedFindings::new(3);
        capped.insert(LineNo::new(0), diag(Info, 0));
        capped.insert(LineNo::new(0), diag(Error, 5));
        capped.insert(LineNo::new(1), diag(Info, 0));
        capped.insert(LineNo::new(2), diag(Warning, 0));
        assert_eq!(capped.len(), 3);
        let order: Vec<(u32, Severity)> = capped
            .in_line_order()
            .into_iter()
            .map(|(line, diag)| (line.raw(), diag.severity))
            .collect();
        assert_eq!(order, [(0, Info), (0, Error), (2, Warning)]);
    }

    fn severity() -> impl Strategy<Value = Severity> {
        prop::sample::select(Severity::ALL.to_vec())
    }

    proptest! {
        /// What the cap keeps is what a stable sort by severity, cut at the cap,
        /// would keep; and the walk's quota, fed only what it admitted, lets
        /// the main loop reach the same set.
        #[test]
        fn capped_findings_are_the_most_severe_first_in_line_order(
            severities in prop::collection::vec(severity(), 0..200),
            cap in 0usize..40,
        ) {
            let mut walk = Quota::new(cap);
            let mut capped = CappedFindings::new(cap);
            for (at, severity) in severities.iter().enumerate() {
                if walk.admit(*severity) != Admit::Refused {
                    capped.insert(LineNo::new(at as u32), diag(*severity, 0));
                }
            }
            let mut expected: Vec<(usize, Severity)> = severities.iter().copied().enumerate().collect();
            expected.sort_by_key(|(_, severity)| severity.rank());
            expected.truncate(cap);
            expected.sort_by_key(|(at, _)| *at);
            let got: Vec<(usize, Severity)> = capped
                .in_line_order()
                .into_iter()
                .map(|(line, diag)| (line.raw() as usize, diag.severity))
                .collect();
            prop_assert_eq!(got, expected);
        }
    }
}
