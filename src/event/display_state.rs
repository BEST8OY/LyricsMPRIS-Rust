//! Morphe-style display gate, 800ms word-sync first-paint hold, and candidate priority queue.

use crate::lyrics::providers::{FetchedCandidate, composite_score};
use crate::lyrics::types::Lyrics;
use std::collections::{BinaryHeap, HashSet};
use std::time::Duration;
use tokio::time::Instant;

pub const FIRST_PAINT_HOLD: Duration = Duration::from_millis(800);

#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub composite_score: i32,
    pub sync_rank: i32,
    pub fingerprint: u64,
    pub lyrics: Lyrics,
}

impl ScoredCandidate {
    pub fn from_lyrics(lyrics: Lyrics, composite_score: i32) -> Self {
        Self {
            composite_score,
            sync_rank: lyrics.sync_rank(),
            fingerprint: lyrics.fingerprint(),
            lyrics,
        }
    }
}

impl PartialEq for ScoredCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.composite_score == other.composite_score && self.fingerprint == other.fingerprint
    }
}

impl Eq for ScoredCandidate {}

impl PartialOrd for ScoredCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredCandidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.composite_score
            .cmp(&other.composite_score)
            .then_with(|| self.sync_rank.cmp(&other.sync_rank))
    }
}

#[derive(Debug)]
pub struct DisplayState {
    pub word_sync: bool,
    pub allow_plain: bool,
    pub exhaustive: bool,
    pub published_fingerprint: Option<u64>,
    pub published_rank: i32,
    pub first_candidate_at: Option<Instant>,
    pub best_result: Option<Lyrics>,
    pub best_display_rank: i32,
    pub best_fallback: Option<Lyrics>,
    pub best_fallback_rank: i32,
    pub window_closed: bool,
    pub candidates: BinaryHeap<ScoredCandidate>,
    seen_fingerprints: HashSet<u64>,
}

impl DisplayState {
    pub fn new(word_sync: bool, allow_plain: bool, exhaustive: bool) -> Self {
        Self {
            word_sync,
            allow_plain,
            exhaustive,
            published_fingerprint: None,
            published_rank: -1,
            first_candidate_at: None,
            best_result: None,
            best_display_rank: if word_sync { -1 } else { -2 },
            best_fallback: None,
            best_fallback_rank: -1,
            window_closed: false,
            candidates: BinaryHeap::new(),
            seen_fingerprints: HashSet::new(),
        }
    }

    /// Accepts a fetched candidate into the priority queue and updates display winners.
    /// Returns `true` when the fast racing window may close immediately.
    pub fn accept(&mut self, cand: FetchedCandidate, variant_penalty: i32, stage12: bool) -> bool {
        if cand.lyrics.is_empty() {
            return false;
        }
        let rank = cand.lyrics.sync_rank();
        if rank == 0 && !self.allow_plain {
            return false;
        }

        let fp = cand.lyrics.fingerprint();
        let composite = composite_score(cand.match_score, &cand.lyrics, variant_penalty);
        if self.seen_fingerprints.insert(fp) {
            self.candidates
                .push(ScoredCandidate::from_lyrics(cand.lyrics.clone(), composite));
        }

        let is_high_match = !cand.is_blind;
        if !is_high_match {
            return false;
        }

        if rank > self.best_fallback_rank {
            self.best_fallback = Some(cand.lyrics.clone());
            self.best_fallback_rank = rank;
        }

        if self.word_sync {
            if rank == 2 {
                self.best_result = Some(cand.lyrics);
                self.best_display_rank = 2;
                return !self.exhaustive;
            }
            return false;
        }

        if rank > self.best_display_rank {
            self.best_result = Some(cand.lyrics);
            self.best_display_rank = rank;
        }
        !self.exhaustive && stage12 && rank == 1 && self.best_result.is_some()
    }

    /// Returns a newly ready `Lyrics` for progressive UI painting, respecting the 800ms word-sync hold.
    pub fn take_ready_to_publish(&mut self, force_flush: bool) -> Option<Lyrics> {
        let available = if let Some(ref best) = self.best_result {
            Some(best)
        } else if force_flush {
            self.best_fallback.as_ref().or_else(|| {
                self.candidates
                    .peek()
                    .filter(|c| self.allow_plain || c.sync_rank > 0)
                    .map(|c| &c.lyrics)
            })
        } else {
            self.best_fallback.as_ref()
        }?;

        let fp = available.fingerprint();
        let rank = available.sync_rank();
        if self.published_fingerprint == Some(fp) {
            return None;
        }

        if self.first_candidate_at.is_none() {
            self.first_candidate_at = Some(Instant::now());
        }

        if !force_flush
            && self.word_sync
            && self.published_fingerprint.is_none()
            && !self.exhaustive
            && rank < 2
            && self
                .first_candidate_at
                .is_some_and(|t| t.elapsed() < FIRST_PAINT_HOLD)
        {
            return None;
        }

        self.published_fingerprint = Some(fp);
        self.published_rank = rank;
        Some(available.clone())
    }

