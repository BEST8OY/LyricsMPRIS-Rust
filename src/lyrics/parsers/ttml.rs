//! Apple Music / AMLL / lrc.red / BetterLyrics TTML2 parser (`TTMLParser.java`).

use crate::lyrics::filter::is_cjk_ideograph;
use crate::lyrics::types::{LyricLine, Lyrics, WordTiming, create_word_timing};
use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static TIME_UNIT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(-?\d+(?:\.\d+)?)(ms|h|m|s)$").unwrap());
static TIME_COLON_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:(\d+):)?(?:(\d+):)?(\d+(?:\.\d+)?)$").unwrap());
static TIME_SMPTE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+):(\d+):(\d+):(\d+(?:\.\d+)?)$").unwrap());
static MULTI_SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

const DEFAULT_FRAME_RATE: f64 = 30.0;
const MIN_IOU: f64 = 0.1;
const FAST_TRACK_SECS: f64 = 0.002;

const AMLL_EXCLUDED_KEYS: &[&str] = &[
    "ncmMusicId",
    "qqMusicId",
    "spotifyId",
    "appleMusicId",
    "isrc",
    "ttmlAuthorGithub",
];
const COMPOSER_EXCLUDED_KEYS: &[&str] =
    &["isrc", "songwriter", "songwriters", "ytVideoId", "language"];

#[derive(Clone, Debug)]
struct RomajiSyllable {
    begin: f64,
    end: f64,
    text: String,
}

#[derive(Clone, Debug)]
struct SidecarTranslation {
    main_text: String,
    bg_text: Option<String>,
}

#[derive(Clone, Debug)]
struct AgentInfo {
    agent_type: String,
    name: Option<String>,
}

#[derive(Default)]
struct HeadMetadata {
    songwriters: Vec<String>,
    credits: Vec<String>,
    agent_types: HashMap<String, AgentInfo>,
    sidecar_roman: HashMap<String, Vec<RomajiSyllable>>,
    sidecar_trans: HashMap<String, SidecarTranslation>,
    about: Option<String>,
    offset_secs: f64,
}

fn local_name(name: &[u8]) -> &str {
    let s = std::str::from_utf8(name).unwrap_or("");
    s.rsplit(':').next().unwrap_or(s)
}

fn attr_value(e: &BytesStart<'_>, wanted_locals: &[&str]) -> Option<String> {
    for a in e.attributes().flatten() {
        let raw_key = std::str::from_utf8(a.key.as_ref()).unwrap_or("");
        let local = raw_key.rsplit(':').next().unwrap_or(raw_key);
        if wanted_locals.iter().any(|&w| w == raw_key || w == local)
            && let Ok(v) = a.unescape_value()
        {
            return Some(v.into_owned());
        }
    }
    None
}

fn attr_prefixed(e: &BytesStart<'_>, full_keys: &[&str], local_fallback: &str) -> Option<String> {
    for fk in full_keys {
        for a in e.attributes().flatten() {
            let raw_key = std::str::from_utf8(a.key.as_ref()).unwrap_or("");
            if raw_key == *fk {
                return a.unescape_value().ok().map(|v| v.into_owned());
            }
        }
    }
    attr_value(e, &[local_fallback])
}

pub fn parse_ttml_time(raw: Option<&str>, frame_rate: f64) -> f64 {
    let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return 0.0;
    };
    if s.eq_ignore_ascii_case("indefinite") {
        return 0.0;
    }
    if let Some(caps) = TIME_UNIT_RE.captures(s)
        && let Ok(val) = caps[1].parse::<f64>()
    {
        return match &caps[2] {
            "ms" => (val / 1000.0).max(0.0),
            "s" => val.max(0.0),
            "m" => (val * 60.0).max(0.0),
            "h" => (val * 3600.0).max(0.0),
            _ => 0.0,
        };
    }
    if let Some(caps) = TIME_SMPTE_RE.captures(s) {
        let h: f64 = caps[1].parse().unwrap_or(0.0);
        let m: f64 = caps[2].parse().unwrap_or(0.0);
        let sec: f64 = caps[3].parse().unwrap_or(0.0);
        let ff: f64 = caps[4].parse().unwrap_or(0.0);
        return (h * 3600.0 + m * 60.0 + sec + ff / frame_rate.max(1.0)).max(0.0);
    }
    if let Some(caps) = TIME_COLON_RE.captures(s) {
        let (h, m, sec) = match (caps.get(1), caps.get(2), caps.get(3)) {
            (Some(a), Some(b), Some(c)) => (
                a.as_str().parse::<f64>().unwrap_or(0.0),
                b.as_str().parse::<f64>().unwrap_or(0.0),
                c.as_str().parse::<f64>().unwrap_or(0.0),
            ),
            (Some(a), None, Some(c)) => (
                0.0,
                a.as_str().parse::<f64>().unwrap_or(0.0),
                c.as_str().parse::<f64>().unwrap_or(0.0),
            ),
            _ => (
                0.0,
                0.0,
                caps.get(3)
                    .and_then(|m| m.as_str().parse::<f64>().ok())
                    .unwrap_or(0.0),
            ),
        };
        return (h * 3600.0 + m * 60.0 + sec).max(0.0);
    }
    s.parse::<f64>().unwrap_or(0.0).max(0.0)
}

