//! Text normalization, metadata cleaning, script conversion, and wrapping utilities.

use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;
use zhconv::{Variant, zhconv};

/// Duration tolerance window (in seconds) for matching track variants.
pub(crate) const DURATION_TOLERANCE_SECS: f64 = 2.0;

/// Morphe's `DEFAULT_LYRICS_REGEX`: strips noise a provider repeats in a title
/// ("Official Video", "Remaster", "【插曲】", "- Topic", bracketed key:value tags).
pub const DEFAULT_METADATA_DECORATION_REGEX: &str = concat!(
    r#"(?i)\s*[（(\[【][^）)\]】]*?(?:official\s+)?(?:video|audio|music\s+video|lyrics?\s+video|visualizer|mv)[^）)\]】]*[）)\]】]"#,
    r#"|(?i)\s*[（(\[【][^）)\]】]*?remaster(?:ed)?(?:\s+\d{4})?[^）)\]】]*[）)\]】]"#,
    r#"|(?i)\s*[（(\[【](?:mono|stereo|hq|hd|4k|8k)[）)\]】]"#,
    r#"|[（(\[【][^）)\]】]*(?:主题曲|片尾曲|插曲|片头曲|广告曲|推广曲)[^）)\]】]*[）)\]】]"#,
    r#"|[（(\[【][^）)\]】]*[\uff1a:][^）)\]】]*[）)\]】]"#,
    r#"|(?i)\s*-\s*topic$"#
);

static DEFAULT_DECORATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(DEFAULT_METADATA_DECORATION_REGEX).unwrap());

static REMASTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:\s*[-–—]\s*|\s+[\(\[])(?:\d{4}\s*[-–—]?\s*|(?:(?:\d+(?:th|nd|rd|st)\s+)?anniversary\s+))?(?:digital\s+)?remaster(?:ed)?(?:\s+(?:version|edition|audio|mix))?(?:\s+\d{4})?[\)\]]?$"#,
    )
    .unwrap()
});

static FEAT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(?:\s+[\(\[](?:feat\.?|ft\.?|featuring|with)\s+[^()\[\]]+[\)\]]|\s*[-–—]\s*(?:feat\.?|ft\.?|featuring|with)\s+[^()\[\]]+$|\s+(?:feat\.?|ft\.?|featuring)\s+[^()\[\]]+$)"#,
    )
    .unwrap()
});

static ARTIST_SPLIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)\s*(?:和|&|feat\.?|ft\.?|,|/|×)\s*"#).unwrap());

static MULTI_SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

/// Canonicalizes typographic characters (smart quotes, dashes, non-breaking spaces)
/// and converts to lowercase trimmed string for resilient matching.
pub(crate) fn normalize(s: &str) -> String {
    let mut normalized = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '`' => normalized.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => normalized.push('"'),
            '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}' => normalized.push('-'),
            '\u{00A0}' => normalized.push(' '),
            '\u{200B}' | '\u{FEFF}' => {}
            other => normalized.push(other),
        }
    }
    normalized.trim().to_lowercase()
}

/// Returns `true` if `s` contains any CJK Unified Ideograph suitable for Han conversion.
#[inline]
pub fn has_han_char(s: &str) -> bool {
    !s.is_ascii()
        && s.chars().any(|c| {
            matches!(
                c as u32,
                0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2CEAF
            )
        })
}

/// Converts Han characters to Simplified Chinese (`zh-Hans`), folding `'妳' -> '你'`.
pub fn to_simplified_chinese(s: &str) -> String {
    if !has_han_char(s) {
        return s.to_string();
    }
    zhconv(s, Variant::ZhHans).replace('妳', "你")
}

/// Converts Han characters to Traditional Chinese (`zh-Hant`).
pub fn to_traditional_chinese(s: &str) -> String {
    if !has_han_char(s) {
        return s.to_string();
    }
    zhconv(s, Variant::ZhHant)
}

/// Folds ASCII fullwidth characters (`U+FF01..=U+FF5E`) to ASCII and strips middle dots (`·•・`).
pub fn fold_fullwidth_and_dots(s: &str) -> String {
    if s.is_ascii() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let ch = match c as u32 {
            0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
            _ => c,
        };
        if matches!(ch, '·' | '•' | '・') {
            continue;
        }
        out.push(ch);
    }
    out
}

/// Normalizes a string like Morphe's `CharactersConverter.normalize`:
/// Fullwidth-Halfwidth -> lowercase -> NFKC -> collapse whitespace.
pub fn normalize_search_text(s: &str) -> String {
    let half = fold_fullwidth_and_dots(s);
    let nfkc: String = half.nfkc().collect();
    let lower = nfkc.to_lowercase();
    MULTI_SPACE_RE.replace_all(lower.trim(), " ").into_owned()
}

