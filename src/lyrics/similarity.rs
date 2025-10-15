use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Comprehensive similarity scoring information for song matching.
#[derive(Clone, Debug)]
pub struct ScoreInfo {
    /// Final aggregated similarity score (0.0..=1.0).
    pub score: f64,

    /// Per-component scores (title, artist, album, duration).
    /// Used for debugging and detailed match analysis.
    #[allow(dead_code)]
    pub components: HashMap<String, f64>,

    /// Normalized importance weights for each component.
    /// Used to calculate the final weighted score.
    #[allow(dead_code)]
    pub weights: HashMap<String, f64>,

    /// Duration values (in seconds) for query and candidate.
    /// Useful for debugging duration-based scoring.
    #[allow(dead_code)]
    pub durations: HashMap<String, Option<f64>>,
}

/// Normalize a string for comparison: lowercase, remove punctuation, collapse whitespace.
fn normalize_string(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    
    let lower = s.to_lowercase();
    let re = Regex::new(r"[^\w\s]").unwrap();
    let replaced = re.replace_all(&lower, " ");
    let ws = Regex::new(r"\s+").unwrap();
    ws.replace_all(&replaced, " ").trim().to_string()
}

/// Generate n-grams of specified size from a string.
fn get_ngrams(s: &str, size: usize) -> HashSet<String> {
    let chars: Vec<char> = s.chars().collect();
    
    if chars.len() < size || size == 0 {
        return HashSet::new();
    }
    
    (0..=(chars.len() - size))
        .map(|i| chars[i..i + size].iter().collect())
        .collect()
}

/// Calculate Sørensen-Dice coefficient between two strings using bigrams.
fn get_dice_coefficient(a: &str, b: &str) -> f64 {
    let a_grams = get_ngrams(a, 2);
    let b_grams = get_ngrams(b, 2);
    
    if a_grams.is_empty() && b_grams.is_empty() {
        return 1.0;
    }
    if a_grams.is_empty() || b_grams.is_empty() {
        return 0.0;
    }
    
    let intersection = a_grams.intersection(&b_grams).count() as f64;
    (2.0 * intersection) / ((a_grams.len() + b_grams.len()) as f64)
}