fn resolve_end_time(
    begin: f64,
    end_attr: Option<&str>,
    dur_attr: Option<&str>,
    frame_rate: f64,
) -> f64 {
    if let Some(e) = end_attr.filter(|s| !s.trim().is_empty()) {
        return parse_ttml_time(Some(e), frame_rate);
    }
    if let Some(d) = dur_attr.filter(|s| !s.trim().is_empty()) {
        return begin + parse_ttml_time(Some(d), frame_rate);
    }
    begin
}

fn has_timing(begin: Option<&str>, end: Option<&str>, dur: Option<&str>) -> bool {
    begin.is_some_and(|s| !s.trim().is_empty())
        && (end.is_some_and(|s| !s.trim().is_empty()) || dur.is_some_and(|s| !s.trim().is_empty()))
}

fn normalize_text(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' || c == '\r' || c == '\t' {
            let prev = i.checked_sub(1).and_then(|p| chars.get(p).copied());
            let next = chars.get(i + 1).copied();
            if prev.is_some_and(is_cjk_ideograph) && next.is_some_and(is_cjk_ideograph) {
                continue;
            }
            cleaned.push(' ');
        } else {
            cleaned.push(c);
        }
    }
    MULTI_SPACE_RE.replace_all(cleaned.trim(), " ").into_owned()
}

fn normalize_text_raw(raw: &str) -> String {
    let replaced = raw.replace(['\r', '\n', '\t'], " ");
    MULTI_SPACE_RE.replace_all(&replaced, " ").into_owned()
}

