//! One decision for every security limit.
//!
//! Slice parsing and streaming both measure the same facts and call
//! [`decide_limit`]. A future DOM builder can call it with the same struct:
//! the function does not read bytes and does not know which cursor
//! implementation produced the offsets.
//!
//! L0, in order:
//!
//! 1. A byte-counted cap ([`LimitKind::TotalInput`], [`LimitKind::TokenLength`],
//!    [`LimitKind::InternalSubsetLength`]) is positional. It is reported at
//!    the offending byte, before a Syntax or Encoding decision that would
//!    have to read that byte.
//! 2. The smaller raw offset wins. On one byte, TotalInput beats TokenLength,
//!    TokenLength beats InternalSubsetLength, and any of those beat a
//!    char-counted limit. [`LimitKind::ExpandedBytes`] uses the byte that
//!    completed the expansion (the `;`) for that race and the `&` as the
//!    reported offset; a tie with TotalInput is TotalInput.
//!
//! Callers on the hot path keep a single armed index compare. This function
//! runs only after that compare fails, or after a counter has already overflowed.

use crate::config::ParserConfig;
use crate::error::LimitKind;

/// A limit the caller has already found exceeded.
///
/// `offset` is the position to report. `needed` is the raw-input byte that
/// had to be read before this limit could be known. For a char-counted limit
/// those differ by the overflowing character: `offset` is the cursor after
/// it, and `needed` is its last byte.
#[derive(Clone, Copy, Debug)]
pub struct MeasuredLimit {
    pub kind: LimitKind,
    pub limit: u64,
    pub actual: u64,
    pub offset: u64,
    pub needed: u64,
}

/// Inputs to [`decide_limit`]. Offsets are absolute positions in the original
/// document, not indexes into a chunk.
#[derive(Clone, Copy, Debug)]
pub struct LimitFacts<'a> {
    pub config: &'a ParserConfig,
    /// Byte the parser has reached. A byte cap whose offending offset is
    /// greater than this has not been read yet.
    pub cursor: u64,
    /// First byte of the current main-input token.
    ///
    /// `None` inside replacement text. Token, total-input, and subset caps
    /// count original input bytes only.
    pub token_origin: Option<u64>,
    /// First byte after the `[` that opened an internal subset.
    pub subset_start: Option<u64>,
    /// A counter that has already overflowed, if this call is not only a
    /// byte-cap check.
    pub measured: Option<MeasuredLimit>,
}

/// The limit that was actually exceeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitDecision {
    pub kind: LimitKind,
    pub limit: u64,
    pub actual: u64,
    pub offset: u64,
}

#[derive(Clone, Copy)]
struct Candidate {
    kind: LimitKind,
    limit: u64,
    actual: u64,
    offset: u64,
    needed: u64,
}

/// Rank on a shared byte. Lower wins.
fn rank(kind: LimitKind) -> u8 {
    match kind {
        LimitKind::TotalInput => 0,
        LimitKind::TokenLength => 1,
        LimitKind::InternalSubsetLength => 2,
        LimitKind::ExpandedBytes => 3,
        LimitKind::NameLength | LimitKind::TextLength | LimitKind::AttrValueLength => 4,
        _ => 5,
    }
}

fn beats(next: Candidate, best: Candidate) -> bool {
    next.needed < best.needed || (next.needed == best.needed && rank(next.kind) < rank(best.kind))
}

