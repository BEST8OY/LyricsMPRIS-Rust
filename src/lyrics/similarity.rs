use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Similarity utilities ported from the project's JS implementation.
/// Public API mirrors the JS behaviour but uses Rust types.
#[derive(Clone, Debug)]
pub struct ScoreInfo {
    /// Final aggregated similarity score (0.0..=1.0).
    pub score: f64,

    /// Per-component numeric scores (e.g. titleScore, artistScore, albumScore, durationScore).
    ///
    /// This is primarily used for debug/inspection. It may be unused in the
    /// main code path; keep it for diagnostics. Silence dead_code warnings on
    /// this field so the compiler doesn't warn when it's not read elsewhere.
    #[allow(dead_code)]
    pub components: HashMap<String, f64>,

    /// Normalized importance weights used to combine component scores into the
    /// final `score`. Keys are component names ("title", "artist", "album",
    /// "duration"). Kept for inspection and future use.
    #[allow(dead_code)]
    pub weights: HashMap<String, f64>,

    /// Durations map contains optional durations (in seconds) for the query
    /// and candidate. Keys used: "query" and "candidate". Useful for
    /// debugging duration-based scoring differences.
    #[allow(dead_code)]
    pub durations: HashMap<String, Option<f64>>,
}

fn normalize_string(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    // Lowercase, replace non-word chars with spaces, collapse whitespace
    let lower = s.to_lowercase();
    let re = Regex::new(r"[^\w\s]").unwrap();
    let replaced = re.replace_all(&lower, " ");
    let ws = Regex::new(r"\s+").unwrap();
    ws.replace_all(&replaced, " ").trim().to_string()
}

fn get_ngrams(s: &str, size: usize) -> HashSet<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = HashSet::new();
    if chars.len() < size || size == 0 {
        return out;
    }
    for i in 0..=(chars.len() - size) {
        let gram: String = chars[i..i + size].iter().collect();
        out.insert(gram);
    }
    out
}