/// Normalizes metadata for scoring/matching (`LyricsRequests.normalizeForMatch`):
/// NFKC -> lowercase -> Simplified Chinese -> decoration regex -> fullwidth/dots fold -> whitespace collapse.
pub fn normalize_for_match(raw: &str, custom_regex: Option<&Regex>) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let nfkc: String = trimmed.nfkc().collect();
    let lower = nfkc.to_lowercase();
    let simp = to_simplified_chinese(&lower);
    let stripped = match custom_regex {
        Some(re) => re.replace_all(&simp, "").into_owned(),
        None => DEFAULT_DECORATION_RE.replace_all(&simp, "").into_owned(),
    };
    let folded = fold_fullwidth_and_dots(&stripped);
    MULTI_SPACE_RE.replace_all(folded.trim(), " ").into_owned()
}

/// Applies Morphe's decoration regex while preserving original fullwidth punctuation if nothing matched.
pub fn apply_decoration_regex_preserve_original(
    input: &str,
    custom_regex: Option<&Regex>,
) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let re = custom_regex.unwrap_or(&DEFAULT_DECORATION_RE);
    let nfkc: String = trimmed.nfkc().collect();
    let half = fold_fullwidth_and_dots(&nfkc);
    if !re.is_match(&half) {
        return trimmed.to_string();
    }
    let cleaned = re.replace_all(&half, "");
    MULTI_SPACE_RE.replace_all(cleaned.trim(), " ").into_owned()
}

/// Strips featured artist suffixes & trailing collaborator lists from artist strings (`MetadataCleaner.cleanArtist`).
pub fn clean_artist(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    let seps = [
        " & ", ", ", " x ", " X ", " feat. ", " feat ", " ft. ", " ft ", " с ", " 和 ", "/", "×",
    ];
    let mut earliest: Option<usize> = None;
    for sep in seps {
        if let Some(idx) = s.find(sep)
            && idx > 0
        {
            earliest = Some(earliest.map_or(idx, |e| e.min(idx)));
        }
    }
    if let Some(idx) = earliest {
        s.truncate(idx);
    }
    apply_decoration_regex_preserve_original(&s, None)
}

/// Splits multi-artist strings into individual artist names (`MetadataCleaner.splitArtists`).
pub fn split_artists(artist: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for part in ARTIST_SPLIT_RE.split(artist) {
        let p = part.trim();
        if !p.is_empty() && seen.insert(p.to_lowercase()) {
            out.push(p.to_string());
        }
    }
    out
}

/// Cleans `(title, artist)` via `MetadataCleaner.parseCleanTitleAndArtist` rules.
#[allow(dead_code)]
pub fn parse_clean_title_and_artist(
    raw_title: &str,
    raw_artist: &str,
    custom_regex: Option<&Regex>,
) -> (String, String) {
    let title = apply_decoration_regex_preserve_original(raw_title, custom_regex);
    let title = clean_title_annotations(&title).unwrap_or(title);
    let artist = apply_decoration_regex_preserve_original(raw_artist, custom_regex);
    let artist = clean_artist(&artist);
    (title, artist)
}

/// Checks if `title` has a `"left - right"` dash split where one side matches `raw_artist`.
pub fn trusted_dash_split(title: &str, raw_artist: &str) -> Option<(String, String)> {
    let (left, right) = title.split_once(" - ")?;
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() || right.is_empty() {
        return None;
    }
    let norm_artist = normalize_for_match(raw_artist, None);
    let norm_left = normalize_for_match(left, None);
    let norm_right = normalize_for_match(right, None);
    if !norm_artist.is_empty()
        && (norm_left.contains(&norm_artist) || norm_artist.contains(&norm_left))
    {
        return Some((right.to_string(), left.to_string()));
    }
    if !norm_artist.is_empty()
        && (norm_right.contains(&norm_artist) || norm_artist.contains(&norm_right))
    {
        return Some((left.to_string(), right.to_string()));
    }
    None
}

/// Splits `"Artist - Title"` unconditionally on `" - "`.
pub fn any_dash_split(title: &str) -> Option<(String, String)> {
    let (left, right) = title.split_once(" - ")?;
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some((right.to_string(), left.to_string()))
}

/// Splits `"Title - Artist"` unconditionally on `" - "`.
pub fn any_dash_split_reversed(title: &str) -> Option<(String, String)> {
    let (left, right) = title.split_once(" - ")?;
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() || right.is_empty() {
        return None;
    }
    Some((left.to_string(), right.to_string()))
}

