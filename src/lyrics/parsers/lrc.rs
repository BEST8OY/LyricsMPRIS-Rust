//! Standard LRC, Enhanced LRC (ELRC), and plain-text lyrics parser.
//!
//! Direct Rust port of Morphe's `LRCParser.java`.

use crate::lyrics::types::{LyricLine, WordTiming, create_word_timing};
use regex::Regex;
use std::sync::LazyLock;

static LRC_META_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\w+):([^]]*)]$").unwrap());
static SPEAKER_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(bg|v\d+|[FMD]|duet|male|female)$").unwrap());
static HEADING_SPACES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static CLEAN_CMP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"["'`.,!?:;~()（）\[\]{}<>«»“”‘’]+"#).unwrap());

const HELD_GAP_SECS: f64 = 15.0;
const MIN_HOLD_SECS: f64 = 10.0;
const HOLD_PER_CHAR_SECS: f64 = 0.180;
const GAP_LEAD_IN_SECS: f64 = 2.0;

pub fn has_outer_parentheses(text: &str) -> bool {
    let trimmed = text.trim();
    let mut chars = trimmed.chars();
    let (Some(first), Some(last)) = (chars.next(), chars.next_back()) else {
        return false;
    };
    matches!(
        (first, last),
        ('(', ')') | ('（', '）') | ('[', ']') | ('{', '}') | ('<', '>')
    )
}

pub fn clean_text_for_comparison(text: &str) -> String {
    CLEAN_CMP_RE.replace_all(text, "").trim().to_lowercase()
}

pub fn extract_credit_metadata(lrc: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in lrc.lines() {
        let line = raw.trim();
        if let Some(caps) = LRC_META_RE.captures(line)
            && let (Some(tag), Some(val)) = (caps.get(1), caps.get(2))
        {
            let key = tag.as_str().to_ascii_lowercase();
            if key == "au" || key == "by" {
                let v = val.as_str().trim();
                if !v.is_empty() {
                    out.push(format!("{}:{}", tag.as_str(), v));
                }
            }
        }
    }
    out
}

pub fn parse_synced_with_credits(lrc: &str) -> (Vec<LyricLine>, Vec<String>) {
    (parse_synced(lrc), extract_credit_metadata(lrc))
}

fn parse_fraction_secs(frac: &str) -> Option<f64> {
    if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let val: f64 = match frac.len() {
        1 => frac.parse::<f64>().ok()? * 0.1,
        2 => frac.parse::<f64>().ok()? * 0.01,
        _ => frac[..3].parse::<f64>().ok()? * 0.001,
    };
    Some(val)
}

pub fn parse_timestamp_secs(raw: &str) -> Option<f64> {
    let tag = raw.trim();
    let (min_part, rest) = tag.split_once(':')?;
    if min_part.is_empty() || !min_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mins: f64 = min_part.parse().ok()?;

    if let Some((sec_part, frac_part)) = rest.split_once(['.', ':']) {
        if sec_part.is_empty() || !sec_part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let secs: f64 = sec_part.parse().ok()?;
        let frac = parse_fraction_secs(frac_part)?;
        Some(mins * 60.0 + secs + frac)
    } else if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        let secs: f64 = rest.parse().ok()?;
        Some(mins * 60.0 + secs)
    } else {
        None
    }
}

fn is_metadata_tag(tag: &str) -> bool {
    let Some((key, _)) = tag.split_once(':') else {
        return false;
    };
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase())
}

fn parse_offset_tag_secs(tag: &str) -> Option<f64> {
    let (key, val) = tag.split_once(':')?;
    if !key.eq_ignore_ascii_case("offset") {
        return None;
    }
    let ms: f64 = val.trim().parse().ok()?;
    Some(-ms / 1000.0)
}

fn agent_marker(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'v') {
        return None;
    }
    let mut i = 1;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 1 || i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    Some(&text[..=i])
}

fn is_duet_agent(agent_id: &str) -> bool {
    if LyricLine::is_centered_agent_id(Some(agent_id)) {
        return false;
    }
    agent_id
        .bytes()
        .last()
        .is_some_and(|b| b.is_ascii_digit() && (b - b'0').is_multiple_of(2))
}

fn is_break_marker(text: &str) -> bool {
    let t = text.trim();
    t.is_empty() || t == "♪" || t == "♪♪"
}

pub fn estimate_end_secs(start: f64, next_start: Option<f64>, text: &str) -> f64 {
    let hold = MIN_HOLD_SECS.max(text.chars().count() as f64 * HOLD_PER_CHAR_SECS);
    match next_start {
        None => start + hold,
        Some(ns) if ns - start < HELD_GAP_SECS => ns,
        Some(ns) => (start + hold).min(ns - GAP_LEAD_IN_SECS),
    }
}