fn get_dice_coefficient(a: &str, b: &str) -> f64 {
    let a_grams = get_ngrams(a, 2);
    let b_grams = get_ngrams(b, 2);
    if a_grams.is_empty() && b_grams.is_empty() {
        return 1.0;
    }
    if a_grams.is_empty() || b_grams.is_empty() {
        return 0.0;
    }
    let inter = a_grams.intersection(&b_grams).count() as f64;
    (2.0 * inter) / ((a_grams.len() + b_grams.len()) as f64)
}

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
    for (j, bj) in b.iter().enumerate() {
        curr[0] = j + 1;
        for (i, ai) in a.iter().enumerate() {
            let cost = if ai == bj { 0 } else { 1 };
            curr[i + 1] = std::cmp::min(
                std::cmp::min(prev[i + 1] + 1, curr[i] + 1),
                prev[i] + cost,
            );
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[a.len()]
}

/// Analyze a title into a base title and a set of tags (e.g., live, remix)
fn analyze_title(title: &str) -> (String, HashSet<String>) {
    let mut tags = HashSet::new();
    let tag_re = Regex::new(r"(?:[-(]|\s-\s)(remix|live|acoustic|instrumental|radio\sedit|remastered|explicit|clean|unplugged|re-recorded|edit|version|mono|stereo|deluxe|anniversary|reprise|demo)(?:\W|$)").unwrap();
    let mut base = normalize_string(title);
    for cap in tag_re.captures_iter(&base) {
        if let Some(m) = cap.get(1) {
            tags.insert(m.as_str().replace(' ', ""));
        }
    }
    // Remove bracketed content and common noise
    let re_brackets = Regex::new(r"\[[^\]]+\]").unwrap();
    base = re_brackets.replace_all(&base, "").to_string();
    let re_parentheses_dur = Regex::new(r"\(\d+(?::\d+(?:\.\d+)?)?\)").unwrap();
    base = re_parentheses_dur.replace_all(&base, "").to_string();
    let re_parentheses = Regex::new(r"\([^)]*\)").unwrap();
    base = re_parentheses.replace_all(&base, "").to_string();
    base = tag_re.replace_all(&base, " ").to_string();
    let re_dash_after = Regex::new(r"\s-\s.*").unwrap();
    base = re_dash_after.replace_all(&base, "").to_string();
    let re_ws = Regex::new(r"\s+").unwrap();
    base = re_ws.replace_all(&base, " ").trim().to_string();
    (base, tags)
}

fn normalize_artist_name(artist: &str) -> String {
    if artist.is_empty() {
        return String::new();
    }
    let mut normalized = artist.to_lowercase();
    let re_brackets = Regex::new(r"\[[^\]]+\]").unwrap();
    normalized = re_brackets.replace_all(&normalized, "").to_string();
    let re_paren = Regex::new(r"\([^)]*\)").unwrap();
    normalized = re_paren.replace_all(&normalized, "").to_string();
    let parts: Vec<String> = normalized
        .split(|c: char| {
            matches!(c, '&' | ',')
        })
        .flat_map(|s| {
            s.split(|c: char| c.is_whitespace())
                .collect::<Vec<&str>>()
                .join(" ")
                .split("feat")
                .map(|p| p.trim().replace("the", "").replace("  ", " ").trim().to_string())
                .collect::<Vec<String>>()
        })
        .filter(|p| !p.is_empty())
        .collect();
    let mut sorted = parts.clone();
    sorted.sort();
    sorted.join(" ")
}

fn calculate_title_similarity(title1: &str, title2: &str) -> f64 {
    let (base1, tags1) = analyze_title(title1);
    let (base2, tags2) = analyze_title(title2);
    let dice = get_dice_coefficient(&base1, &base2);
    let max_len = base1.len().max(base2.len()) as f64;
    let lev = if max_len > 0.0 {
        1.0 - (levenshtein_distance(&base1, &base2) as f64 / max_len)
    } else {
        1.0
    };
    let base_score = dice * 0.6 + lev * 0.4;
    let mut tag_score = 0.0;
    let all_tags = tags1.union(&tags2).count();
    if all_tags > 0 {
        let common = tags1.intersection(&tags2).count();
        if common == tags1.len() && common == tags2.len() {
            tag_score = 0.1;
        } else if !tags1.is_empty() && !tags2.is_empty() && common == 0 {
            tag_score = -0.25;
        }
    } else {
        tag_score = 0.05;
    }
    (base_score + tag_score).min(1.0)
}

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

fn calculate_duration_similarity(d1: Option<f64>, d2: Option<f64>) -> f64 {
    if d1.is_none() || d2.is_none() {
        return 0.5;
    }
    let dur1 = d1.unwrap();
    let dur2 = d2.unwrap();
    let diff = (dur1 - dur2).abs();
    let avg = (dur1 + dur2) / 2.0;
    let perc = if avg > 0.0 { diff / avg } else { 0.0 };
    if diff == 0.0 { return 1.0; }
    if diff <= 3.0 || perc <= 0.02 { return 0.98; }
    if diff <= 5.0 || perc <= 0.05 { return 0.95; }
    if diff <= 10.0 || perc <= 0.08 { return 0.85; }
    if diff <= 15.0 || perc <= 0.12 { return 0.7; }
    if diff <= 30.0 || perc <= 0.20 { return 0.5; }
    let decay = (-diff / 60.0).exp();
    decay.max(0.1) * 0.4
}

/// Calculate overall song similarity for a candidate JSON-like object.
/// Candidate shape: either { attributes: { name, artistName, albumName, durationInMillis } } or a flat object.
pub fn calculate_song_similarity(candidate: &Value, query_title: &str, query_artist: &str, query_album: Option<&str>, query_duration: Option<f64>) -> ScoreInfo {
    let attrs = if candidate.get("attributes").is_some() { candidate.get("attributes").unwrap() } else { candidate };
    // Accept several common key names: Apple/JS style (name/artistName/title/artist)
    // and Musixmatch style (track_name/artist_name).
    let cand_title = attrs.get("name")
        .and_then(|v| v.as_str())
        .or_else(|| attrs.get("title").and_then(|v| v.as_str()))
        .or_else(|| attrs.get("track_name").and_then(|v| v.as_str()))
        .unwrap_or("");
    let cand_artist = attrs.get("artistName")
        .and_then(|v| v.as_str())
        .or_else(|| attrs.get("artist").and_then(|v| v.as_str()))
        .or_else(|| attrs.get("artist_name").and_then(|v| v.as_str()))
        .unwrap_or("");
    // Accept several album key names, including Musixmatch's `album_name` and `album_vanity_id`.
    let cand_album = attrs.get("albumName").and_then(|v| v.as_str())
        .or_else(|| attrs.get("album").and_then(|v| v.as_str()))
        .or_else(|| attrs.get("album_name").and_then(|v| v.as_str()))
        .or_else(|| attrs.get("album_vanity_id").and_then(|v| v.as_str()));

    let cand_duration = attrs
        .get("durationInMillis")
        .and_then(|v| v.as_f64())
        .map(|d| d / 1000.0)
        .or_else(|| attrs.get("durationMs").and_then(|v| v.as_f64()).map(|d| d / 1000.0))
        .or_else(|| attrs.get("duration").and_then(|v| v.as_f64()).map(|d| if d > 1000.0 { d / 1000.0 } else { d }))
        .or_else(|| attrs.get("track_length").and_then(|v| v.as_f64()));

    let title_score = calculate_title_similarity(cand_title, query_title);
    let artist_score = calculate_artist_similarity(cand_artist, query_artist);
    let album_score = if let (Some(qal), Some(cal)) = (query_album, cand_album) {
        get_dice_coefficient(&normalize_string(cal), &normalize_string(qal))
    } else { 0.0 };
    let duration_score = calculate_duration_similarity(cand_duration, query_duration);

    let get_importance = |score: f64| ((score - 0.5).abs() * 2.0).powf(2.0);
    let importances = vec![
        ("title", get_importance(title_score)),
        ("artist", get_importance(artist_score)),
        ("album", if query_album.is_some() { get_importance(album_score) } else { 0.0 }),
        ("duration", if query_duration.is_some() { get_importance(duration_score) } else { 0.0 }),
    ];
    let total_importance: f64 = importances.iter().map(|(_, v)| *v).sum();
    if total_importance == 0.0 {
    let mut comps = HashMap::new();
    comps.insert("titleScore".to_string(), title_score);
    comps.insert("artistScore".to_string(), artist_score);
    comps.insert("albumScore".to_string(), album_score);
    comps.insert("durationScore".to_string(), duration_score);
    let mut weights = HashMap::new();
    weights.insert("title".to_string(), 0.25);
    weights.insert("artist".to_string(), 0.25);
    weights.insert("album".to_string(), 0.25);
    weights.insert("duration".to_string(), 0.25);
    return ScoreInfo { score: 0.5, components: comps, weights, durations: { let mut d=HashMap::new(); d.insert("query".to_string(), query_duration); d.insert("candidate".to_string(), cand_duration); d } };
    }

    let mut weights = HashMap::new();
    for (k, v) in &importances {
        weights.insert(k.to_string(), v / total_importance);
    }

    let final_score = title_score * weights.get("title").copied().unwrap_or(0.0)
        + artist_score * weights.get("artist").copied().unwrap_or(0.0)
        + album_score * weights.get("album").copied().unwrap_or(0.0)
        + duration_score * weights.get("duration").copied().unwrap_or(0.0);

    let mut comps = HashMap::new();
    comps.insert("titleScore".to_string(), title_score);
    comps.insert("artistScore".to_string(), artist_score);
    comps.insert("albumScore".to_string(), album_score);
    comps.insert("durationScore".to_string(), duration_score);
    let mut durations = HashMap::new();
    durations.insert("query".to_string(), query_duration);
    durations.insert("candidate".to_string(), cand_duration);

    ScoreInfo { score: final_score.clamp(0.0, 1.0), components: comps, weights, durations }
}

/// Find the best song match among candidates.
/// Returns the index and ScoreInfo if a confident match was found.
pub fn find_best_song_match(candidates: &[Value], query_title: &str, query_artist: &str, query_album: Option<&str>, query_duration: Option<f64>) -> Option<(usize, ScoreInfo)> {
    if candidates.is_empty() || query_title.is_empty() { return None; }
    let mut valid: Vec<(usize, ScoreInfo)> = Vec::new();
    for (i, cand) in candidates.iter().enumerate() {
        // ensure candidate has title and artist
        let attrs = if cand.get("attributes").is_some() { cand.get("attributes").unwrap() } else { cand };
        // Accept several common key names here so musixmatch-style objects with
        // `track_name` / `artist_name` are not filtered out before scoring.
        let title = attrs.get("name").and_then(|v| v.as_str())
            .or_else(|| attrs.get("title").and_then(|v| v.as_str()))
            .or_else(|| attrs.get("track_name").and_then(|v| v.as_str()));
        let artist = attrs.get("artistName").and_then(|v| v.as_str())
            .or_else(|| attrs.get("artist").and_then(|v| v.as_str()))
            .or_else(|| attrs.get("artist_name").and_then(|v| v.as_str()));
        if title.is_none() || artist.is_none() { continue; }
        let score_info = calculate_song_similarity(cand, query_title, query_artist, query_album, query_duration);
        valid.push((i, score_info));
    }
    if valid.is_empty() { return None; }
    valid.sort_by(|a, b| b.1.score.partial_cmp(&a.1.score).unwrap());
    // debug printing omitted in library context
    let best = &valid[0];
    // Relaxed thresholds: allow slightly lower confidence and smaller gaps
    let confidence_threshold = 0.60;
    if best.1.score < confidence_threshold { return None; }
    if valid.len() > 1 {
        let second = &valid[1];
        let gap = best.1.score - second.1.score;
        // require a slightly larger gap or a higher top score to accept
        if gap < 0.08 && best.1.score < 0.75 { return None; }
    }
    Some((best.0, best.1.clone()))
}
