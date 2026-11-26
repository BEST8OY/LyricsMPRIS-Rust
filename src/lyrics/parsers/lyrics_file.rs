//! Custom YAML subset parser for LRCLIB `lyricsFile` (`LyricsFileParser.java`).

use crate::lyrics::parsers::lrc::{infer_missing_word_ends, parse_plain};
use crate::lyrics::types::{LyricLine, Lyrics, WordTiming, create_word_timing};

#[derive(Debug, Clone)]
enum YamlVal {
    Str(String),
    Map(Vec<(String, YamlVal)>),
    List(Vec<YamlVal>),
}

impl YamlVal {
    fn as_str(&self) -> Option<&str> {
        match self {
            YamlVal::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }
    fn as_map(&self) -> Option<&[(String, YamlVal)]> {
        match self {
            YamlVal::Map(m) => Some(m),
            _ => None,
        }
    }
    fn as_list(&self) -> Option<&[YamlVal]> {
        match self {
            YamlVal::List(l) => Some(l),
            _ => None,
        }
    }
    fn get(&self, key: &str) -> Option<&YamlVal> {
        self.as_map()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

fn unquote(raw: &str) -> String {
    let t = raw.trim();
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        return t[1..t.len() - 1].replace("''", "'");
    }
    if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') {
        let inner = &t[1..t.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some(other) => {
                        out.push('\\');
                        out.push(other);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    t.to_string()
}

fn indent_of(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

fn parse_block(lines: &[String], start: usize, base_indent: usize) -> (YamlVal, usize) {
    let is_list = lines
        .get(start)
        .is_some_and(|l| l[base_indent.min(l.len())..].starts_with("- ") || l.trim() == "-");

    if is_list {
        let mut list = Vec::new();
        let mut i = start;
        while i < lines.len() {
            let ind = indent_of(&lines[i]);
            if ind < base_indent {
                break;
            }
            if ind > base_indent {
                i += 1;
                continue;
            }
            let rest = lines[i][ind..].trim_start();
            if !rest.starts_with('-') {
                break;
            }
            let after_dash = rest[1..].trim_start();
            if after_dash.is_empty() {
                if i + 1 < lines.len() && indent_of(&lines[i + 1]) > ind {
                    let (val, next_i) = parse_block(lines, i + 1, indent_of(&lines[i + 1]));
                    list.push(val);
                    i = next_i;
                } else {
                    list.push(YamlVal::Str(String::new()));
                    i += 1;
                }
            } else if let Some((k, v)) = after_dash.split_once(':') {
                let mut map = Vec::new();
                let key = unquote(k.trim());
                let val_str = v.trim();
                let child_indent = ind + 2;
                let mut next_i = i + 1;
                if val_str.is_empty()
                    && next_i < lines.len()
                    && indent_of(&lines[next_i]) > child_indent
                {
                    let (nested, ni) = parse_block(lines, next_i, indent_of(&lines[next_i]));
                    map.push((key, nested));
                    next_i = ni;
                } else {
                    map.push((key, YamlVal::Str(unquote(val_str))));
                }
                if next_i < lines.len() && indent_of(&lines[next_i]) == child_indent {
                    let (YamlVal::Map(extra), ni) = parse_block(lines, next_i, child_indent) else {
                        unreachable!();
                    };
                    map.extend(extra);
                    next_i = ni;
                }
                list.push(YamlVal::Map(map));
                i = next_i;
            } else {
                list.push(YamlVal::Str(unquote(after_dash)));
                i += 1;
            }
        }
        (YamlVal::List(list), i)
    } else {
        let mut map = Vec::new();
        let mut i = start;
        while i < lines.len() {
            let ind = indent_of(&lines[i]);
            if ind < base_indent {
                break;
            }
            let trimmed = lines[i][ind..].trim_end();
            let Some((k, v)) = trimmed.split_once(':') else {
                i += 1;
                continue;
            };
            let key = unquote(k.trim());
            let val_str = v.trim();
            if matches!(val_str, "|" | ">" | "|-" | ">-" | "|+" | ">+") {
                let mut block = Vec::new();
                let mut ni = i + 1;
                let block_ind = if ni < lines.len() {
                    indent_of(&lines[ni])
                } else {
                    ind + 2
                };
                while ni < lines.len() && indent_of(&lines[ni]) >= block_ind && block_ind > ind {
                    block.push(lines[ni][block_ind..].to_string());
                    ni += 1;
                }
                let joined = if val_str.starts_with('>') {
                    block.join(" ")
                } else {
                    block.join("\n")
                };
                map.push((key, YamlVal::Str(joined)));
                i = ni;
            } else if val_str.is_empty() {
                if i + 1 < lines.len() && indent_of(&lines[i + 1]) > ind {
                    let (nested, ni) = parse_block(lines, i + 1, indent_of(&lines[i + 1]));
                    map.push((key, nested));
                    i = ni;
                } else {
                    map.push((key, YamlVal::Str(String::new())));
                    i += 1;
                }
            } else {
                map.push((key, YamlVal::Str(unquote(val_str))));
                i += 1;
            }
        }
        (YamlVal::Map(map), i)
    }
}

/// Parses an LRCLIB `lyricsFile` YAML document into `Lyrics`.
pub fn parse_lyrics_file(yaml: &str) -> Option<Lyrics> {
    let cleaned_lines: Vec<String> = yaml
        .lines()
        .map(|l| l.replace('\t', "    "))
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        })
        .collect();
    if cleaned_lines.is_empty() {
        return None;
    }

    let (root, _) = parse_block(&cleaned_lines, 0, indent_of(&cleaned_lines[0]));
    let mut credits = Vec::new();
    let mut instrumental = false;

    if let Some(meta) = root.get("metadata").and_then(YamlVal::as_map) {
        for (k, v) in meta {
            let s = v.as_str().unwrap_or("").trim();
            if k == "instrumental" && s.eq_ignore_ascii_case("true") {
                instrumental = true;
            } else if matches!(k.as_str(), "title" | "artist" | "album") && !s.is_empty() {
                credits.push(format!("{k}: {s}"));
            }
        }
    }

    let mut parsed_lines = Vec::new();
    let mut has_synced = false;

    if let Some(yaml_lines) = root.get("lines").and_then(YamlVal::as_list) {
        for item in yaml_lines {
            let start_ms = item
                .get("start_ms")
                .and_then(YamlVal::as_str)
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(-1);
            if start_ms >= 0 {
                has_synced = true;
            }
            let mut text = item
                .get("text")
                .and_then(YamlVal::as_str)
                .unwrap_or("")
                .trim()
                .to_string();

            let mut words: Vec<WordTiming> = Vec::new();
            let mut invalid_words = false;
            if let Some(w_list) = item.get("words").and_then(YamlVal::as_list) {
                for w in w_list {
                    let wt = w.get("text").and_then(YamlVal::as_str).unwrap_or("");
                    let ws = w
                        .get("start_ms")
                        .and_then(YamlVal::as_str)
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(-1);
                    let we = w
                        .get("end_ms")
                        .and_then(YamlVal::as_str)
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(ws);
                    if ws < 0 {
                        invalid_words = true;
                    }
                    if !wt.trim().is_empty() {
                        let mut word = create_word_timing(
                            ws.max(0) as f64 / 1000.0,
                            we.max(ws).max(0) as f64 / 1000.0,
                            wt.trim(),
                        );
                        word.ends_with_space = wt.ends_with(' ');
                        words.push(word);
                    }
                }
            }
            if text.is_empty() && !words.is_empty() {
                text = words
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            if invalid_words {
                words.clear();
            } else if !words.is_empty() {
                infer_missing_word_ends(&mut words, 0.8);
            }

            if !text.is_empty() {
                let end = words.last().map(|w| w.end);
                parsed_lines.push(
                    LyricLine::new(
                        if start_ms >= 0 {
                            start_ms as f64 / 1000.0
                        } else {
                            -1.0
                        },
                        text,
                    )
                    .with_end(end)
                    .with_words((!words.is_empty()).then_some(words)),
                );
            }
        }
    }

    if !has_synced {
        if let Some(plain) = root.get("plain").and_then(YamlVal::as_str) {
            let plain_lines = parse_plain(plain);
            if !plain_lines.is_empty() {
                let mut l = Lyrics::new(plain_lines, "LRCLIB", false);
                l.credits = credits;
                l.raw_format = Some(yaml.to_string());
                l.format_type = Some("lyricsfile.yaml".to_string());
                return Some(l);
            }
        }
        return None;
    }

    if instrumental && parsed_lines.is_empty() {
        return None;
    }

    let mut l = Lyrics::new(parsed_lines, "LRCLIB", true);
    l.credits = credits;
    l.raw_format = Some(yaml.to_string());
    l.format_type = Some("lyricsfile.yaml".to_string());
    Some(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_lyrics_file_yaml() {
        let yaml = "\
metadata:
  title: Song
  artist: Artist
lines:
  - start_ms: 1000
    text: Hello world
    words:
      - text: Hello
        start_ms: 1000
        end_ms: 1400
      - text: world
        start_ms: 1500
        end_ms: 2000
";
        let lyrics = parse_lyrics_file(yaml).unwrap();
        assert!(lyrics.synced);
        assert_eq!(lyrics.lines.len(), 1);
        assert_eq!(lyrics.lines[0].text, "Hello world");
        assert_eq!(lyrics.lines[0].words.as_ref().unwrap().len(), 2);
        assert_eq!(lyrics.credits, vec!["title: Song", "artist: Artist"]);
    }
}