pub fn infer_missing_word_ends(words: &mut [WordTiming], default_last_secs: f64) {
    let len = words.len();
    for i in 0..len {
        let needs_end = words[i].end <= words[i].start;
        if needs_end {
            let inferred = if i + 1 < len && words[i + 1].start > words[i].start {
                words[i + 1].start
            } else {
                words[i].start + default_last_secs
            };
            let space = words[i].ends_with_space;
            let roma = words[i].romaji.clone();
            let ruby = words[i].ruby.clone();
            let mut rebuilt = create_word_timing(words[i].start, inferred, &words[i].text);
            rebuilt.ends_with_space = space;
            rebuilt.romaji = roma;
            rebuilt.ruby = ruby;
            words[i] = rebuilt;
        }
    }
}

fn mark_space_after(words: &mut [WordTiming]) {
    if let Some(prev) = words.last_mut() {
        prev.ends_with_space = true;
    }
}

fn add_word(words: &mut Vec<WordTiming>, start: f64, written: &str) {
    let body = written.trim();
    if body.is_empty() {
        mark_space_after(words);
        return;
    }
    if written.chars().next().is_some_and(char::is_whitespace) {
        mark_space_after(words);
    }
    let mut w = create_word_timing(start, start, body);
    w.ends_with_space = written.chars().next_back().is_some_and(char::is_whitespace);
    words.push(w);
}

fn parse_body(body: &str, line_start: f64) -> (String, Vec<WordTiming>) {
    if !body.contains('<') && !body.contains('[') {
        return (body.to_string(), Vec::new());
    }

    let mut words = Vec::new();
    let mut full = String::new();
    let mut pending_start: Option<f64> = None;
    let mut pending = String::new();
    let mut prefix = String::new();
    let mut has_token = false;

    let mut idx = 0usize;
    let len = body.len();
    while idx < len {
        let ch = body[idx..].chars().next().unwrap();
        if ch == '<' || ch == '[' {
            let close = if ch == '<' { '>' } else { ']' };
            if let Some(rel_end) = body[idx + 1..].find(close) {
                let end = idx + 1 + rel_end;
                let inner = &body[idx + 1..end];
                if let Some(t) = parse_timestamp_secs(inner) {
                    if let Some(ps) = pending_start {
                        add_word(&mut words, ps, &pending);
                    } else {
                        prefix = pending.clone();
                    }
                    pending_start = Some(t);
                    has_token = true;
                    pending.clear();
                } else {
                    full.push_str(&body[idx..=end]);
                    pending.push_str(&body[idx..=end]);
                }
                idx = end + 1;
            } else {
                full.push_str(&body[idx..]);
                pending.push_str(&body[idx..]);
                break;
            }
        } else {
            full.push(ch);
            pending.push(ch);
            idx += ch.len_utf8();
        }
    }

    if !has_token {
        return (body.to_string(), Vec::new());
    }

    if !pending.trim().is_empty() {
        if let Some(ps) = pending_start {
            add_word(&mut words, ps, &pending);
        }
    } else if let (Some(ps), Some(last)) = (pending_start, words.last_mut())
        && ps > last.start
    {
        let mut rebuilt = create_word_timing(last.start, ps, &last.text);
        rebuilt.ends_with_space = last.ends_with_space;
        *last = rebuilt;
    }

    if !prefix.trim().is_empty() {
        words.insert(0, create_word_timing(line_start, line_start, prefix.trim()));
    }

    if words.is_empty() {
        return (full.trim().to_string(), Vec::new());
    }

    infer_missing_word_ends(&mut words, 0.8);
    (full.trim().to_string(), words)
}

fn strip_agent_marker_from_words(mut words: Vec<WordTiming>, marker: &str) -> Vec<WordTiming> {
    if let Some(first) = words.first_mut()
        && let Some(rest) = first.text.strip_prefix(marker)
    {
        let rem = rest.trim_start();
        if rem.is_empty() {
            words.remove(0);
        } else {
            let mut rebuilt = create_word_timing(first.start, first.end, rem);
            rebuilt.ends_with_space = first.ends_with_space;
            rebuilt.romaji = first.romaji.clone();
            rebuilt.ruby = first.ruby.clone();
            *first = rebuilt;
        }
    }
    words
}

pub fn mark_echoes_as_background(lines: &mut [LyricLine]) {
    for i in 1..lines.len() {
        if lines[i].is_bg {
            continue;
        }
        let gap = lines[i].time - lines[i - 1].time;
        let overlaps = lines[i - 1].end.is_some_and(|e| e > lines[i].time);
        if !overlaps && !(0.0..=1.2).contains(&gap) {
            continue;
        }
        let clean_curr = clean_text_for_comparison(&lines[i].text);
        if !clean_curr.is_empty() && clean_curr == clean_text_for_comparison(&lines[i - 1].text) {
            lines[i].is_bg = true;
        }
    }
}

