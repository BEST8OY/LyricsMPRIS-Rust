//! 3-stage concurrent lyrics fetcher implementing Morphe's `LyricsManager` racing pipeline.

use crate::config::RuntimeSettings;
use crate::event::display_state::{DisplayState, FIRST_PAINT_HOLD, ScoredCandidate};
use crate::lyrics::providers::{FetchedCandidate, LyricsProvider, ProviderContext, build_provider};
use crate::lyrics::types::{Lyrics, LyricsError, TrackInfo, TrackMatchInfo, is_retryable_status};
use crate::mpris::TrackMetadata;
use crate::text_utils::{
    any_dash_split, any_dash_split_reversed, clean_title_annotations, script_variants,
    split_artists, trusted_dash_split,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;

const STAGE1_DEADLINE: Duration = Duration::from_secs(2);
const STAGE2_DEADLINE: Duration = Duration::from_secs(4);
const STAGE3_DEADLINE: Duration = Duration::from_secs(9);
const ENDGAME_DEADLINE: Duration = Duration::from_secs(5);
const MIN_CUTOFF_CHECK_SECS: i64 = 60;
const MIN_COMPLETE_FRACTION: f64 = 0.15;

/// Outcome emitted by background lyrics fetching tasks.
#[derive(Debug)]
pub enum FetchOutcome {
    /// Initial lyrics result delivered for the track (with sorted fallback candidates).
    Success {
        epoch: u64,
        meta: TrackMetadata,
        lyrics: Box<Lyrics>,
        position: f64,
        candidates: Vec<ScoredCandidate>,
    },
    /// Upgraded or richer lyrics discovered later during the concurrent race.
    Upgraded {
        epoch: u64,
        meta: TrackMetadata,
        lyrics: Box<Lyrics>,
        candidates: Vec<ScoredCandidate>,
    },
    /// Full background candidate queue ready after Stage 3 completion for `'r'` cycling.
    CandidatesReady {
        epoch: u64,
        meta: TrackMetadata,
        candidates: Vec<ScoredCandidate>,
    },
    /// Authoritative conclusion: no matching lyrics found across configured providers.
    Exhausted {
        epoch: u64,
        meta: TrackMetadata,
        err: Option<String>,
    },
    /// Network/service unavailability prevented querying; eligible for exponential retry.
    Unavailable {
        epoch: u64,
        meta: TrackMetadata,
        service: String,
        message: String,
    },
}

impl FetchOutcome {
    pub fn epoch(&self) -> u64 {
        match self {
            Self::Success { epoch, .. }
            | Self::Upgraded { epoch, .. }
            | Self::CandidatesReady { epoch, .. }
            | Self::Exhausted { epoch, .. }
            | Self::Unavailable { epoch, .. } => *epoch,
        }
    }
}

#[derive(Clone, Debug)]
struct VariantQuery {
    track: TrackInfo,
    penalty: i32,
}

fn build_query_tiers(track: &TrackInfo) -> (Vec<VariantQuery>, Vec<VariantQuery>) {
    let mut original_tier = Vec::new();
    let mut derived_tier = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    let mut push_unique = |list: &mut Vec<VariantQuery>, t: TrackInfo, penalty: i32| {
        let key = (t.title.to_lowercase(), t.artist.to_lowercase());
        if !t.title.is_empty() && seen.insert(key) {
            list.push(VariantQuery { track: t, penalty });
        }
    };

    push_unique(&mut original_tier, track.clone(), 0);

    // Strip featuring/remaster noise if title changes meaningfully.
    if let Some(cleaned_title) = clean_title_annotations(&track.title)
        && !cleaned_title.is_empty()
        && cleaned_title != track.title
    {
        let mut ct = track.clone();
        ct.title = cleaned_title;
        push_unique(&mut original_tier, ct, 1);
    }

    if let Some((t_artist, t_title)) = trusted_dash_split(&track.title, &track.artist) {
        let trusted = TrackInfo::new(t_title, t_artist, track.album.clone(), track.duration_secs);
        push_unique(&mut original_tier, trusted, 1);
    }

    // Derived script variants (Simplified / Traditional Han) across title and/or artist
    let title_vars = script_variants(&track.title);
    let artist_vars = script_variants(&track.artist);
    for t_var in std::iter::once(&track.title).chain(title_vars.iter()) {
        for a_var in std::iter::once(&track.artist).chain(artist_vars.iter()) {
            if *t_var == track.title && *a_var == track.artist {
                continue;
            }
            let v = TrackInfo::new(
                t_var.clone(),
                a_var.clone(),
                track.album.clone(),
                track.duration_secs,
            );
            push_unique(&mut derived_tier, v, 1);
        }
    }

    // Derived split multi-artist queries
    for artist in split_artists(&track.artist) {
        if artist != track.artist {
            let v = TrackInfo::new(
                track.title.clone(),
                artist,
                track.album.clone(),
                track.duration_secs,
            );
            push_unique(&mut derived_tier, v, 2);
        }
    }

    // Derived dash splits (any-dash & reversed)
    if let Some((a, t)) = any_dash_split(&track.title) {
        let v = TrackInfo::new(t, a, track.album.clone(), track.duration_secs);
        push_unique(&mut derived_tier, v, 2);
    }
    if let Some((a, t)) = any_dash_split_reversed(&track.title) {
        let v = TrackInfo::new(t, a, track.album.clone(), track.duration_secs);
        push_unique(&mut derived_tier, v, 2);
    }

    (original_tier, derived_tier)
}

fn is_cut_off(lyrics: &Lyrics, track: &TrackInfo) -> bool {
    if !lyrics.synced || track.duration_secs < MIN_CUTOFF_CHECK_SECS {
        return false;
    }
    let last_end = lyrics
        .lines
        .iter()
        .map(|l| l.end.unwrap_or(l.time))
        .fold(0.0_f64, f64::max);
    last_end > 0.0 && last_end < (track.duration_secs as f64) * MIN_COMPLETE_FRACTION
}

fn is_transient(err: &LyricsError) -> bool {
    match err {
        LyricsError::Network(_) => true,
        LyricsError::Http(code) => is_retryable_status(*code),
        LyricsError::Api(_) | LyricsError::Serde(_) => false,
    }
}

struct TaskResult {
    provider_id: &'static str,
    penalty: i32,
    stage12: bool,
    outcome: Result<Vec<FetchedCandidate>, LyricsError>,
}

async fn run_provider_task(
    provider: Arc<dyn LyricsProvider>,
    vq: VariantQuery,
    ids: Arc<TrackMatchInfo>,
    ctx: Arc<ProviderContext>,
    stage12: bool,
) -> TaskResult {
    let outcome = provider.fetch_candidates(&vq.track, &ids, &ctx).await;
    let outcome = match outcome {
        Ok(cands) => {
            if let Some(first) = cands.first()
                && is_cut_off(&first.lyrics, &vq.track)
                && let Ok(retry_cands) = provider.fetch_candidates(&vq.track, &ids, &ctx).await
                && !retry_cands.is_empty()
            {
                Ok(retry_cands)
            } else {
                Ok(cands)
            }
        }
        Err(e) => Err(e),
    };
    TaskResult {
        provider_id: provider.id(),
        penalty: vq.penalty,
        stage12,
        outcome,
    }
}

fn build_ids(meta: &TrackMetadata) -> TrackMatchInfo {
    TrackMatchInfo {
        track_isrcs: meta.isrc.iter().cloned().collect(),
        track_spotify_ids: meta.spotify_id.iter().cloned().collect(),
        track_itunes_ids: meta.itunes_id.iter().cloned().collect(),
        youtube_video_id: meta.youtube_video_id.clone(),
    }
}

struct PollContext<'a> {
    epoch: u64,
    meta: &'a TrackMetadata,
    service: Option<&'a str>,
    endgame_at: Instant,
    outcome_tx: &'a mpsc::Sender<FetchOutcome>,
}