/// Calculate Levenshtein (edit) distance between two strings.
fn levenshtein_distance(s1: &str, s2: &str) -> usize {
    if s1 == s2 {
        return 0;
    }
    
    let a: Vec<char> = s1.chars().collect();
    let b: Vec<char> = s2.chars().collect();
    
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    let mut prev: Vec<usize> = (0..=a.len()).collect();
    let mut curr: Vec<usize> = vec![0; a.len() + 1];
    
    for (j, &bj) in b.iter().enumerate() {
        curr[0] = j + 1;
        for (i, &ai) in a.iter().enumerate() {
            let cost = if ai == bj { 0 } else { 1 };
            curr[i + 1] = (prev[i + 1] + 1)  // deletion
                .min(curr[i] + 1)             // insertion
                .min(prev[i] + cost);         // substitution
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    
    prev[a.len()]
}

/// Analyze a title into a base title and a set of version tags (remix, live, etc.).
fn analyze_title(title: &str) -> (String, HashSet<String>) {
    let tag_re = Regex::new(
        r"(?:[-(]|\s-\s)(remix|live|acoustic|instrumental|radio\sedit|remastered|explicit|clean|unplugged|re-recorded|edit|version|mono|stereo|deluxe|anniversary|reprise|demo)(?:\W|$)"
    ).unwrap();
    
    let mut base = normalize_string(title);
    let mut tags = HashSet::new();
    
    // Extract version tags
    for cap in tag_re.captures_iter(&base) {
        if let Some(m) = cap.get(1) {
            tags.insert(m.as_str().replace(' ', ""));
        }
    }
    
    // Clean up the base title: remove brackets, parentheses, and trailing content
    let patterns = [
        (Regex::new(r"\[[^\]]+\]").unwrap(), ""),                      // [text]
        (Regex::new(r"\(\d+(?::\d+(?:\.\d+)?)?\)").unwrap(), ""),     // (duration)
        (Regex::new(r"\([^)]*\)").unwrap(), ""),                       // (text)
    ];
    
    for (re, replacement) in &patterns {
        base = re.replace_all(&base, *replacement).to_string();
    }
    
    // Remove tags and trailing content after dash
    base = tag_re.replace_all(&base, " ").to_string();
    base = Regex::new(r"\s-\s.*").unwrap().replace_all(&base, "").to_string();
    
    // Normalize whitespace
    base = Regex::new(r"\s+").unwrap().replace_all(&base, " ").trim().to_string();
    
    (base, tags)
}

/// Normalize artist names for comparison: handle collaborations, features, and variations.
fn normalize_artist_name(artist: &str) -> String {
    if artist.is_empty() {
        return String::new();
    }
    
    let mut normalized = artist.to_lowercase();
    
    // Remove bracketed and parenthesized content
    let re_brackets = Regex::new(r"\[[^\]]+\]").unwrap();
    normalized = re_brackets.replace_all(&normalized, "").to_string();
    let re_paren = Regex::new(r"\([^)]*\)").unwrap();
    normalized = re_paren.replace_all(&normalized, "").to_string();
    
    // Split by collaboration separators and process each part
    let parts: Vec<String> = normalized
        .split(&['&', ','][..])
        .flat_map(|segment| {
            // Split by whitespace and handle features
            segment
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .split("feat")
                .map(|part| {
                    part.trim()
                        .replace("the", "")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect::<Vec<String>>()
        })
        .filter(|p| !p.is_empty())
        .collect();
    
    // Sort alphabetically for consistent comparison
    let mut sorted = parts;
    sorted.sort();
    sorted.join(" ")
}

/// Calculate title similarity considering base title and version tags.
fn calculate_title_similarity(title1: &str, title2: &str) -> f64 {
    let (base1, tags1) = analyze_title(title1);
    let (base2, tags2) = analyze_title(title2);
    
    // Combine Dice coefficient (60%) and normalized Levenshtein (40%)
    let dice = get_dice_coefficient(&base1, &base2);
    let max_len = base1.len().max(base2.len()) as f64;
    let lev = if max_len > 0.0 {
        1.0 - (levenshtein_distance(&base1, &base2) as f64 / max_len)
    } else {
        1.0
    };
    let base_score = dice * 0.6 + lev * 0.4;
    
    // Adjust score based on version tag matching
    let tag_adjustment = match (tags1.len(), tags2.len()) {
        (0, 0) => 0.05,  // Both have no tags: slight bonus
        (_, _) => {
            let common = tags1.intersection(&tags2).count();
            if common == tags1.len() && common == tags2.len() {
                0.1  // Perfect tag match: bonus
            } else if !tags1.is_empty() && !tags2.is_empty() && common == 0 {
                -0.25  // Tags mismatch: penalty
            } else {
                0.0  // Partial match: neutral
            }
        }
    };
    
    (base_score + tag_adjustment).clamp(0.0, 1.0)
}

/// Calculate artist similarity, handling collaborations and features.
fn calculate_artist_similarity(a1: &str, a2: &str) -> f64 {
    if a1.is_empty() || a2.is_empty() {
        return 0.0;
    }
    
    let n1 = normalize_artist_name(a1);
    let n2 = normalize_artist_name(a2);
    
    if n1 == n2 {
        return 1.0;
    }
    if n1.is_empty() || n2.is_empty() {
        return 0.0;
    }
    
    get_dice_coefficient(&n1, &n2)
}

/// Calculate duration similarity with tolerance for small differences.
fn calculate_duration_similarity(d1: Option<f64>, d2: Option<f64>) -> f64 {
    let Some(dur1) = d1 else { return 0.5 };
    let Some(dur2) = d2 else { return 0.5 };
    
    let diff = (dur1 - dur2).abs();
    let avg = (dur1 + dur2) / 2.0;
    let percentage = if avg > 0.0 { diff / avg } else { 0.0 };
    
    // Tiered scoring based on absolute and relative differences
    match () {
        _ if diff == 0.0 => 1.0,
        _ if diff <= 3.0 || percentage <= 0.02 => 0.98,
        _ if diff <= 5.0 || percentage <= 0.05 => 0.95,
        _ if diff <= 10.0 || percentage <= 0.08 => 0.85,
        _ if diff <= 15.0 || percentage <= 0.12 => 0.7,
        _ if diff <= 30.0 || percentage <= 0.20 => 0.5,
        _ => {
            // Exponential decay for large differences
            let decay = (-diff / 60.0).exp();
            (decay * 0.4).max(0.1)
        }
    }
}

/// Calculate overall song similarity for a candidate JSON object.
/// Supports multiple API formats (Apple Music, Musixmatch, etc.).
pub fn calculate_song_similarity(
    candidate: &Value,
    query_title: &str,
    query_artist: &str,
    query_album: Option<&str>,
    query_duration: Option<f64>,
) -> ScoreInfo {
    // Handle nested attributes (Apple Music style) or flat object
    let attrs = candidate.get("attributes").unwrap_or(candidate);
    
    // Extract candidate fields with fallback key names
    let cand_title = attrs
        .get("name")
        .or_else(|| attrs.get("title"))
        .or_else(|| attrs.get("track_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    
    let cand_artist = attrs
        .get("artistName")
        .or_else(|| attrs.get("artist"))
        .or_else(|| attrs.get("artist_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    
    let cand_album = attrs
        .get("albumName")
        .or_else(|| attrs.get("album"))
        .or_else(|| attrs.get("album_name"))
        .or_else(|| attrs.get("album_vanity_id"))
        .and_then(|v| v.as_str());
    
    // Handle various duration formats (milliseconds or seconds)
    let cand_duration = attrs
        .get("durationInMillis")
        .and_then(|v| v.as_f64())
        .map(|d| d / 1000.0)
        .or_else(|| attrs.get("durationMs").and_then(|v| v.as_f64()).map(|d| d / 1000.0))
        .or_else(|| {
            attrs.get("duration").and_then(|v| v.as_f64()).map(|d| {
                if d > 1000.0 { d / 1000.0 } else { d }
            })
        })
        .or_else(|| attrs.get("track_length").and_then(|v| v.as_f64()));

    // Calculate component similarity scores
    let title_score = calculate_title_similarity(cand_title, query_title);
    let artist_score = calculate_artist_similarity(cand_artist, query_artist);
    let album_score = match (query_album, cand_album) {
        (Some(q_album), Some(c_album)) => {
            get_dice_coefficient(&normalize_string(c_album), &normalize_string(q_album))
        }
        _ => 0.0,
    };
    let duration_score = calculate_duration_similarity(cand_duration, query_duration);

    // Calculate adaptive importance weights based on how distinctive each score is
    // Scores further from 0.5 (more distinctive) get higher importance
    let get_importance = |score: f64| ((score - 0.5).abs() * 2.0).powi(2);
    
    let importances = [
        ("title", get_importance(title_score)),
        ("artist", get_importance(artist_score)),
        ("album", if query_album.is_some() { get_importance(album_score) } else { 0.0 }),
        ("duration", if query_duration.is_some() { get_importance(duration_score) } else { 0.0 }),
    ];
    
    let total_importance: f64 = importances.iter().map(|(_, v)| v).sum();
    
    // If all importances are zero, use equal weights
    let weights: HashMap<String, f64> = if total_importance == 0.0 {
        importances.iter().map(|(k, _)| (k.to_string(), 0.25)).collect()
    } else {
        importances.iter().map(|(k, v)| (k.to_string(), v / total_importance)).collect()
    };

    // Calculate weighted final score
    let final_score = title_score * weights.get("title").copied().unwrap_or(0.0)
        + artist_score * weights.get("artist").copied().unwrap_or(0.0)
        + album_score * weights.get("album").copied().unwrap_or(0.0)
        + duration_score * weights.get("duration").copied().unwrap_or(0.0);

    // Build component scores map for debugging
    let components = [
        ("titleScore", title_score),
        ("artistScore", artist_score),
        ("albumScore", album_score),
        ("durationScore", duration_score),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), *v))
    .collect();
    
    let durations = [
        ("query", query_duration),
        ("candidate", cand_duration),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), *v))
    .collect();

    ScoreInfo {
        score: final_score.clamp(0.0, 1.0),
        components,
        weights,
        durations,
    }
}

/// Find the best song match among candidates using similarity scoring.
/// Returns the index and ScoreInfo if a confident match was found.
pub fn find_best_song_match(
    candidates: &[Value],
    query_title: &str,
    query_artist: &str,
    query_album: Option<&str>,
    query_duration: Option<f64>,
) -> Option<(usize, ScoreInfo)> {
    if candidates.is_empty() || query_title.is_empty() {
        return None;
    }
    
    // Filter candidates that have required fields and calculate scores
    let mut scored: Vec<(usize, ScoreInfo)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, cand)| {
            let attrs = cand.get("attributes").unwrap_or(cand);
            
            // Ensure candidate has title and artist
            let has_title = attrs
                .get("name")
                .or_else(|| attrs.get("title"))
                .or_else(|| attrs.get("track_name"))
                .and_then(|v| v.as_str())
                .is_some();
            
            let has_artist = attrs
                .get("artistName")
                .or_else(|| attrs.get("artist"))
                .or_else(|| attrs.get("artist_name"))
                .and_then(|v| v.as_str())
                .is_some();
            
            if has_title && has_artist {
                let score_info = calculate_song_similarity(cand, query_title, query_artist, query_album, query_duration);
                Some((i, score_info))
            } else {
                None
            }
        })
        .collect();
    
    if scored.is_empty() {
        return None;
    }
    
    // Sort by score descending
    scored.sort_by(|a, b| b.1.score.partial_cmp(&a.1.score).unwrap_or(std::cmp::Ordering::Equal));
    
    let (best_idx, best_score) = &scored[0];
    
    // Confidence threshold: require reasonable similarity
    const CONFIDENCE_THRESHOLD: f64 = 0.60;
    if best_score.score < CONFIDENCE_THRESHOLD {
        return None;
    }
    
    // If multiple candidates, ensure best is clearly better
    if scored.len() > 1 {
        let second_score = &scored[1].1;
        let gap = best_score.score - second_score.score;
        
        // Require clear separation unless top score is very high
        const MIN_GAP: f64 = 0.08;
        const HIGH_CONFIDENCE: f64 = 0.75;
        
        if gap < MIN_GAP && best_score.score < HIGH_CONFIDENCE {
            return None;
        }
    }
    
    Some((*best_idx, best_score.clone()))
}