/// Generates deduplicated script variants (Traditional / Simplified Chinese, NFKC) for fallback queries.
pub fn script_variants(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(input.to_string());

    for candidate in [
        to_simplified_chinese(input),
        to_traditional_chinese(input),
        input.nfkc().collect::<String>(),
    ] {
        let trimmed = candidate.trim().to_string();
        if !trimmed.is_empty() && seen.insert(trimmed.clone()) {
            out.push(trimmed);
        }
    }
    out
}

/// Strips common remaster annotations from track titles, returning the clean base title if modified.
pub(crate) fn strip_remaster_suffix(title: &str) -> Option<String> {
    if REMASTER_RE.is_match(title) {
        let stripped = REMASTER_RE.replace(title, "");
        let clean = stripped.trim();
        if !clean.is_empty() && clean != title {
            return Some(clean.to_string());
        }
    }
    None
}

/// Strips common featured artist annotations from titles or artists, returning the clean string if modified.
pub(crate) fn strip_feat_suffix(title: &str) -> Option<String> {
    if FEAT_RE.is_match(title) {
        let stripped = FEAT_RE.replace(title, "");
        let clean = stripped.trim();
        if !clean.is_empty() && clean != title {
            return Some(clean.to_string());
        }
    }
    None
}

/// Strips all common title annotations (remaster versions, anniversary editions, and featured artists).
pub(crate) fn clean_title_annotations(title: &str) -> Option<String> {
    let mut current = title.to_string();
    let mut changed = false;

    for _ in 0..3 {
        let mut round_changed = false;
        if let Some(clean) = strip_remaster_suffix(&current) {
            current = clean;
            changed = true;
            round_changed = true;
        }
        if let Some(clean) = strip_feat_suffix(&current) {
            current = clean;
            changed = true;
            round_changed = true;
        }
        if !round_changed {
            break;
        }
    }

    if changed && !current.is_empty() && current != title {
        Some(current)
    } else {
        None
    }
}

/// Wrap text to a given width, preserving empty lines and not splitting words
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            result.push(String::new());
            continue;
        }
        let wrapped = textwrap::wrap(line, width);
        for w in wrapped {
            result.push(w.to_string());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_punctuation_canonicalization() {
        assert_eq!(normalize("Don’t Stop"), "don't stop");
        assert_eq!(normalize("‘Don’t’"), "'don't'");
        assert_eq!(normalize("Don`t Stop"), "don't stop");
        assert_eq!(normalize("“Heroes”"), "\"heroes\"");
        assert_eq!(normalize("„Heroes‟"), "\"heroes\"");
        assert_eq!(normalize("Artist – Song"), "artist - song");
        assert_eq!(normalize("Artist — Song"), "artist - song");
        assert_eq!(normalize("Artist ― Song"), "artist - song");
        assert_eq!(normalize("Song\u{00A0}Title"), "song title");
        assert_eq!(normalize("Song\u{200B}Title"), "songtitle");
        assert_eq!(normalize("Song\u{FEFF}Title"), "songtitle");
    }

    #[test]
    fn test_strip_remaster_suffix() {
        assert_eq!(
            strip_remaster_suffix("Heroes - 2015 Remaster"),
            Some("Heroes".to_string())
        );
        assert_eq!(
            strip_remaster_suffix("Heroes (40th Anniversary Remaster)"),
            Some("Heroes".to_string())
        );
        assert_eq!(strip_remaster_suffix("Heroes (Live at Wembley)"), None);
    }

    #[test]
    fn test_strip_feat_suffix_and_clean_annotations() {
        assert_eq!(
            strip_feat_suffix("Beauty And A Beat (feat. Nicki Minaj)"),
            Some("Beauty And A Beat".to_string())
        );
        assert_eq!(strip_feat_suffix("Stay With Me"), None);
        assert_eq!(
            clean_title_annotations("Heroes (feat. Brian Eno) [2011 Remaster]"),
            Some("Heroes".to_string())
        );
    }

    #[test]
    fn test_normalize_for_match_and_dash_splits() {
        assert_eq!(
            normalize_for_match("Song (Official Video) [4K]", None),
            "song"
        );
        assert_eq!(normalize_for_match("Artist - Topic", None), "artist");
        assert_eq!(
            trusted_dash_split("Daft Punk - Get Lucky", "Daft Punk"),
            Some(("Get Lucky".to_string(), "Daft Punk".to_string()))
        );
    }
}
