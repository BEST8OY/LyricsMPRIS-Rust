//! Multi-script Romanization engine for non-Latin lyrics.
//!
//! Supports Japanese (Kana/Kanji with furigana/Romaji rules via kakasi),
//! Chinese (Pinyin), Korean (Hangul), Cyrillic, Greek, and Arabic via deunicode.

pub(crate) mod japanese;

use crate::lyrics::LyricLine;
use deunicode::deunicode_char;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritingSystem {
    Japanese,
    Chinese,
    Korean,
    Cyrillic,
    Greek,
    Arabic,
    Other,
}

impl WritingSystem {
    pub const ALL: [WritingSystem; 7] = [
        WritingSystem::Japanese,
        WritingSystem::Chinese,
        WritingSystem::Korean,
        WritingSystem::Cyrillic,
        WritingSystem::Greek,
        WritingSystem::Arabic,
        WritingSystem::Other,
    ];
}

/// Applies Romanization annotations to lyric lines in-place.
/// Detects the dominant non-Latin writing system across all lines of the song.
pub fn apply_romanization(lines: &mut [LyricLine]) {
    let system = detect(lines.iter().map(|l| l.text.as_str()));
    let Some(system) = system else { return };

    for line in lines {
        line.romanized = convert(&line.text, system);
    }
}

/// Converts a single line of text to its Romanized form if non-Latin.
#[cfg(test)]
pub fn romanize_text(text: &str) -> Option<String> {
    let system = detect(std::iter::once(text))?;
    convert(text, system)
}

fn detect<'a>(texts: impl Iterator<Item = &'a str>) -> Option<WritingSystem> {
    let text = texts.collect::<Vec<_>>().join("\n");
    if text.chars().any(japanese::kana) {
        return Some(WritingSystem::Japanese);
    }
    dominant_system(&text)
}

fn convert(text: &str, system: WritingSystem) -> Option<String> {
    let romanized = match system {
        WritingSystem::Japanese => japanese::romanize(text),
        _ => universal(text),
    };
    let romanized = tidy(&romanized);
    if !romanized.is_empty() && romanized.to_lowercase() != tidy(text).to_lowercase() {
        Some(romanized)
    } else {
        None
    }
}

fn dominant_system(text: &str) -> Option<WritingSystem> {
    let mut counts = [0usize; WritingSystem::ALL.len()];
    for letter in text.chars().filter(|letter| non_latin_letter(*letter)) {
        counts[system_index(system_of(letter))] += 1;
    }
    counts
        .into_iter()
        .enumerate()
        .filter(|(_, count)| *count > 0)
        .max_by_key(|(_, count)| *count)
        .map(|(index, _)| WritingSystem::ALL[index])
}

fn system_of(letter: char) -> WritingSystem {
    match letter as u32 {
        0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x323af => {
            WritingSystem::Chinese
        }
        0x1100..=0x11ff | 0x3130..=0x318f | 0xa960..=0xa97f | 0xac00..=0xd7af | 0xd7b0..=0xd7ff => {
            WritingSystem::Korean
        }
        0x0400..=0x052f | 0x1c80..=0x1c8f | 0x2de0..=0x2dff | 0xa640..=0xa69f => {
            WritingSystem::Cyrillic
        }
        0x0370..=0x03ff | 0x1f00..=0x1fff => WritingSystem::Greek,
        0x0600..=0x06ff | 0x0750..=0x077f | 0x08a0..=0x08ff | 0xfb50..=0xfdff | 0xfe70..=0xfeff => {
            WritingSystem::Arabic
        }
        _ => WritingSystem::Other,
    }
}

const fn system_index(system: WritingSystem) -> usize {
    match system {
        WritingSystem::Japanese => 0,
        WritingSystem::Chinese => 1,
        WritingSystem::Korean => 2,
        WritingSystem::Cyrillic => 3,
        WritingSystem::Greek => 4,
        WritingSystem::Arabic => 5,
        WritingSystem::Other => 6,
    }
}

fn universal(text: &str) -> String {
    let mut romanized = String::new();
    for letter in text.chars() {
        if latin(letter) || letter.is_ascii() {
            romanized.push(letter);
        } else if letter == '，' {
            romanized.push_str(", ");
        } else if letter == '。' {
            romanized.push_str(". ");
        } else if let Some(trans) = deunicode_char(letter) {
            romanized.push_str(trans);
        } else {
            romanized.push(letter);
        }
    }
    romanized
}

fn non_latin_letter(letter: char) -> bool {
    letter.is_alphabetic() && !latin(letter)
}

fn latin(letter: char) -> bool {
    matches!(
        letter as u32,
        0x0041..=0x024f | 0x1d00..=0x1eff | 0x2c60..=0x2c7f | 0xa720..=0xa7ff | 0xab30..=0xab6f
    )
}

fn tidy(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .replace(" ,", ",")
                .replace(" .", ".")
                .replace(" !", "!")
                .replace(" ?", "?")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_japanese_kana_identifies_and_romanizes() {
        let system = detect(["こんにちは世界", "君のそばにいるよ"].into_iter())
            .expect("kana identifies Japanese");
        assert_eq!(system, WritingSystem::Japanese);

        let res = convert("こんにちは世界", system);
        assert_eq!(res.as_deref(), Some("konnichiwa sekai"));
    }

    #[test]
    fn test_cyrillic_romanization() {
        let res = romanize_text("Привет, мир!");
        assert_eq!(res.as_deref(), Some("Privet, mir!"));
    }

    #[test]
    fn test_chinese_transliteration() {
        let res = romanize_text("你好，世界");
        assert_eq!(res.as_deref(), Some("Ni Hao, Shi Jie"));
    }

    #[test]
    fn test_korean_transliteration() {
        let res = romanize_text("안녕하세요");
        assert_eq!(res.as_deref(), Some("annyeonghaseyo"));
    }

    #[test]
    fn test_latin_skips_romanization() {
        assert_eq!(romanize_text("Hello world!"), None);
        assert_eq!(romanize_text("Déjà vu"), None);
    }
}