async fn emit_fetch_lyrics(
    ctx: &PollContext<'_>,
    lyrics: Lyrics,
    published_initial: &mut bool,
    candidates: Vec<ScoredCandidate>,
) {
    if !*published_initial {
        *published_initial = true;
        let fresh_pos = if let Some(svc) = ctx.service {
            crate::mpris::playback::get_position(svc)
                .await
                .unwrap_or(0.0)
        } else {
            0.0
        };
        let _ = ctx
            .outcome_tx
            .send(FetchOutcome::Success {
                epoch: ctx.epoch,
                meta: ctx.meta.clone(),
                lyrics: Box::new(lyrics),
                position: fresh_pos,
                candidates,
            })
            .await;
    } else {
        let _ = ctx
            .outcome_tx
            .send(FetchOutcome::Upgraded {
                epoch: ctx.epoch,
                meta: ctx.meta.clone(),
                lyrics: Box::new(lyrics),
                candidates,
            })
            .await;
    }
}

async fn poll_stage(
    pctx: &PollContext<'_>,
    deadline: Instant,
    join_set: &mut JoinSet<TaskResult>,
    display: &mut DisplayState,
    published_initial: &mut bool,
    any_transient_err: &mut bool,
    first_err: &mut Option<String>,
) {
    loop {
        if join_set.is_empty() || display.window_closed {
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if now >= pctx.endgame_at
            && display.best_result.is_none()
            && let Some(promoted) = display.promote_endgame()
        {
            let snapshot = display.sorted_snapshot();
            emit_fetch_lyrics(pctx, promoted, published_initial, snapshot).await;
            break;
        }

        let hold_wake = display
            .first_candidate_at
            .filter(|_| !*published_initial && display.word_sync)
            .map(|t| t + FIRST_PAINT_HOLD)
            .unwrap_or(deadline);
        let next_wake = deadline.min(pctx.endgame_at).min(hold_wake).max(now);

        tokio::select! {
            Some(res) = join_set.join_next() => {
                if let Ok(tr) = res {
                    match tr.outcome {
                        Ok(cands) => {
                            let mut close_win = false;
                            for c in cands {
                                if display.accept(c, tr.penalty, tr.stage12) {
                                    close_win = true;
                                }
                            }
                            if close_win {
                                display.window_closed = true;
                            }
                        }
                        Err(e) => {
                            tracing::debug!(provider = tr.provider_id, error = %e, "Provider fetch failed");
                            if is_transient(&e) {
                                *any_transient_err = true;
                            }
                            if first_err.is_none() {
                                *first_err = Some(e.to_string());
                            }
                        }
                    }
                    if let Some(ready) = display.take_ready_to_publish(false) {
                        let snapshot = display.sorted_snapshot();
                        emit_fetch_lyrics(pctx, ready, published_initial, snapshot).await;
                    }
                }
            }
            _ = tokio::time::sleep_until(next_wake) => {
                if let Some(ready) = display.take_ready_to_publish(false) {
                    let snapshot = display.sorted_snapshot();
                    emit_fetch_lyrics(pctx, ready, published_initial, snapshot).await;
                }
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }
}

/// Spawns the 3-stage concurrent lyrics fetch task for the current track epoch.
#[allow(clippy::too_many_arguments)]
pub fn spawn_fetch(
    epoch: u64,
    meta: TrackMetadata,
    providers: Vec<String>,
    service: Option<String>,
    word_sync: bool,
    collect_all_candidates: bool,
    settings: RuntimeSettings,
    outcome_tx: mpsc::Sender<FetchOutcome>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let ctx = Arc::new(ProviderContext::from_settings(settings));
        let track = TrackInfo::from_metadata(&meta);
        let ids = Arc::new(build_ids(&meta));

        let mut active_providers: Vec<Arc<dyn LyricsProvider>> = Vec::new();
        for id in &providers {
            if let Some(p) = build_provider(id) {
                if !p.is_configured(&ctx) {
                    continue;
                }
                if p.requires_video_id() && ids.youtube_video_id.is_none() {
                    continue;
                }
                active_providers.push(Arc::from(p));
            }
        }

        if active_providers.is_empty() || (track.title.is_empty() && ids.youtube_video_id.is_none())
        {
            let _ = outcome_tx
                .send(FetchOutcome::Exhausted {
                    epoch,
                    meta,
                    err: Some("No configured providers for track".into()),
                })
                .await;
            return;
        }

        let (original_tier, derived_tier) = build_query_tiers(&track);
        let (blind, non_blind): (Vec<_>, Vec<_>) =
            active_providers.into_iter().partition(|p| p.is_blind());

        let stage1_split = non_blind.len().min(4);
        let stage1_providers = &non_blind[..stage1_split];
        let stage2_providers = &non_blind[stage1_split..];

        let start = Instant::now();
        let stage1_deadline = start + STAGE1_DEADLINE;
        let stage2_deadline = start + STAGE2_DEADLINE;
        let stage3_deadline = start + STAGE3_DEADLINE;
        let endgame_at = start + ENDGAME_DEADLINE;

        let mut join_set: JoinSet<TaskResult> = JoinSet::new();
        for vq in &original_tier {
            for p in stage1_providers {
                join_set.spawn(run_provider_task(
                    Arc::clone(p),
                    vq.clone(),
                    Arc::clone(&ids),
                    Arc::clone(&ctx),
                    true,
                ));
            }
        }

        let mut display = DisplayState::new(word_sync, ctx.settings.allow_plain, false);
        let mut published_initial = false;
        let mut any_transient_err = false;
        let mut first_err: Option<String> = None;
        let mut stage2_submitted = stage2_providers.is_empty();
        let mut stage3_submitted = blind.is_empty() && derived_tier.is_empty();

        let pctx = PollContext {
            epoch,
            meta: &meta,
            service: service.as_deref(),
            endgame_at,
            outcome_tx: &outcome_tx,
        };

        // Stage 1 (0..2s)
        poll_stage(
            &pctx,
            stage1_deadline,
            &mut join_set,
            &mut display,
            &mut published_initial,
            &mut any_transient_err,
            &mut first_err,
        )
        .await;

        // Stage 2 (2..4s)
        if !display.window_closed && !stage2_providers.is_empty() {
            stage2_submitted = true;
            for vq in &original_tier {
                for p in stage2_providers {
                    join_set.spawn(run_provider_task(
                        Arc::clone(p),
                        vq.clone(),
                        Arc::clone(&ids),
                        Arc::clone(&ctx),
                        true,
                    ));
                }
            }
            poll_stage(
                &pctx,
                stage2_deadline,
                &mut join_set,
                &mut display,
                &mut published_initial,
                &mut any_transient_err,
                &mut first_err,
            )
            .await;
        }

        // Stage 3 (4..9s)
        if !display.window_closed && (!blind.is_empty() || !derived_tier.is_empty()) {
            stage3_submitted = true;
            for vq in &derived_tier {
                for p in &non_blind {
                    join_set.spawn(run_provider_task(
                        Arc::clone(p),
                        vq.clone(),
                        Arc::clone(&ids),
                        Arc::clone(&ctx),
                        false,
                    ));
                }
                for p in &blind {
                    join_set.spawn(run_provider_task(
                        Arc::clone(p),
                        vq.clone(),
                        Arc::clone(&ids),
                        Arc::clone(&ctx),
                        false,
                    ));
                }
            }
            for vq in &original_tier {
                for p in &blind {
                    join_set.spawn(run_provider_task(
                        Arc::clone(p),
                        vq.clone(),
                        Arc::clone(&ids),
                        Arc::clone(&ctx),
                        false,
                    ));
                }
            }
            poll_stage(
                &pctx,
                stage3_deadline,
                &mut join_set,
                &mut display,
                &mut published_initial,
                &mut any_transient_err,
                &mut first_err,
            )
            .await;
        }

        // Drain any already-finished tasks without blocking
        while let Some(res) = join_set.try_join_next() {
            if let Ok(tr) = res {
                match tr.outcome {
                    Ok(cands) => {
                        for c in cands {
                            display.accept(c, tr.penalty, false);
                        }
                    }
                    Err(e) => {
                        if is_transient(&e) {
                            any_transient_err = true;
                        }
                        if first_err.is_none() {
                            first_err = Some(e.to_string());
                        }
                    }
                }
            }
        }

        // Promote best fallback / blind winner if nothing published yet
        if let Some(promoted) = display.promote_endgame().or_else(|| {
            if !published_initial {
                display
                    .candidates
                    .peek()
                    .filter(|c| display.allow_plain || c.sync_rank > 0)
                    .map(|c| c.lyrics.clone())
            } else {
                None
            }
        }) {
            let snapshot = display.sorted_snapshot();
            emit_fetch_lyrics(&pctx, promoted, &mut published_initial, snapshot).await;
        }

        if !published_initial {
            join_set.abort_all();
            if any_transient_err {
                let _ = outcome_tx
                    .send(FetchOutcome::Unavailable {
                        epoch,
                        meta,
                        service: service.unwrap_or_default(),
                        message: first_err.unwrap_or_else(|| "Providers unreachable".into()),
                    })
                    .await;
            } else {
                let _ = outcome_tx
                    .send(FetchOutcome::Exhausted {
                        epoch,
                        meta,
                        err: first_err,
                    })
                    .await;
            }
            return;
        }

        if !collect_all_candidates {
            join_set.abort_all();
            return;
        }

        // Background queue fill: collect remaining providers for 'r' candidate cycling
        if !stage2_submitted {
            for vq in &original_tier {
                for p in stage2_providers {
                    join_set.spawn(run_provider_task(
                        Arc::clone(p),
                        vq.clone(),
                        Arc::clone(&ids),
                        Arc::clone(&ctx),
                        false,
                    ));
                }
            }
        }
        if !stage3_submitted {
            for vq in &original_tier {
                for p in &blind {
                    join_set.spawn(run_provider_task(
                        Arc::clone(p),
                        vq.clone(),
                        Arc::clone(&ids),
                        Arc::clone(&ctx),
                        false,
                    ));
                }
            }
            for vq in &derived_tier {
                for p in non_blind.iter().chain(blind.iter()) {
                    join_set.spawn(run_provider_task(
                        Arc::clone(p),
                        vq.clone(),
                        Arc::clone(&ids),
                        Arc::clone(&ctx),
                        false,
                    ));
                }
            }
        }

        let fill_deadline = Instant::now() + Duration::from_secs(6);
        while !join_set.is_empty() {
            tokio::select! {
                Some(res) = join_set.join_next() => {
                    if let Ok(TaskResult { outcome: Ok(cands), penalty, .. }) = res {
                        for c in cands {
                            display.accept(c, penalty, false);
                        }
                    }
                }
                _ = tokio::time::sleep_until(fill_deadline) => {
                    join_set.abort_all();
                    break;
                }
            }
        }

        let all_candidates = display.drain_sorted_candidates();
        if !all_candidates.is_empty() {
            let _ = outcome_tx
                .send(FetchOutcome::CandidatesReady {
                    epoch,
                    meta,
                    candidates: all_candidates,
                })
                .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_transient_error_classification() {
        assert!(is_transient(&LyricsError::Http(500)));
        assert!(is_transient(&LyricsError::Http(429)));
        assert!(is_transient(&LyricsError::Http(503)));
        assert!(!is_transient(&LyricsError::Http(404)));
        assert!(!is_transient(&LyricsError::Api("bad request".into())));
    }

    #[test]
    fn test_retryable_status_boundaries() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
    }

    #[test]
    fn test_query_tiers_and_cutoff() {
        let track = TrackInfo::new(
            "Artist - Track Name (Remastered 2020)",
            "Artist feat. Guest",
            "Album",
            240,
        );
        let (orig, derived) = build_query_tiers(&track);
        assert!(!orig.is_empty());
        assert!(!derived.is_empty());

        let mut short_lyrics = Lyrics::new(
            vec![crate::lyrics::types::LyricLine::new(5.0, "Early line").with_end(Some(10.0))],
            "LRCLIB",
            true,
        );
        assert!(is_cut_off(&short_lyrics, &track));
        short_lyrics.lines[0].end = Some(120.0);
        assert!(!is_cut_off(&short_lyrics, &track));
    }

    #[test]
    fn test_word_timing_defaults_in_fetch() {
        let word = crate::lyrics::types::WordTiming {
            start: 1.0,
            end: 2.0,
            text: "Hello".into(),
            grapheme_boundaries: vec![0, 5],
            grapheme_times: vec![],
            ..Default::default()
        };
        assert_eq!(word.text, "Hello");
        assert!(!word.ends_with_space);
    }

    #[test]
    fn test_build_query_tiers_single_side_han() {
        let track = TrackInfo::new("English Title", "周杰倫", "Album", 240);
        let (_orig, derived) = build_query_tiers(&track);
        assert!(
            derived
                .iter()
                .any(|v| v.track.title == "English Title" && v.track.artist == "周杰伦"),
            "Expected Simplified Han artist variant paired with unchanged ASCII title"
        );
    }
}