/// Choose the exceeded limit, if any.
///
/// Byte caps are derived from `config` and the offsets. `measured` is added
/// as one more candidate; it does not override an earlier byte.
#[cold]
#[inline(never)]
pub fn decide_limit(facts: &LimitFacts<'_>) -> Option<LimitDecision> {
    let mut best: Option<Candidate> = None;
    let mut consider = |candidate: Candidate| {
        if best.map(|have| beats(candidate, have)).unwrap_or(true) {
            best = Some(candidate);
        }
    };

    if let Some(origin) = facts.token_origin {
        let total = facts.config.max_total_input;
        if facts.cursor >= total {
            consider(Candidate {
                kind: LimitKind::TotalInput,
                limit: total,
                actual: total.saturating_add(1),
                offset: total,
                needed: total,
            });
        }
        let token_at = origin.saturating_add(facts.config.max_token_len);
        if facts.cursor >= token_at {
            let limit = facts.config.max_token_len;
            consider(Candidate {
                kind: LimitKind::TokenLength,
                limit,
                actual: limit.saturating_add(1),
                offset: token_at,
                needed: token_at,
            });
        }
        if let Some(start) = facts.subset_start {
            let at = start.saturating_add(facts.config.max_internal_subset_len);
            if facts.cursor >= at {
                let limit = facts.config.max_internal_subset_len;
                consider(Candidate {
                    kind: LimitKind::InternalSubsetLength,
                    limit,
                    actual: limit.saturating_add(1),
                    offset: at,
                    needed: at,
                });
            }
        }
    }

    if let Some(measured) = facts.measured {
        consider(Candidate {
            kind: measured.kind,
            limit: measured.limit,
            actual: measured.actual,
            offset: measured.offset,
            needed: measured.needed,
        });
    }

    best.map(|candidate| LimitDecision {
        kind: candidate.kind,
        limit: candidate.limit,
        actual: candidate.actual,
        offset: candidate.offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ParserConfig {
        ParserConfig::default()
    }

    fn facts(config: &ParserConfig, cursor: u64) -> LimitFacts<'_> {
        LimitFacts {
            config,
            cursor,
            token_origin: Some(0),
            subset_start: None,
            measured: None,
        }
    }

    #[test]
    fn total_only_and_token_only() {
        let total = cfg().with_max_total_input(6);
        let got = decide_limit(&facts(&total, 6)).unwrap();
        assert_eq!(
            (got.kind, got.limit, got.actual, got.offset),
            (LimitKind::TotalInput, 6, 7, 6)
        );
        let token = cfg().with_max_token_len(20);
        let got = decide_limit(&facts(&token, 20)).unwrap();
        assert_eq!(
            (got.kind, got.limit, got.actual, got.offset),
            (LimitKind::TokenLength, 20, 21, 20)
        );
    }

    #[test]
    fn same_byte_total_beats_token_and_text() {
        let config = cfg().with_max_total_input(15).with_max_token_len(12);
        let mut view = facts(&config, 15);
        view.token_origin = Some(3);
        view.measured = Some(MeasuredLimit {
            kind: LimitKind::TextLength,
            limit: 12,
            actual: 13,
            offset: 16,
            needed: 15,
        });
        let got = decide_limit(&view).unwrap();
        assert_eq!(got.kind, LimitKind::TotalInput);
        assert_eq!(got.offset, 15);
    }

    #[test]
    fn text_wins_when_its_byte_is_before_total() {
        let config = cfg().with_max_total_input(16);
        let mut view = facts(&config, 16);
        view.measured = Some(MeasuredLimit {
            kind: LimitKind::TextLength,
            limit: 12,
            actual: 13,
            offset: 16,
            needed: 15,
        });
        let got = decide_limit(&view).unwrap();
        assert_eq!(
            (got.kind, got.actual, got.offset),
            (LimitKind::TextLength, 13, 16)
        );
    }

    #[test]
    fn expanded_tie_is_total_and_earlier_expanded_wins() {
        let config = cfg().with_max_total_input(82).with_max_expanded_bytes(1);
        let mut view = facts(&config, 83);
        view.measured = Some(MeasuredLimit {
            kind: LimitKind::ExpandedBytes,
            limit: 1,
            actual: 6,
            offset: 80,
            needed: 82,
        });
        assert_eq!(decide_limit(&view).unwrap().kind, LimitKind::TotalInput);
        view.config = &config;
        let later = cfg().with_max_total_input(83).with_max_expanded_bytes(1);
        view.config = &later;
        let got = decide_limit(&view).unwrap();
        assert_eq!((got.kind, got.offset), (LimitKind::ExpandedBytes, 80));
    }

    #[test]
    fn subset_or_token_follows_which_cap_the_cursor_reached() {
        let subset = cfg().with_max_internal_subset_len(30);
        let mut view = facts(&subset, 43);
        view.subset_start = Some(13);
        let got = decide_limit(&view).unwrap();
        assert_eq!(
            (got.kind, got.limit, got.actual, got.offset),
            (LimitKind::InternalSubsetLength, 30, 31, 43)
        );
        let token = cfg().with_max_token_len(44);
        let view = facts(&token, 44);
        assert_eq!(decide_limit(&view).unwrap().kind, LimitKind::TokenLength);
    }
}