fn read_text_content(reader: &mut Reader<&[u8]>) -> String {
    let mut out = String::new();
    let mut depth = 1usize;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.unescape() {
                    out.push_str(&s);
                }
            }
            Ok(Event::CData(c)) => {
                out.push_str(&String::from_utf8_lossy(c.as_ref()));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

fn skip_element(reader: &mut Reader<&[u8]>) {
    let _ = read_text_content(reader);
}

fn parse_head_metadata(ttml: &str) -> HeadMetadata {
    let mut hm = HeadMetadata::default();
    let mut reader = Reader::from_str(ttml);
    reader.config_mut().trim_text(false);

    let mut buf = Vec::new();
    let mut in_head = false;
    let mut in_songwriters = false;
    let mut in_agent = false;
    let mut in_itunes_meta = false;
    let mut agent_id: Option<String> = None;
    let mut agent_type: Option<String> = None;
    let mut agent_name: Option<String> = None;
    let mut in_transliteration_lang: Option<String> = None;
    let mut in_translation_lang: Option<String> = None;

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if local == "head" {
                    in_head = true;
                } else if !in_head {
                    continue;
                } else if local == "songwriters" {
                    in_songwriters = true;
                } else if in_songwriters && local == "songwriter" {
                    let n = read_text_content(&mut reader).trim().to_string();
                    if !n.is_empty() {
                        hm.songwriters.push(n);
                    }
                } else if local == "meta" {
                    if let (Some(k), Some(v)) =
                        (attr_value(&e, &["key"]), attr_value(&e, &["value"]))
                    {
                        let k = k.trim();
                        let v = v.trim();
                        if !k.is_empty()
                            && !v.is_empty()
                            && !AMLL_EXCLUDED_KEYS.contains(&k)
                            && !COMPOSER_EXCLUDED_KEYS.contains(&k)
                        {
                            let entry = format!("{k}: {v}");
                            if !hm.credits.contains(&entry) {
                                hm.credits.push(entry);
                            }
                        }
                    }
                } else if local == "agent" {
                    agent_id = attr_prefixed(&e, &["xml:id", "ttm:id"], "id");
                    agent_type = attr_prefixed(&e, &["ttm:type"], "type");
                    agent_name = None;
                    in_agent = true;
                } else if in_agent && local == "name" {
                    let n = read_text_content(&mut reader).trim().to_string();
                    if !n.is_empty() {
                        agent_name = Some(n);
                    }
                } else if local.eq_ignore_ascii_case("itunesmetadata") {
                    in_itunes_meta = true;
                } else if in_itunes_meta && local == "audio" {
                    if let Some(off) = attr_prefixed(&e, &["itunes:lyricOffset"], "lyricOffset")
                        && let Ok(secs) = off.trim().parse::<f64>()
                    {
                        hm.offset_secs = secs;
                    }
                } else if local == "transliteration" {
                    in_transliteration_lang = attr_prefixed(&e, &["xml:lang"], "lang");
                } else if local == "translation" {
                    in_translation_lang = attr_prefixed(&e, &["xml:lang"], "lang");
                } else if local == "text"
                    && (in_transliteration_lang.is_some() || in_translation_lang.is_some())
                {
                    let for_key = attr_value(&e, &["for"]);
                    if let Some(fk) = for_key.filter(|s| !s.is_empty()) {
                        if let Some(ref lang) = in_transliteration_lang {
                            let syms = parse_sidecar_transliteration_text(&mut reader);
                            if !syms.is_empty() {
                                let key = if lang.is_empty() {
                                    fk
                                } else {
                                    format!("{lang}:{fk}")
                                };
                                hm.sidecar_roman.insert(key, syms);
                            }
                        } else if let Some(ref lang) = in_translation_lang {
                            let st = parse_sidecar_translation_text(&mut reader);
                            if !st.main_text.is_empty() || st.bg_text.is_some() {
                                hm.sidecar_trans.insert(format!("{lang}:{fk}"), st);
                            }
                        } else {
                            skip_element(&mut reader);
                        }
                    } else {
                        skip_element(&mut reader);
                    }
                } else if local == "desc" && hm.about.is_none() {
                    let d = read_text_content(&mut reader).trim().to_string();
                    if !d.is_empty() {
                        hm.about = Some(d);
                    }
                } else if local == "credits" {
                    let c = read_text_content(&mut reader).trim().to_string();
                    if !c.is_empty() && !hm.credits.contains(&c) {
                        hm.credits.push(c);
                    }
                }
            }
            Ok(Event::Empty(e)) if in_head => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if local == "meta" {
                    if let (Some(k), Some(v)) =
                        (attr_value(&e, &["key"]), attr_value(&e, &["value"]))
                    {
                        let k = k.trim();
                        let v = v.trim();
                        if !k.is_empty()
                            && !v.is_empty()
                            && !AMLL_EXCLUDED_KEYS.contains(&k)
                            && !COMPOSER_EXCLUDED_KEYS.contains(&k)
                        {
                            let entry = format!("{k}: {v}");
                            if !hm.credits.contains(&entry) {
                                hm.credits.push(entry);
                            }
                        }
                    }
                } else if in_itunes_meta && local == "audio" {
                    if let Some(off) = attr_prefixed(&e, &["itunes:lyricOffset"], "lyricOffset")
                        && let Ok(secs) = off.trim().parse::<f64>()
                    {
                        hm.offset_secs = secs;
                    }
                } else if local == "agent" {
                    let id = attr_prefixed(&e, &["xml:id", "ttm:id"], "id");
                    let ty = attr_prefixed(&e, &["ttm:type"], "type");
                    if let (Some(i), Some(t)) = (id, ty) {
                        hm.agent_types.insert(
                            i,
                            AgentInfo {
                                agent_type: t,
                                name: None,
                            },
                        );
                    }
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if local == "head" {
                    break;
                } else if local == "songwriters" {
                    in_songwriters = false;
                } else if local == "agent" && in_agent {
                    if let (Some(i), Some(t)) = (agent_id.take(), agent_type.take()) {
                        hm.agent_types.insert(
                            i,
                            AgentInfo {
                                agent_type: t,
                                name: agent_name.take(),
                            },
                        );
                    }
                    in_agent = false;
                } else if local.eq_ignore_ascii_case("itunesmetadata") {
                    in_itunes_meta = false;
                } else if local == "transliteration" {
                    in_transliteration_lang = None;
                } else if local == "translation" {
                    in_translation_lang = None;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    hm
}

fn parse_sidecar_transliteration_text(reader: &mut Reader<&[u8]>) -> Vec<RomajiSyllable> {
    let mut syms = Vec::new();
    let mut depth = 1usize;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if local == "span" {
                    let b = attr_value(&e, &["begin"]);
                    let end = attr_value(&e, &["end"]);
                    let dur = attr_value(&e, &["dur"]);
                    if has_timing(b.as_deref(), end.as_deref(), dur.as_deref()) {
                        let txt = normalize_text(&read_text_content(reader));
                        if !txt.is_empty() {
                            let begin_s = parse_ttml_time(b.as_deref(), DEFAULT_FRAME_RATE);
                            let end_s = resolve_end_time(
                                begin_s,
                                end.as_deref(),
                                dur.as_deref(),
                                DEFAULT_FRAME_RATE,
                            );
                            syms.push(RomajiSyllable {
                                begin: begin_s,
                                end: end_s,
                                text: txt,
                            });
                        }
                    } else {
                        skip_element(reader);
                    }
                } else {
                    depth += 1;
                }
            }
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(raw) = t.unescape() {
                    let trimmed = normalize_text(&raw);
                    if !trimmed.is_empty() {
                        syms.push(RomajiSyllable {
                            begin: 0.0,
                            end: 0.0,
                            text: trimmed,
                        });
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    syms
}

fn parse_sidecar_translation_text(reader: &mut Reader<&[u8]>) -> SidecarTranslation {
    let mut main_buf = String::new();
    let mut bg_buf = String::new();
    let mut in_bg = false;
    let mut bg_depth = 0usize;
    let mut depth = 1usize;
    let mut buf = Vec::new();

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                let qname = e.name();
                let local = local_name(qname.as_ref());
                let role = attr_prefixed(&e, &["ttm:role"], "role");
                if role.as_deref() == Some("x-bg") {
                    in_bg = true;
                    bg_depth = depth;
                } else if local == "br" {
                    if in_bg {
                        bg_buf.push(' ');
                    } else {
                        main_buf.push(' ');
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == "br" {
                    if in_bg {
                        bg_buf.push(' ');
                    } else {
                        main_buf.push(' ');
                    }
                }
            }
            Ok(Event::End(_)) => {
                if in_bg && depth == bg_depth {
                    in_bg = false;
                }
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(raw) = t.unescape() {
                    if in_bg {
                        bg_buf.push_str(&raw);
                    } else {
                        main_buf.push_str(&raw);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    let main_text = normalize_text(&main_buf);
    let bg_text = normalize_text(&bg_buf);
    SidecarTranslation {
        main_text,
        bg_text: (!bg_text.is_empty()).then_some(bg_text),
    }
}

fn find_sidecar_roman<'a>(
    line_id: Option<&str>,
    map: &'a HashMap<String, Vec<RomajiSyllable>>,
) -> Option<&'a Vec<RomajiSyllable>> {
    let id = line_id?.trim();
    if id.is_empty() {
        return None;
    }
    if let Some(exact) = map.get(id) {
        return Some(exact);
    }
    let suffix = format!(":{id}");
    map.iter()
        .find(|(k, _)| k.ends_with(&suffix))
        .map(|(_, v)| v)
}

fn align_romaji_to_words(words: &mut [WordTiming], sidecar: &[RomajiSyllable]) {
    if words.is_empty() || sidecar.is_empty() {
        return;
    }
    for w in words.iter_mut() {
        let mut matched: Vec<&str> = Vec::new();
        for s in sidecar {
            let sub_dur = (s.end - s.begin).max(0.0);
            if sub_dur <= 0.0 {
                continue;
            }
            let inter_start = w.start.max(s.begin);
            let inter_end = w.end.min(s.end);
            let inter = (inter_end - inter_start).max(0.0);
            if ((w.start - s.begin).abs() <= FAST_TRACK_SECS && inter > 0.0)
                || inter * 2.0 >= sub_dur
            {
                matched.push(&s.text);
            }
        }
        if matched.is_empty() {
            let mut best_iou = 0.0;
            let mut best_text = None;
            for s in sidecar {
                let inter = (w.end.min(s.end) - w.start.max(s.begin)).max(0.0);
                let union = (w.end.max(s.end) - w.start.min(s.begin)).max(0.001);
                let iou = inter / union;
                if iou >= MIN_IOU && iou > best_iou {
                    best_iou = iou;
                    best_text = Some(s.text.as_str());
                }
            }
            if let Some(bt) = best_text {
                w.romaji = Some(bt.to_string());
            }
        } else {
            w.romaji = Some(matched.join(" "));
        }
    }
}

struct ParsedPElement {
    begin: f64,
    end: f64,
    text: String,
    words: Vec<WordTiming>,
    bg_lines: Vec<LyricLine>,
    bg_agent: Option<String>,
    inline_trans: HashMap<String, String>,
    inline_roma: HashMap<String, String>,
    bg_inline_trans: HashMap<String, String>,
    bg_inline_roma: HashMap<String, String>,
}

fn strip_bg_parens(s: &str) -> String {
    let mut t = s.trim();
    loop {
        if t.starts_with('(') && t.ends_with(')') && t.len() >= 2 {
            t = t[1..t.len() - 1].trim();
        } else if t.starts_with('（') && t.ends_with('）') && t.chars().count() >= 2 {
            t = t['（'.len_utf8()..t.len() - '）'.len_utf8()].trim();
        } else {
            break;
        }
    }
    t.to_string()
}

fn parse_p_element(
    reader: &mut Reader<&[u8]>,
    p_begin: f64,
    p_end: f64,
    frame_rate: f64,
) -> ParsedPElement {
    let mut words: Vec<WordTiming> = Vec::new();
    let mut full_text = String::new();
    let mut bg_lines = Vec::new();
    let mut bg_agent: Option<String> = None;

    let mut inline_trans: HashMap<String, String> = HashMap::new();
    let mut inline_roma: HashMap<String, String> = HashMap::new();
    let mut bg_inline_trans: HashMap<String, String> = HashMap::new();
    let mut bg_inline_roma: HashMap<String, String> = HashMap::new();

    let mut in_word = false;
    let mut word_depth = 0usize;
    let mut word_buf = String::new();
    let mut word_begin = 0.0;
    let mut word_end = 0.0;
    let mut word_ruby: Option<String> = None;

    let mut in_bg = false;
    let mut bg_depth = 0usize;
    let mut bg_begin = p_begin;
    let mut bg_end = p_end;
    let mut bg_words: Vec<WordTiming> = Vec::new();
    let mut bg_full_text = String::new();
    let mut in_bg_word = false;
    let mut bg_word_depth = 0usize;
    let mut bg_word_buf = String::new();
    let mut bg_word_begin = 0.0;
    let mut bg_word_end = 0.0;

    let mut in_trans: Option<(usize, String, bool)> = None;
    let mut trans_buf = String::new();
    let mut in_roma: Option<(usize, String, bool)> = None;
    let mut roma_buf = String::new();

    let mut in_ruby_base: Option<usize> = None;
    let mut ruby_base_buf = String::new();
    let mut in_ruby_text: Option<usize> = None;
    let mut ruby_text_buf = String::new();
    let mut ruby_container_depth: Option<(usize, f64, f64)> = None;

    let mut depth = 1usize;
    let mut buf = Vec::new();

    let push_word = |target: &mut Vec<WordTiming>,
                     full: &mut String,
                     raw_buf: &str,
                     wb: f64,
                     we: f64,
                     ruby: Option<String>| {
        let normalized = normalize_text_raw(raw_buf);
        let starts_space = normalized.chars().next().is_some_and(char::is_whitespace);
        let ends_space = normalized
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let txt = normalized.trim();
        full.push_str(&normalized);
        if starts_space && let Some(prev) = target.last_mut() {
            prev.ends_with_space = true;
        }
        if !txt.is_empty() {
            let mut w = create_word_timing(wb, we, txt);
            w.ends_with_space = ends_space;
            w.ruby = ruby;
            target.push(w);
        }
    };

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                depth += 1;
                let qname = e.name();
                let local = local_name(qname.as_ref());
                let role = attr_prefixed(&e, &["ttm:role"], "role");
                let ruby_role = attr_prefixed(&e, &["tts:ruby"], "ruby");

                if local == "br" {
                    if in_word {
                        word_buf.push(' ');
                    } else if in_bg_word {
                        bg_word_buf.push(' ');
                    } else if in_bg {
                        bg_full_text.push(' ');
                    } else {
                        full_text.push(' ');
                    }
                    continue;
                }

                if role.as_deref() == Some("x-bg") && !in_bg {
                    in_bg = true;
                    bg_depth = depth;
                    bg_words.clear();
                    bg_full_text.clear();
                    bg_agent = attr_prefixed(&e, &["ttm:agent"], "agent");
                    let b = attr_value(&e, &["begin"]);
                    let end = attr_value(&e, &["end"]);
                    let dur = attr_value(&e, &["dur"]);
                    bg_begin = b
                        .as_deref()
                        .map(|s| parse_ttml_time(Some(s), frame_rate))
                        .unwrap_or(p_begin);
                    bg_end = if has_timing(b.as_deref(), end.as_deref(), dur.as_deref()) {
                        resolve_end_time(bg_begin, end.as_deref(), dur.as_deref(), frame_rate)
                    } else {
                        p_end
                    };
                } else if role.as_deref() == Some("x-translation") {
                    let lang = attr_prefixed(&e, &["xml:lang"], "lang")
                        .unwrap_or_else(|| "default".to_string());
                    in_trans = Some((depth, lang, in_bg));
                    trans_buf.clear();
                } else if role.as_deref() == Some("x-roman") {
                    let lang = attr_prefixed(&e, &["xml:lang"], "lang")
                        .unwrap_or_else(|| "default".to_string());
                    in_roma = Some((depth, lang, in_bg));
                    roma_buf.clear();
                } else if ruby_role.as_deref() == Some("container") || local == "ruby" {
                    let b = attr_value(&e, &["begin"]);
                    let end = attr_value(&e, &["end"]);
                    let dur = attr_value(&e, &["dur"]);
                    let rb = b
                        .as_deref()
                        .map(|s| parse_ttml_time(Some(s), frame_rate))
                        .unwrap_or(p_begin);
                    let re = resolve_end_time(rb, end.as_deref(), dur.as_deref(), frame_rate);
                    ruby_container_depth = Some((depth, rb, re));
                    ruby_base_buf.clear();
                    ruby_text_buf.clear();
                } else if ruby_role.as_deref() == Some("base") || local == "rb" {
                    in_ruby_base = Some(depth);
                } else if ruby_role.as_deref() == Some("text") || local == "rt" {
                    in_ruby_text = Some(depth);
                } else if local == "span" {
                    let b = attr_value(&e, &["begin"]);
                    let end = attr_value(&e, &["end"]);
                    let dur = attr_value(&e, &["dur"]);
                    if has_timing(b.as_deref(), end.as_deref(), dur.as_deref()) {
                        let wb = parse_ttml_time(b.as_deref(), frame_rate);
                        let we = resolve_end_time(wb, end.as_deref(), dur.as_deref(), frame_rate);
                        if in_bg && !in_bg_word {
                            in_bg_word = true;
                            bg_word_depth = depth;
                            bg_word_buf.clear();
                            bg_word_begin = wb;
                            bg_word_end = we;
                        } else if !in_bg && !in_word {
                            in_word = true;
                            word_depth = depth;
                            word_buf.clear();
                            word_begin = wb;
                            word_end = we;
                            word_ruby = None;
                        }
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                if local_name(e.name().as_ref()) == "br" {
                    if in_bg {
                        if let Some(prev) = bg_words.last_mut() {
                            prev.ends_with_space = true;
                        }
                        if !bg_full_text.ends_with(' ') {
                            bg_full_text.push(' ');
                        }
                    } else {
                        if let Some(prev) = words.last_mut() {
                            prev.ends_with_space = true;
                        }
                        if !full_text.ends_with(' ') {
                            full_text.push(' ');
                        }
                    }
                }
            }
            Ok(Event::End(_)) => {
                if let Some((td, lang, was_bg)) = in_trans.as_ref()
                    && depth == *td
                {
                    let t = normalize_text(&trans_buf);
                    if !t.is_empty() {
                        let target = if *was_bg {
                            &mut bg_inline_trans
                        } else {
                            &mut inline_trans
                        };
                        target
                            .entry(lang.clone())
                            .and_modify(|prev| {
                                prev.push(' ');
                                prev.push_str(&t);
                            })
                            .or_insert(t);
                    }
                    in_trans = None;
                } else if let Some((rd, lang, was_bg)) = in_roma.as_ref()
                    && depth == *rd
                {
                    let r = normalize_text(&roma_buf);
                    if !r.is_empty() {
                        let target = if *was_bg {
                            &mut bg_inline_roma
                        } else {
                            &mut inline_roma
                        };
                        target
                            .entry(lang.clone())
                            .and_modify(|prev| {
                                prev.push(' ');
                                prev.push_str(&r);
                            })
                            .or_insert(r);
                    }
                    in_roma = None;
                } else if in_ruby_base == Some(depth) {
                    in_ruby_base = None;
                } else if in_ruby_text == Some(depth) {
                    in_ruby_text = None;
                } else if let Some((cd, rb, re)) = ruby_container_depth
                    && depth == cd
                {
                    let base = normalize_text(&ruby_base_buf);
                    let rt = normalize_text(&ruby_text_buf);
                    if !base.is_empty() {
                        if in_word {
                            word_buf.push_str(&base);
                            if !rt.is_empty() {
                                word_ruby = Some(rt);
                            }
                        } else if !in_bg {
                            push_word(
                                &mut words,
                                &mut full_text,
                                &base,
                                rb,
                                re,
                                (!rt.is_empty()).then_some(rt),
                            );
                        }
                    }
                    ruby_container_depth = None;
                } else if in_bg_word && depth == bg_word_depth {
                    in_bg_word = false;
                    push_word(
                        &mut bg_words,
                        &mut bg_full_text,
                        &bg_word_buf,
                        bg_word_begin,
                        bg_word_end,
                        None,
                    );
                } else if in_word && depth == word_depth {
                    in_word = false;
                    push_word(
                        &mut words,
                        &mut full_text,
                        &word_buf,
                        word_begin,
                        word_end,
                        word_ruby.take(),
                    );
                } else if in_bg && depth == bg_depth {
                    in_bg = false;
                    let stripped_text = strip_bg_parens(&normalize_text(&bg_full_text));
                    if !stripped_text.is_empty() {
                        if let Some(first) = bg_words.first_mut() {
                            let ft = first
                                .text
                                .trim_start_matches(['(', '（'])
                                .trim()
                                .to_string();
                            if ft.is_empty() {
                                bg_words.remove(0);
                            } else {
                                let mut nw = create_word_timing(first.start, first.end, &ft);
                                nw.ends_with_space = first.ends_with_space;
                                *first = nw;
                            }
                        }
                        if let Some(last) = bg_words.last_mut() {
                            let lt = last.text.trim_end_matches([')', '）']).trim().to_string();
                            if lt.is_empty() {
                                bg_words.pop();
                            } else {
                                let mut nw = create_word_timing(last.start, last.end, &lt);
                                nw.ends_with_space = false;
                                *last = nw;
                            }
                        }
                        let start = bg_words.first().map(|w| w.start).unwrap_or(bg_begin);
                        let end = bg_words.last().map(|w| w.end).unwrap_or(bg_end);
                        bg_lines.push(
                            LyricLine::new(start, stripped_text)
                                .with_end(Some(end))
                                .with_words((!bg_words.is_empty()).then(|| bg_words.clone()))
                                .with_bg(true),
                        );
                    }
                }

                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(raw) = t.unescape() {
                    if in_trans.is_some() {
                        trans_buf.push_str(&raw);
                    } else if in_roma.is_some() {
                        roma_buf.push_str(&raw);
                    } else if in_ruby_text.is_some() {
                        ruby_text_buf.push_str(&raw);
                    } else if in_ruby_base.is_some() {
                        ruby_base_buf.push_str(&raw);
                    } else if in_bg_word {
                        bg_word_buf.push_str(&raw);
                    } else if in_word {
                        word_buf.push_str(&raw);
                    } else if in_bg {
                        bg_full_text.push_str(&raw);
                        if !raw.contains('\n')
                            && raw.trim().is_empty()
                            && let Some(prev) = bg_words.last_mut()
                        {
                            prev.ends_with_space = true;
                        }
                    } else {
                        full_text.push_str(&raw);
                        if !raw.contains('\n')
                            && raw.trim().is_empty()
                            && let Some(prev) = words.last_mut()
                        {
                            prev.ends_with_space = true;
                        }
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if let Some(last) = words.last_mut() {
        last.ends_with_space = false;
    }

    let text = normalize_text(&full_text);
    let mut begin = p_begin;
    let mut end = p_end;
    if let (Some(fw), Some(lw)) = (words.first(), words.last()) {
        if begin == 0.0 || fw.start < begin {
            begin = fw.start;
        }
        if lw.end > end {
            end = lw.end;
        }
    }

    ParsedPElement {
        begin,
        end,
        text,
        words,
        bg_lines,
        bg_agent,
        inline_trans,
        inline_roma,
        bg_inline_trans,
        bg_inline_roma,
    }
}

fn extract_agent_number(agent_id: &str) -> u32 {
    let digits: String = agent_id
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return 0;
    }
    let rev: String = digits.chars().rev().collect();
    rev.parse().unwrap_or(0)
}

fn resolve_duets(lines: &mut [LyricLine], agent_types: &HashMap<String, AgentInfo>) {
    let mut distinct_agents = HashSet::new();
    for l in lines.iter() {
        if let Some(ref a) = l.agent_id
            && !a.is_empty()
            && !LyricLine::is_centered_agent_id(Some(a))
        {
            distinct_agents.insert(a.clone());
        }
    }
    if distinct_agents.len() < 2 {
        return;
    }

    let mut last_person: Option<String> = None;
    let mut last_duet = false;
    for l in lines.iter_mut() {
        let Some(ref aid) = l.agent_id else {
            continue;
        };
        if LyricLine::is_centered_agent_id(Some(aid)) {
            l.is_duet = false;
            continue;
        }
        let num = extract_agent_number(aid);
        if num > 0 {
            l.is_duet = num.is_multiple_of(2);
            continue;
        }
        match agent_types.get(aid).map(|i| i.agent_type.as_str()) {
            Some("group") => l.is_duet = false,
            Some("other") => l.is_duet = last_person.is_none(),
            _ => {
                if let Some(ref prev) = last_person {
                    if prev != aid {
                        last_duet = !last_duet;
                        last_person = Some(aid.clone());
                    }
                    l.is_duet = last_duet;
                } else {
                    last_person = Some(aid.clone());
                    last_duet = false;
                    l.is_duet = false;
                }
            }
        }
    }
}

/// Parses Apple Music / AMLL / lrc.red TTML2 into a full `Lyrics` structure.
pub fn parse_ttml(ttml: &str, provider_name: &str) -> Option<Lyrics> {
    if ttml.trim().is_empty() {
        return None;
    }

    let hm = parse_head_metadata(ttml);
    let mut reader = Reader::from_str(ttml);
    reader.config_mut().trim_text(false);

    let mut lines: Vec<LyricLine> = Vec::new();
    let mut translations: HashMap<String, Vec<LyricLine>> = HashMap::new();
    let mut romanizations: HashMap<String, Vec<LyricLine>> = HashMap::new();

    let mut no_timing = false;
    let mut frame_rate = DEFAULT_FRAME_RATE;
    let mut in_head = false;
    let mut in_body = false;
    let mut div_song_part: Option<String> = None;
    let mut div_stack: Vec<Option<String>> = Vec::new();
    let mut buf = Vec::new();

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if local == "tt" {
                    no_timing = attr_prefixed(&e, &["itunes:timing"], "timing")
                        .is_some_and(|v| v.eq_ignore_ascii_case("none"));
                    if let Some(fr) = attr_value(&e, &["frameRate"])
                        && let Ok(parsed_fr) = fr.trim().parse::<f64>()
                        && parsed_fr > 0.0
                    {
                        frame_rate = parsed_fr;
                    }
                } else if local == "head" {
                    in_head = true;
                } else if local == "body" {
                    in_body = true;
                } else if local == "div" && in_body && !in_head {
                    div_stack.push(div_song_part.clone());
                    if let Some(sp) =
                        attr_prefixed(&e, &["itunes:songPart", "itunes:song-part"], "songPart")
                    {
                        let t = sp.trim();
                        if !t.is_empty() {
                            div_song_part = Some(t.to_string());
                        }
                    }
                } else if local == "p" && in_body && !in_head {
                    let line_id = attr_prefixed(&e, &["itunes:key"], "key");
                    let agent_id = attr_prefixed(&e, &["ttm:agent"], "agent");
                    let own_part =
                        attr_prefixed(&e, &["itunes:songPart", "itunes:song-part"], "songPart")
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty());
                    let song_part = own_part.or_else(|| div_song_part.clone());

                    let p_begin = if no_timing {
                        0.0
                    } else {
                        parse_ttml_time(attr_value(&e, &["begin"]).as_deref(), frame_rate)
                    };
                    let p_end = if no_timing {
                        0.0
                    } else {
                        resolve_end_time(
                            p_begin,
                            attr_value(&e, &["end"]).as_deref(),
                            attr_value(&e, &["dur"]).as_deref(),
                            frame_rate,
                        )
                    };

                    let mut pl = parse_p_element(&mut reader, p_begin, p_end, frame_rate);
                    if pl.text.is_empty() && pl.bg_lines.is_empty() {
                        continue;
                    }

                    if let Some(sidecar) = find_sidecar_roman(line_id.as_deref(), &hm.sidecar_roman)
                    {
                        align_romaji_to_words(&mut pl.words, sidecar);
                    }

                    let main_idx = lines.len();
                    if !pl.text.is_empty() {
                        lines.push(
                            LyricLine::new(pl.begin, pl.text.clone())
                                .with_end((pl.end > pl.begin).then_some(pl.end))
                                .with_words((!pl.words.is_empty()).then_some(pl.words.clone()))
                                .with_agent(agent_id.clone(), false)
                                .with_song_part(song_part.clone()),
                        );
                    }

                    let bg_agent = pl.bg_agent.or(agent_id);
                    for mut bgl in pl.bg_lines {
                        bgl.agent_id = bg_agent.clone();
                        bgl.song_part = song_part.clone();
                        bgl.is_bg = true;
                        lines.push(bgl);
                    }

                    // Record inline & sidecar translations/romanizations
                    for (lang, text) in pl.inline_trans {
                        let list = translations.entry(lang).or_default();
                        while list.len() < main_idx {
                            list.push(LyricLine::new(-1.0, ""));
                        }
                        list.push(LyricLine::new(pl.begin, text));
                    }
                    for (lang, text) in pl.bg_inline_trans {
                        let list = translations.entry(format!("bg:{lang}")).or_default();
                        while list.len() < main_idx {
                            list.push(LyricLine::new(-1.0, ""));
                        }
                        list.push(LyricLine::new(pl.begin, text));
                    }
                    for (lang, text) in pl.inline_roma {
                        let list = romanizations.entry(lang).or_default();
                        while list.len() < main_idx {
                            list.push(LyricLine::new(-1.0, ""));
                        }
                        list.push(LyricLine::new(pl.begin, text));
                    }
                    for (lang, text) in pl.bg_inline_roma {
                        let list = romanizations.entry(format!("bg:{lang}")).or_default();
                        while list.len() < main_idx {
                            list.push(LyricLine::new(-1.0, ""));
                        }
                        list.push(LyricLine::new(pl.begin, text));
                    }

                    if let Some(ref lid) = line_id {
                        let suffix = format!(":{lid}");
                        for (k, st) in &hm.sidecar_trans {
                            if let Some(lang) = k.strip_suffix(&suffix) {
                                if !st.main_text.is_empty() {
                                    let list = translations.entry(lang.to_string()).or_default();
                                    while list.len() < main_idx {
                                        list.push(LyricLine::new(-1.0, ""));
                                    }
                                    list.push(LyricLine::new(pl.begin, st.main_text.clone()));
                                }
                                if let Some(ref bgt) = st.bg_text {
                                    let list =
                                        translations.entry(format!("bg:{lang}")).or_default();
                                    while list.len() < main_idx {
                                        list.push(LyricLine::new(-1.0, ""));
                                    }
                                    list.push(LyricLine::new(pl.begin, bgt.clone()));
                                }
                            }
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let qname = e.name();
                let local = local_name(qname.as_ref());
                if local == "head" {
                    in_head = false;
                } else if local == "body" {
                    in_body = false;
                } else if local == "div" && in_body && !in_head {
                    div_song_part = div_stack.pop().flatten();
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if lines.is_empty() {
        return None;
    }

    if hm.offset_secs.abs() > f64::EPSILON {
        for line in &mut lines {
            if line.time >= 0.0 {
                line.time = (line.time + hm.offset_secs).max(0.0);
            }
            if let Some(end) = line.end {
                line.end = Some((end + hm.offset_secs).max(0.0));
            }
            if let Some(words) = line.words.as_mut() {
                for w in words.iter_mut() {
                    let mut nw = create_word_timing(
                        (w.start + hm.offset_secs).max(0.0),
                        (w.end + hm.offset_secs).max(0.0),
                        &w.text,
                    );
                    nw.ends_with_space = w.ends_with_space;
                    nw.romaji = w.romaji.clone();
                    nw.ruby = w.ruby.clone();
                    *w = nw;
                }
            }
        }
    }

    resolve_duets(&mut lines, &hm.agent_types);

    let target_len = lines.len();
    for list in translations.values_mut() {
        while list.len() < target_len {
            list.push(LyricLine::new(-1.0, ""));
        }
    }
    for list in romanizations.values_mut() {
        while list.len() < target_len {
            list.push(LyricLine::new(-1.0, ""));
        }
    }

    let synced = !no_timing && lines.iter().any(|l| l.time > 0.0 || l.end.is_some());
    let mut lyrics = Lyrics::new(lines, provider_name, synced);
    lyrics.translations = translations;
    lyrics.romanizations = romanizations;
    lyrics.songwriters = hm.songwriters;
    lyrics.credits = hm.credits;
    lyrics.raw_format = Some(ttml.to_string());
    lyrics.format_type = Some("ttml".to_string());
    lyrics.about = hm.about;
    for (id, info) in hm.agent_types {
        if let Some(name) = info.name {
            lyrics.agent_names.insert(id, name);
        }
    }
    Some(lyrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ttml_word_sync_bg_and_song_part() {
        let ttml = r#"<?xml version="1.0" encoding="UTF-8"?>
<tt xmlns="http://www.w3.org/ns/ttml"
    xmlns:itunes="http://music.apple.com/lyric-ttml-internal"
    xmlns:ttm="http://www.w3.org/ns/ttml#metadata">
  <head>
    <metadata>
      <ttm:agent xml:id="v1" type="person"><ttm:name>Lead</ttm:name></ttm:agent>
      <ttm:agent xml:id="v2" type="person"><ttm:name>Duet</ttm:name></ttm:agent>
      <itunes:iTunesMetadata>
        <songwriters><songwriter>Alice</songwriter></songwriters>
      </itunes:iTunesMetadata>
    </metadata>
  </head>
  <body>
    <div itunes:songPart="Chorus">
      <p begin="00:10.000" end="00:14.000" ttm:agent="v1" itunes:key="L1">
        <span begin="00:10.000" end="00:11.000">Hello</span> <span begin="00:11.000" end="00:12.500">world</span>
        <span ttm:role="x-bg" begin="00:12.500" end="00:14.000">(<span begin="00:12.500" end="00:14.000">oh</span>)</span>
      </p>
      <p begin="00:15.000" end="00:17.000" ttm:agent="v2" itunes:key="L2">Duet line</p>
    </div>
  </body>
</tt>"#;

        let lyrics = parse_ttml(ttml, "Apple").unwrap();
        assert!(lyrics.synced);
        assert_eq!(lyrics.songwriters, vec!["Alice"]);
        assert_eq!(lyrics.lines.len(), 3);
        assert_eq!(lyrics.lines[0].text, "Hello world");
        assert_eq!(lyrics.lines[0].song_part.as_deref(), Some("Chorus"));
        assert!(!lyrics.lines[0].is_duet);
        assert!(lyrics.lines[0].has_word_timings());
        assert_eq!(lyrics.lines[1].text, "oh");
        assert!(lyrics.lines[1].is_bg);
        assert_eq!(lyrics.lines[2].text, "Duet line");
        assert!(lyrics.lines[2].is_duet);
    }

    #[test]
    fn test_parse_ttml_time_formats() {
        assert!((parse_ttml_time(Some("1250ms"), 30.0) - 1.25).abs() < 1e-6);
        assert!((parse_ttml_time(Some("1:05.500"), 30.0) - 65.5).abs() < 1e-6);
        assert!((parse_ttml_time(Some("00:01:02:15"), 30.0) - 62.5).abs() < 1e-6);
    }
}