fn resolve_duet(lines: &mut [LyricLine]) {
    let mut first_speaker: Option<String> = None;
    let mut has_multiple = false;
    for l in lines.iter() {
        if let Some(ref sp) = l.agent_id
            && !sp.is_empty()
            && !sp.eq_ignore_ascii_case("bg")
            && !sp.eq_ignore_ascii_case("duet")
        {
            match &first_speaker {
                None => first_speaker = Some(sp.clone()),
                Some(f) if !f.eq_ignore_ascii_case(sp) => {
                    has_multiple = true;
                    break;
                }
                _ => {}
            }
        }
    }
    if !has_multiple {
        return;
    }

    let mut last_speaker: Option<String> = None;
    let mut last_duet = false;
    for l in lines.iter_mut() {
        let Some(sp) = l.agent_id.clone() else {
            continue;
        };
        if sp.is_empty() || sp.eq_ignore_ascii_case("bg") || sp.eq_ignore_ascii_case("duet") {
            continue;
        }
        let mut duet = if sp.eq_ignore_ascii_case("v1")
            || sp.eq_ignore_ascii_case("m")
            || sp.eq_ignore_ascii_case("male")
        {
            last_speaker = Some(sp.clone());
            last_duet = false;
            false
        } else if sp.eq_ignore_ascii_case("v2")
            || sp.eq_ignore_ascii_case("f")
            || sp.eq_ignore_ascii_case("female")
        {
            last_speaker = Some(sp.clone());
            last_duet = true;
            true
        } else if let Some(ref prev) = last_speaker {
            if sp.eq_ignore_ascii_case(prev) {
                last_duet
            } else {
                last_speaker = Some(sp.clone());
                last_duet = !last_duet;
                last_duet
            }
        } else {
            last_speaker = Some(sp.clone());
            false
        };

        if LyricLine::is_centered_agent_id(Some(&sp)) {
            duet = false;
        }
        l.is_duet = duet;
    }
}

struct RawEntry {
    time: f64,
    text: String,
    words: Vec<WordTiming>,
    agent_id: Option<String>,
    is_duet: bool,
    is_bg: bool,
    song_part: Option<String>,
}