    /// Promotes endgame fallback when no primary winner emerged after the endgame deadline.
    pub fn promote_endgame(&mut self) -> Option<Lyrics> {
        if self.best_result.is_none() {
            if let Some(fb) = self.best_fallback.clone() {
                self.best_result = Some(fb);
            } else if let Some(top) = self.candidates.peek().cloned() {
                self.best_result = Some(top.lyrics);
            }
        }
        if self.best_result.is_some() {
            self.window_closed = true;
        }
        self.take_ready_to_publish(true)
    }

    /// Pops sorted remaining unshown candidates (highest composite score first).
    pub fn drain_sorted_candidates(&mut self) -> Vec<ScoredCandidate> {
        let mut list: Vec<ScoredCandidate> = self.candidates.drain().collect();
        list.sort_by(|a, b| b.cmp(a));
        list
    }

    /// Clones a sorted candidate snapshot (highest composite score first) without draining.
    pub fn sorted_snapshot(&self) -> Vec<ScoredCandidate> {
        let mut list: Vec<ScoredCandidate> = self.candidates.iter().cloned().collect();
        list.sort_by(|a, b| b.cmp(a));
        list
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::types::{LyricLine, WordTiming};

    fn make_lyrics(provider: &str, rank: i32, text: &str) -> Lyrics {
        let words = if rank >= 2 {
            Some(vec![WordTiming {
                start: 1.0,
                end: 1.5,
                text: text.to_string(),
                ..Default::default()
            }])
        } else {
            None
        };
        Lyrics {
            lines: vec![LyricLine {
                time: 1.0,
                end: Some(2.0),
                text: text.to_string(),
                words,
                ..Default::default()
            }],
            synced: rank >= 1,
            provider_name: provider.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_allow_plain_gate_blocks_unsynced_unless_enabled() {
        let mut strict = DisplayState::new(true, false, false);
        let plain = FetchedCandidate {
            lyrics: make_lyrics("Genius", 0, "Plain line"),
            match_score: 95,
            is_blind: false,
            video_id_keyed: false,
        };
        assert!(!strict.accept(plain.clone(), 0, true));
        assert!(strict.candidates.is_empty());
        assert!(strict.promote_endgame().is_none());

        let mut permissive = DisplayState::new(true, true, false);
        assert!(!permissive.accept(plain, 0, true));
        let promoted = permissive.promote_endgame().expect("plain should promote");
        assert_eq!(promoted.provider_name, "Genius");
    }

    #[test]
    fn test_word_sync_800ms_hold_and_upgrade() {
        let mut state = DisplayState::new(true, false, false);
        let line_cand = FetchedCandidate {
            lyrics: make_lyrics("LRCLIB", 1, "Synced line"),
            match_score: 90,
            is_blind: false,
            video_id_keyed: false,
        };
        assert!(!state.accept(line_cand, 0, true));
        // Held while < 800ms elapsed
        assert!(state.take_ready_to_publish(false).is_none());
        // Force flush bypasses hold
        let flushed = state
            .take_ready_to_publish(true)
            .expect("force flush should publish line fallback");
        assert_eq!(flushed.provider_name, "LRCLIB");

        // Word-synced match arrives and immediately upgrades + signals close
        let word_cand = FetchedCandidate {
            lyrics: make_lyrics("Apple Music", 2, "Word line"),
            match_score: 95,
            is_blind: false,
            video_id_keyed: false,
        };
        assert!(state.accept(word_cand, 0, true));
        let upgraded = state
            .take_ready_to_publish(false)
            .expect("word sync should immediately upgrade");
        assert_eq!(upgraded.provider_name, "Apple Music");
    }

    #[test]
    fn test_scored_candidate_ranking_and_dedup() {
        let mut state = DisplayState::new(true, false, true);
        let dup = make_lyrics("LRCLIB", 1, "Same text");
        let word = make_lyrics("Apple Music", 2, "Word text");
        state.accept(
            FetchedCandidate {
                lyrics: dup.clone(),
                match_score: 85,
                is_blind: false,
                video_id_keyed: false,
            },
            0,
            true,
        );
        state.accept(
            FetchedCandidate {
                lyrics: dup,
                match_score: 85,
                is_blind: false,
                video_id_keyed: false,
            },
            0,
            true,
        );
        state.accept(
            FetchedCandidate {
                lyrics: word,
                match_score: 90,
                is_blind: false,
                video_id_keyed: false,
            },
            0,
            true,
        );

        let drained = state.drain_sorted_candidates();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].lyrics.provider_name, "Apple Music");
    }
}
