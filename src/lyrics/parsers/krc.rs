//! Decrypted KuGou KRC and NetEase YRC word-synced parser (`KRCParser.java`).

use crate::lyrics::types::{LyricLine, WordTiming, create_word_timing};
use regex::Regex;
use std::sync::LazyLock;

static LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\d+),(\d+)](.*)$").unwrap());
static WORD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(\d+),(\d+),(\d+)>([^<]*)").unwrap());

/// Parses decrypted KRC/YRC text into word-synced `LyricLine`s.
pub fn parse_krc(krc: &str) -> Vec<LyricLine> {
    if krc.trim().is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for raw in krc.lines() {
        let line = raw.trim();
        if line.is_empty() || !line.starts_with('[') {
            continue;
        }
        let Some(caps) = LINE_RE.captures(line) else {
            continue;
        };
        let Ok(line_start_ms) = caps[1].parse::<i64>() else {
            continue;
        };
        let Ok(line_dur_ms) = caps[2].parse::<i64>() else {
            continue;
        };
        let content = &caps[3];
        let line_end_ms = line_start_ms + line_dur_ms;

        let mut words: Vec<WordTiming> = Vec::new();
        let mut full = String::new();
        let word_caps: Vec<_> = WORD_RE.captures_iter(content).collect();

        for (i, wcap) in word_caps.iter().enumerate() {
            let Ok(word_offset_ms) = wcap[1].parse::<i64>() else {
                continue;
            };
            let word_text = &wcap[4];
            if word_text.is_empty() {
                continue;
            }
            let word_start_ms = line_start_ms + word_offset_ms;
            let word_end_ms = if i + 1 < word_caps.len() {
                word_caps[i + 1][1]
                    .parse::<i64>()
                    .map(|off| line_start_ms + off)
                    .unwrap_or(line_end_ms)
            } else {
                line_end_ms
            };

            let ends_with_space = word_text
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
            let mut wt = create_word_timing(
                word_start_ms as f64 / 1000.0,
                word_end_ms.max(word_start_ms) as f64 / 1000.0,
                word_text,
            );
            wt.ends_with_space = ends_with_space;
            words.push(wt);
            full.push_str(word_text);
        }

        if words.is_empty() && !content.trim().is_empty() {
            full = content.trim().to_string();
        }
        let text = full.trim().to_string();
        if text.is_empty() {
            continue;
        }

        lines.push(
            LyricLine::new(line_start_ms as f64 / 1000.0, text)
                .with_end(Some(line_end_ms as f64 / 1000.0))
                .with_words((!words.is_empty()).then_some(words)),
        );
    }

    lines.sort_by(|a, b| a.time.total_cmp(&b.time));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_krc_words() {
        let krc = "[ti:Test]\n[1000,2000]<0,500,0>Hel<500,500,0>lo <1000,1000,0>world";
        let lines = parse_krc(krc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "Hello world");
        assert!((lines[0].time - 1.0).abs() < 1e-6);
        assert_eq!(lines[0].end, Some(3.0));
        let words = lines[0].words.as_ref().unwrap();
        assert_eq!(words.len(), 3);
        assert!((words[0].end - 1.5).abs() < 1e-6);
        assert!(words[1].ends_with_space);
    }
}