/// Parses standard LRC and Enhanced LRC (ELRC) content (`LRCParser.parseSynced`).
pub fn parse_synced(lrc: &str) -> Vec<LyricLine> {
    if lrc.trim().is_empty() {
        return Vec::new();
    }

    let mut raw_entries: Vec<RawEntry> = Vec::new();
    let mut file_offset = 0.0f64;

    for raw in lrc.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        let mut timestamps = Vec::with_capacity(1);
        let mut current_speaker: Option<String> = None;
        let mut line_is_bg = false;
        let mut line_is_duet = false;
        let mut song_part: Option<String> = None;
        let mut bracketed_agent: Option<String> = None;
        let mut idx = 0usize;

        while idx < line.len() && line.as_bytes()[idx] == b'[' {
            let Some(rel_end) = line[idx + 1..].find(']') else {
                break;
            };
            let end = idx + 1 + rel_end;
            let tag = line[idx + 1..end].trim();

            if tag.eq_ignore_ascii_case("bg:1") || tag.eq_ignore_ascii_case("bg") {
                line_is_bg = true;
            } else if is_metadata_tag(tag) {
                if let Some(off) = parse_offset_tag_secs(tag) {
                    file_offset = off;
                }
            } else if tag.eq_ignore_ascii_case("duet") {
                line_is_duet = true;
            } else if SPEAKER_TAG_RE.is_match(tag) {
                current_speaker = Some(tag.to_lowercase());
            } else if let Some(ts) = parse_timestamp_secs(tag) {
                timestamps.push(ts);
            } else if agent_marker(tag).is_some() {
                if bracketed_agent.is_none() {
                    bracketed_agent = Some(tag.to_string());
                }
            } else if song_part.is_none() && !tag.is_empty() {
                song_part = Some(HEADING_SPACES_RE.replace_all(tag, " ").into_owned());
            }

            idx = end + 1;
        }

        if timestamps.is_empty() {
            continue;
        }

        let line_start = (timestamps[0] + file_offset).max(0.0);
        let (mut text, mut words) = parse_body(&line[idx..], line_start);
        text = text.trim().to_string();

        let mut agent_id = current_speaker.clone();
        let mut is_duet = line_is_duet;
        if let Some(marker) = agent_marker(&text).map(str::to_string) {
            text = text[marker.len()..].trim().to_string();
            words = strip_agent_marker_from_words(words, &marker);
            let id = marker.trim_end_matches(':').to_string();
            is_duet = is_duet_agent(&id);
            agent_id = Some(id);
        } else if let Some(ref ba) = bracketed_agent {
            let id = ba.trim_end_matches(':').to_string();
            is_duet = is_duet_agent(&id);
            agent_id = Some(id);
        } else if let Some(ref sp) = agent_id
            && sp.bytes().last().is_some_and(|b| b.is_ascii_digit())
        {
            is_duet = is_duet_agent(sp);
        }

        let is_bg = line_is_bg
            || agent_id
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("bg"))
            || has_outer_parentheses(&text);

        for (ti, ts) in timestamps.iter().enumerate() {
            let shifted = (ts + file_offset).max(0.0);
            let delta = shifted - line_start;
            let shifted_words = if ti == 0 || words.is_empty() {
                words.clone()
            } else {
                words
                    .iter()
                    .map(|w| {
                        let mut nw = create_word_timing(
                            (w.start + delta).max(0.0),
                            (w.end + delta).max(0.0),
                            &w.text,
                        );
                        nw.ends_with_space = w.ends_with_space;
                        nw
                    })
                    .collect()
            };
            raw_entries.push(RawEntry {
                time: shifted,
                text: text.clone(),
                words: shifted_words,
                agent_id: agent_id.clone(),
                is_duet,
                is_bg,
                song_part: song_part.clone(),
            });
        }
    }

    if raw_entries.is_empty() {
        return Vec::new();
    }

    raw_entries.sort_by(|a, b| a.time.total_cmp(&b.time));

    let mut lines = Vec::new();
    let count = raw_entries.len();
    for i in 0..count {
        let cur = &raw_entries[i];
        let trimmed = cur.text.trim();
        if is_break_marker(trimmed) {
            continue;
        }
        let mut end = cur.words.last().map(|w| w.end);
        if end.is_none() {
            let next = raw_entries[i + 1..]
                .iter()
                .find(|cand| cand.time > cur.time && !(!cur.is_bg && cand.is_bg));
            end = Some(match next {
                Some(n) if is_break_marker(&n.text) => n.time,
                Some(n) => estimate_end_secs(cur.time, Some(n.time), trimmed),
                None => estimate_end_secs(cur.time, None, trimmed),
            });
        }

        lines.push(LyricLine {
            time: cur.time,
            end,
            text: trimmed.to_string(),
            words: (!cur.words.is_empty()).then(|| cur.words.clone()),
            agent_id: cur.agent_id.clone(),
            is_duet: cur.is_duet,
            is_bg: cur.is_bg,
            song_part: cur.song_part.clone(),
        });
    }

    if lines.is_empty() {
        return Vec::new();
    }

    mark_echoes_as_background(&mut lines);
    resolve_duet(&mut lines);
    lines
}

/// Parses plain unsynced lyrics (treating `[Section]` lines as `song_part` headers).
pub fn parse_plain(raw: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    let mut pending_part: Option<String> = None;

    for raw_line in raw.lines() {
        let trimmed = raw_line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() > 2 {
            let inner = trimmed[1..trimmed.len() - 1].trim();
            if !inner.is_empty() {
                pending_part = Some(HEADING_SPACES_RE.replace_all(inner, " ").into_owned());
                continue;
            }
        }
        if trimmed.is_empty() {
            if !lines.is_empty() {
                lines.push(LyricLine::new(-1.0, ""));
            }
            continue;
        }
        lines.push(LyricLine::new(-1.0, trimmed).with_song_part(pending_part.take()));
    }

    while lines.last().is_some_and(|l| l.text.is_empty()) {
        lines.pop();
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_elrc_and_offset_and_bg() {
        let lrc = "\
[offset:500]
[au:Uploader]
[00:10.00][Chorus]v1:<00:10.00>Hel<00:10.20>lo <00:10.50>world<00:11.00>
[00:11.50][bg:1](echo)
";
        let (lines, credits) = parse_synced_with_credits(lrc);
        assert_eq!(credits, vec!["au:Uploader"]);
        assert_eq!(lines.len(), 2);
        assert!((lines[0].time - 9.5).abs() < 1e-6);
        assert_eq!(lines[0].song_part.as_deref(), Some("Chorus"));
        assert_eq!(lines[0].agent_id.as_deref(), Some("v1"));
        assert_eq!(lines[0].text, "Hello world");
        assert!(lines[0].has_word_timings());
        assert!(lines[1].is_bg);
    }

    #[test]
    fn test_parse_multi_timestamp_lrc() {
        let lrc = "[00:10.00][00:45.50]Repeated chorus";
        let lines = parse_synced(lrc);
        assert_eq!(lines.len(), 2);
        assert!((lines[0].time - 10.0).abs() < 1e-6);
        assert!((lines[1].time - 45.5).abs() < 1e-6);
    }
}
