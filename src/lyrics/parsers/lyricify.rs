//! Lyricify line-synced (`.lyl`), syllable-synced (`.lys`), and translation parser (`LyricifyParser.java`).

use crate::lyrics::types::{LyricLine, WordTiming, create_word_timing};
use regex::Regex;
use std::sync::LazyLock;

static LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\d+),(\d+)](.*)$").unwrap());
static SYLLABLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([^(]+)\((\d+),(\d+)\)").unwrap());
static TRANS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\d+):(\d+\.\d+)](.*)$").unwrap());

pub fn parse_lyricify_lines(text: &str, offset_ms: i64) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        let Some(caps) = LINE_RE.captures(line) else {
            continue;
        };
        let (Ok(s), Ok(e)) = (caps[1].parse::<i64>(), caps[2].parse::<i64>()) else {
            continue;
        };
        let start_ms = (s + offset_ms).max(0);
        let end_ms = (e + offset_ms).max(0);
        let body = caps[3].trim();
        if !body.is_empty() {
            lines.push(
                LyricLine::new(start_ms as f64 / 1000.0, body)
                    .with_end(Some(end_ms as f64 / 1000.0)),
            );
        }
    }
    lines.sort_by(|a, b| a.time.total_cmp(&b.time));
    lines
}

pub fn parse_lyricify_syllable(text: &str, offset_ms: i64) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut line = raw.trim();
        if line.starts_with('[')
            && let Some(close) = line.find(']')
        {
            line = &line[close + 1..];
        }
        let mut words: Vec<WordTiming> = Vec::new();
        let mut sb = String::new();
        for caps in SYLLABLE_RE.captures_iter(line) {
            let word_text = &caps[1];
            let (Ok(s), Ok(dur)) = (caps[2].parse::<i64>(), caps[3].parse::<i64>()) else {
                continue;
            };
            let start_ms = (s + offset_ms).max(0);
            let end_ms = (start_ms + dur).max(0);
            sb.push_str(word_text);
            let mut wt =
                create_word_timing(start_ms as f64 / 1000.0, end_ms as f64 / 1000.0, word_text);
            wt.ends_with_space = word_text.ends_with(' ');
            words.push(wt);
        }
        if words.is_empty() {
            continue;
        }
        let line_start = words.first().unwrap().start;
        let line_end = words.last().unwrap().end;
        lines.push(
            LyricLine::new(line_start, sb.trim())
                .with_end(Some(line_end))
                .with_words(Some(words)),
        );
    }
    lines.sort_by(|a, b| a.time.total_cmp(&b.time));
    lines
}

pub fn parse_lyricify_translation(trans: &str, offset_ms: i64) -> Vec<(f64, String)> {
    let mut entries = Vec::new();
    for raw in trans.lines() {
        let line = raw.trim();
        let Some(caps) = TRANS_RE.captures(line) else {
            continue;
        };
        let (Ok(min), Ok(sec)) = (caps[1].parse::<i64>(), caps[2].parse::<f64>()) else {
            continue;
        };
        let t_ms = (min * 60_000 + (sec * 1000.0).round() as i64 + offset_ms).max(0);
        let body = caps[3].trim();
        if !body.is_empty() {
            entries.push((t_ms as f64 / 1000.0, body.to_string()));
        }
    }
    entries.sort_by(|a, b| a.0.total_cmp(&b.0));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_lyricify_syllable() {
        let lys = "[0,1500]Hel(0,500)lo (500,500)world(1000,500)";
        let lines = parse_lyricify_syllable(lys, 0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Hello world");
        let words = lines[0].words.as_ref().unwrap();
        assert_eq!(words.len(), 3);
        assert!(words[1].ends_with_space);
    }
}
