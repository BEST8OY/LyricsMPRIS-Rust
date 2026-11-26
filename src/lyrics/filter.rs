//! Lyrics text filtering, credit extraction, bracket splitting, overlap retiming, and formatting.
//!
//! Direct Rust port of Morphe's `LyricsFilter`, `LyricsFilterDefaults`, `LyricsBracketSplitter`,
//! `LyricsRetiming`, and `LyricsTextFormatter`.

use crate::config::{FilterConfig, HanMode};
use crate::lyrics::types::{LyricLine, Lyrics, TrackInfo, WordTiming, create_word_timing};
use crate::text_utils::{
    apply_decoration_regex_preserve_original, normalize_search_text, script_variants,
    to_simplified_chinese, to_traditional_chinese,
};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Morphe's `DEFAULT_LYRICS_TEXT_FILTER`: removes Chinese music-app promotional boilerplate.
pub const DEFAULT_LYRICS_TEXT_FILTER: &str = concat!(
    r#".*?(?:"#,
    r#"未经.*?(?:不得|禁止)"#,
    r#"|本作品声明.*?著作权权利保留.*?不得"#,
    r#"|本字幕由TME AI技术生成"#,
    r#"|部分素材源自网络"#,
    r#"|酷我音乐.*?特别出品"#,
    r#"|酷狗.*?星曜计划"#,
    r#"|酷狗.*?国潮"#,
    r#"|酷狗音乐.*?就是歌多"#,
    r#"|听国潮.*?酷狗"#,
    r#"|未经许可.*?(?:翻唱|盗版)"#,
    r#"|本作品.*?授权"#,
    r#"|已获得.*?授权"#,
    r#"|星曜计划.*?企划|黑胶复刻"#,
    r#"|此歌曲为没有填词的纯音乐"#,
    r#"|纯音乐，请欣赏"#,
    r#"|此歌曲由Vemus未音APP\.制作 音乐创作如此简单！"#,
    r#"|酷狗音乐『万物皆可dj』企划"#,
    r#"|『听dj, 到中国酷狗』"#,
    r#"|本歌曲来自〖飓风计划〗"#,
    r#"|10亿现金激励，千亿流量扶持！"#,
    r#"|版权所有\s*未经\s*许可\s*请勿\s*使用"#,
    r#"|想听的歌在评\s*论区"#,
    r#").*"#
);

/// Morphe's `DEFAULT_LYRICS_CREDIT_LINE_REGEX`: ~300 English & Chinese credit role markers.
pub const DEFAULT_CREDIT_LINE_MARKERS: &str = concat!(
    "1st Violin,2nd Violin,A&R,A.Guita,AU,Additional Drums Engineering,Administer,Administered,Administered By,",
    "Administering,Administers,Agency,Album,All Instruments,Arranged,Arranged By,Arranger,Arrangers,",
    "Arranging,Artist,Artists,Atmos混音,Assistant Engineer,Assistant Engineers,",
    "Assistant Mix Engineer,Assistant Mix Engineers,Author,Authoring,Authors,Autotune,Backed,",
    "Background,Background Vocal,Background Vocals,Backing,Backing Vocals 和音,Bass,Bass 贝斯,",
    "Bass Guitar,Bass by,Brass Arrange,Brass Band,COS,CV,Cello,Cello 大提琴,Child,Child Choir,",
    "Child Choir Instruction,Child Lead,Children,Chorus,Composed By,Composer,Composers,Composing,",
    "Computer Programming by,Conductor,Copyright,Cover,Credit,DJ,Digital Edited,Digital Edited By,",
    "Digital Editing,Directed,Directed By,Directing,Director,Directors,Drum,Drum programming,Drums,",
    "Duration,E.Guitar,Edited,Edited By,Editing Engineer,Editing Engineers,Editor,Editors,",
    "Electric Guitar,Engineer,Engineered,Engineered By,Engineering,Engineers,Executed,Executing,",
    "Executive,Executive Producer,First Violin 第一小提琴,Guitar,Guitar by,Guitars,Guitars 吉他,",
    "Group,Harmony,Illustration,ISRC,Instrumental Technician,Keyboard,Keyboard by,Keyboard 键盘,",
    "Keyboard/Music Arrangement 键盘与编曲,LA,Label A&R,Lang,Language,Lead,Leader,Leaders,Length,",
    "Lyric,Lyricist,Lyricist 词,Lyricist 词作,Lyricists,Lyrics,Lyrics By,MV,Main Sample,Manufactory,",
    "Manufactured,Manufactured By,Manufacturing,Management,Master,Mastered,Mastered By,Mastering,",
    "Mastering 母带后期处理,Mastering Engineer,Mastering Engineers,Mastering Studio,Masters,",
    "Mix Engineer,Mix Engineered by,Mixed,Mixed By,Mixed in Dolby Atmos by,Mixer,Mixers,Mixing,",
    "Mixing Engineer,Mixing Studio,Music,Musical Director & Mixing Engineer,Movie Effect,OA,OC,OP,OT,",
    "Original Lyrics by,Original Title,Original Publisher,Original Writer,PGM,PV,Percussion,",
    "Performed,Performed By,Performer,Performers,Performing,Piano,Pro-Tools Editing,Produced,",
    "Produced By,Producer,Producer 制作人,Producers,Producing,Program,Programming,Programming by,",
    "Published,Published By,Publisher,Publishers,Publishing,Publishing Group,",
    "Publishing Group Administered By,QQ,RE,Rap,Record,Recorded,Recorded At,Recorded By,Recorder,",
    "Recorders,Recording,Recording & Mixing Engineer,Recording Engineer,Recording Engineers,",
    "Recording Engineers by,Recordings,Records,SP,Sample,Sampled,Samples,Sampling,",
    "Second Violin 第二小提琴,Singer,Singers,Singing,Song,String Arrangement 弦乐编写,",
    "String Recording,String Recording Engineer 弦乐录音师,String Recording Studio 弦乐录音棚,",
    "Strings,Strings Arrangement,Strings Arrangement & Piano,Strings Direction,Studio,Sub,",
    "Sub Publisher,Subs,Subscribe,Subscribed,Subscriber,Subscribers,Surround,Synthesizer,",
    "Synthesizer by,Synthesizers,TA,Title,VE,Ver,Version,Viola,Viola 中提琴,Vocal,Vocal Arrangement,",
    "Vocal & Rap Arrangement,Vocal Directed,Vocal Directed By,Vocal Director,Vocal Engineer,",
    "Vocal Engineering,Vocal Produced,Vocal Produced By,Vocal Producer,Vocal Producers,Vocals,",
    "Vocals Arrangement,Voice,Written,Written By,Writter,专辑,业务联系,业务邮箱,中提,中提琴,中提琴手,",
    "中文,中文词SA,主催,主唱,乐器,乐团,乐队,书法,布祖基琴 Bousouki,竖琴吉他,二胡,人声,",
    "人声剪辑 Vocal Editing,企业宣传,企划,企宣,伴唱,伴奏,伴舞,低音提琴,低音吉他,作曲,作画,作者,作词,",
    "修音,修音师,公司,出品,出品人,创作,创作者,指挥,创意,制谱,制谱 Scoring,制作,制作人,前置混音,剧情,",
    "剪纸艺术家,助力推广,助理,协力,厂牌,原唱,原曲,原歌名,原版,原画,原编曲,原翻,原著,原词,原词曲,发型,",
    "发布,发布者,发行,口琴,口风琴,古筝,合作,合作伙伴,合作者,合声,合成,合成器,合音,合唱,吉他,后期,",
    "吟唱,和声,和音,风笛 Bagpipes,呼麦,唢呐,商务,哨笛 Whistle,团队,图,图片,图画,地址,场景,场景提供,",
    "填词,声乐,声音,处理,大提,大提琴,大提琴手,女声,妆造,官方,官方指定音乐合作伙伴,宣传,宣发,宣推,",
    "富鲁格,导演,封设,封面,小号,小提,小提琴,小提琴手,第一小提琴,第二小提琴,工作室,工程,工程师,",
    "平面设计,平台,弦乐,弦乐团,录音,录音室,录音师,录音棚,录制,微信,微博,快手,念白,总企划,总监,总监制,",
    "总策划,总顾问,总营销,手碟,手风琴,打击乐,执行,抖音,短视频平台总统筹,短视频平台宣推,短视频宣推,",
    "短视频统筹,拍摄,指定音乐合作伙伴,指导,指导老师,推广,摄影,改编,改编词,文案,时长,曲,曲 Composer,",
    "曲Composed by,曲 Music,曲Composer,曲Music,曲名,曲绘,曲编,曲协力,木吉他,木管,杜比全景声,板胡,",
    "校准,次中音萨克斯,歌名,歌声,歌手,歌曲,歌词改编,歌唱指导,母带,海外配唱执行,海报,混缩,混缩室,混音,",
    "混音室,混音师,混音棚,滤镜,演唱,演奏,漫画,灯光,版本,版权,版权归属,特别鸣谢,",
    "特別感謝 Special Thanks to,班卓琴,琵琶,电吉他,电贝斯Electric Bass,电脑工程,电钢琴,男声,画,画师,",
    "监制,监唱,私人,童声,笛,笛子,笛萧,策划,管乐,管弦,管弦乐,箫,粤语,经纪,统筹,编,编写,编剧,编唱,编导,",
    "编曲,编舞,编舞师,编著,编辑,缩混,网易音乐人商务合作,美工,美声演唱,美术,美术设计,翻唱,翻译,翻译者,",
    "联合,联系,联系方式,舞台,舞团,舞曲,舞蹈,艺人制作统筹,艺人经纪,艺人经纪公司,艺人统筹,艺术家,",
    "艺术指导,艺术指导老师,艺统,花脸,英文,营销,萧笛,萨克斯,视觉,记,设计,词,词 Lyricist,词 Lyrics,",
    "词Lyricist,词Lyrics,词曲作者,词曲提供,词协力,译者,语言,语言代码,说唱,说唱词,调校,调音,谱曲,贝斯,",
    "贴唱,造型,邮件,邮箱地址,配唱,采样,钢琴,铜管,键盘,键盘手,长号,长笛,队长,附加,音乐,音乐人,",
    "音准调校,音响,音效,音编,音频,项目企划,项目协力,项目总企划,项目总监,项目统筹,项目营销,顾问,领唱,",
    "领舞,题字,题记,飓风计划商务合作,马头琴,鸣谢,鼓,鼓手,鼓 Drums,鼓录音,鼓录音室,鼓录制,鼓机,",
    "鼓技师Drum Technician,爵士鼓Drum,鼓组编程Drums Arrangement,鼓组 Drums,",
    "鼓组录音师 Drum Recording Engineer,鼓组录音室 Drum Recording Studio,鼓组音频编辑,运营"
);

static DEFAULT_TEXT_FILTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(DEFAULT_LYRICS_TEXT_FILTER).unwrap());
static DEFAULT_CREDIT_VARIANTS: LazyLock<Vec<String>> =
    LazyLock::new(|| build_credit_variants(DEFAULT_CREDIT_LINE_MARKERS));
static ARTIST_SONG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[^\-]+\s*-\s*[^\-]+$").unwrap());
static NON_VOCAL_BRACKET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^\s*(?:x\s*\d+|\d+\s*x|repeat|chorus|verse|pre\s*chorus|post\s*chorus)\s*$")
        .unwrap()
});
static MULTI_SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

const CREDIT_LINE_GAP_TOLERANCE: usize = 3;
const MAX_ARTIST_SONG_LINE_LEN: usize = 80;

#[inline]
pub fn is_cjk_ideograph(c: char) -> bool {
    matches!(
        c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x20000..=0x2A6DF | 0xF900..=0xFAFF
    )
}

#[inline]
fn is_unspaced_codepoint(c: char) -> bool {
    is_cjk_ideograph(c)
        || matches!(
            c as u32,
            0x3040..=0x30FF | 0x31F0..=0x31FF | 0xAC00..=0xD7AF | 0x3000..=0x303F | 0xFF00..=0xFFEF
        )
}

fn build_credit_variants(markers_csv: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in markers_csv.split(',') {
        let m = raw.trim();
        if m.is_empty() {
            continue;
        }
        let norm = normalize_search_text(m);
        if !norm.is_empty() && seen.insert(norm.clone()) {
            out.push(norm);
        }
        for v in script_variants(m) {
            let nv = normalize_search_text(&v);
            if !nv.is_empty() && seen.insert(nv.clone()) {
                out.push(nv);
            }
        }
    }
    out
}

fn is_credit_label_boundary(
    text: &str,
    pos: usize,
    all_variants: &[String],
    current_variant: &str,
) -> bool {
    if pos >= text.len() {
        return true;
    }
    let Some(c) = text[pos..].chars().next() else {
        return true;
    };
    if matches!(
        c,
        '/' | '&'
            | '|'
            | '·'
            | '~'
            | '@'
            | '\\'
            | '－'
            | '—'
            | '-'
            | ':'
            | '、'
            | '；'
            | '，'
            | ','
            | ';'
            | '＋'
            | '+'
    ) {
        if matches!(c, '-' | '－' | '—') {
            let before = text[..pos].chars().next_back();
            let after = text[pos + c.len_utf8()..].chars().next();
            if before.is_some_and(|b| b.is_alphanumeric() && !is_cjk_ideograph(b))
                && after.is_some_and(|a| a.is_alphanumeric() && !is_cjk_ideograph(a))
            {
                return false;
            }
            let rest = &text[pos + c.len_utf8()..];
            return !rest
                .to_lowercase()
                .starts_with(&current_variant.to_lowercase());
        }
        return true;
    }
    if c.is_whitespace()
        && let Some(prev) = text[..pos].chars().next_back()
        && (is_cjk_ideograph(prev) || current_variant.contains(' '))
    {
        return true;
    }
    if matches!(c, '和' | '与' | '及') {
        return true;
    }
    if let Some(prev) = text[..pos].chars().next_back()
        && is_cjk_ideograph(prev) != is_cjk_ideograph(c)
        && c.is_alphanumeric()
    {
        return true;
    }
    let remainder = &text[pos..];
    for v in all_variants {
        if v != current_variant && v.chars().count() >= 2 && remainder.starts_with(v) {
            return true;
        }
    }
    false
}

fn has_real_separator(has_sep: bool, normalized_with_spaces: &str) -> bool {
    if !has_sep {
        return false;
    }
    let seps = [
        ':', '|', '｜', '—', '－', '-', ';', '；', ',', '，', '~', '～', '：', '·', '@', '/', '\\',
        '&',
    ];
    let real_sep_idx = seps
        .iter()
        .filter_map(|&ch| normalized_with_spaces.find(ch))
        .min();
    let space_idx = normalized_with_spaces.find(' ');
    match (real_sep_idx, space_idx) {
        (Some(r), Some(s)) => r < s,
        (Some(_), None) => true,
        _ => false,
    }
}

fn is_artist_song_line(text: &str, track: &TrackInfo) -> bool {
    if track.artist.trim().is_empty() || track.title.trim().is_empty() {
        return false;
    }
    let Some((left, right)) = text.split_once('-') else {
        return false;
    };
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let a_vars = script_variants(track.artist.trim());
    let t_vars = script_variants(track.title.trim());
    let l_vars = script_variants(left);
    let r_vars = script_variants(right);

    let contains_any = |hay: &[String], need: &[String]| -> bool {
        hay.iter().any(|h| {
            let hl = h.to_lowercase();
            need.iter().any(|n| {
                let nl = n.to_lowercase();
                hl.contains(&nl) || nl.contains(&hl)
            })
        })
    };

    let mut a_all = vec![track.artist.trim().to_string()];
    a_all.extend(a_vars);
    let mut t_all = vec![track.title.trim().to_string()];
    t_all.extend(t_vars);
    let mut l_all = vec![left.to_string()];
    l_all.extend(l_vars);
    let mut r_all = vec![right.to_string()];
    r_all.extend(r_vars);

    (contains_any(&l_all, &a_all) && contains_any(&r_all, &t_all))
        || (contains_any(&l_all, &t_all) && contains_any(&r_all, &a_all))
}

fn is_credit_line(raw: &str, track: &TrackInfo, variants: &[String]) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return true;
    }
    let collapsed = MULTI_SPACE_RE.replace_all(trimmed, " ");
    if collapsed.chars().count() <= MAX_ARTIST_SONG_LINE_LEN
        && ARTIST_SONG_RE.is_match(&collapsed)
        && is_artist_song_line(&collapsed, track)
    {
        return true;
    }
    if collapsed.chars().filter(|&c| c == '/').count() > 5 {
        return true;
    }
    if variants.is_empty() {
        return false;
    }

    let normalized_with_spaces = normalize_search_text(&collapsed);
    for v in variants {
        if v.chars().count() >= 2 && normalized_with_spaces.starts_with(v) {
            let end = v.len();
            if end >= normalized_with_spaces.len()
                || is_credit_label_boundary(&normalized_with_spaces, end, variants, v)
            {
                return true;
            }
        }
    }

    let mut sep_norm = String::with_capacity(normalized_with_spaces.len());
    for c in normalized_with_spaces.chars() {
        match c {
            '|' | '｜' | '—' | '－' | ';' | '；' | ',' | '，' | '~' | '～' | ' ' | '：' | '·'
            | '@' | '/' | '\\' | '&' => sep_norm.push(':'),
            other => sep_norm.push(other),
        }
    }
    let sep_idx = sep_norm.find(':');
    if let Some(idx) = sep_idx
        && sep_norm[idx + 1..].trim().is_empty()
    {
        return false;
    }
    let before_sep = sep_idx.map(|i| &sep_norm[..i]).unwrap_or(&sep_norm);
    let real_sep = has_real_separator(sep_idx.is_some(), &normalized_with_spaces);

    for v in variants {
        if v.is_empty() {
            continue;
        }
        if real_sep && before_sep == v {
            return true;
        }
        if sep_idx.is_some()
            && before_sep.ends_with(v)
            && before_sep.len() > v.len()
            && let Some(prev) = before_sep[..before_sep.len() - v.len()].chars().next_back()
            && !prev.is_alphanumeric()
            && prev != '\''
        {
            return true;
        }
        if v.chars().count() >= 2
            && v.chars().next().is_some_and(is_cjk_ideograph)
            && before_sep.starts_with(v)
        {
            let end = v.len();
            if end >= before_sep.len() || is_credit_label_boundary(before_sep, end, variants, v) {
                return true;
            }
        }
    }
    false
}

fn filter_aligned_map(map: &mut HashMap<String, Vec<LyricLine>>, kept: &[bool]) {
    for list in map.values_mut() {
        let mut out = Vec::with_capacity(list.len());
        for (i, line) in list.drain(..).enumerate() {
            if kept.get(i).copied().unwrap_or(false) {
                out.push(line);
            }
        }
        *list = out;
    }
}

/// Removes contiguous credit blocks at the start and end of lyrics (`LyricsFilter.filterCreditLines`).
pub fn filter_credit_lines(
    mut lyrics: Lyrics,
    track: &TrackInfo,
    custom_markers: Option<&str>,
) -> Lyrics {
    if lyrics.lines.is_empty() {
        return lyrics;
    }
    let owned_variants;
    let variants = match custom_markers {
        Some(m) if m.trim().is_empty() => return lyrics,
        Some(m) => {
            owned_variants = build_credit_variants(m);
            &owned_variants[..]
        }
        None => &DEFAULT_CREDIT_VARIANTS[..],
    };

    let size = lyrics.lines.len();
    let mut is_credit = vec![false; size];

    let mut start_block_end: Option<usize> = None;
    let mut consecutive_non = 0usize;
    let mut front_checked_end = 0usize;
    for (i, line) in lyrics.lines.iter().enumerate() {
        front_checked_end = i + 1;
        let c = is_credit_line(line.text.trim(), track, variants);
        is_credit[i] = c;
        if c {
            start_block_end = Some(i);
            consecutive_non = 0;
        } else {
            consecutive_non += 1;
            if consecutive_non > CREDIT_LINE_GAP_TOLERANCE {
                break;
            }
        }
    }

    let mut end_block_start = size;
    consecutive_non = 0;
    for i in (0..size).rev() {
        let c = if i < front_checked_end {
            is_credit[i]
        } else {
            let v = is_credit_line(lyrics.lines[i].text.trim(), track, variants);
            is_credit[i] = v;
            v
        };
        if c {
            end_block_start = i;
            consecutive_non = 0;
        } else {
            consecutive_non += 1;
            if consecutive_non > CREDIT_LINE_GAP_TOLERANCE {
                break;
            }
        }
    }

    let filter_first_line = start_block_end.is_some_and(|end_idx| {
        end_idx >= 1 && !is_credit[0] && (1..=end_idx).all(|i| is_credit[i])
    });

    let mut kept = vec![false; size];
    let mut credit_lines = Vec::new();
    let mut filtered = Vec::with_capacity(size);

    for (i, line) in lyrics.lines.into_iter().enumerate() {
        let in_start = start_block_end.is_some_and(|e| i <= e);
        let in_end = i >= end_block_start;
        let should_filter =
            ((in_start || in_end) && is_credit[i]) || (in_start && i == 0 && filter_first_line);

        if should_filter {
            let t = line.text.trim();
            if !t.is_empty() {
                credit_lines.push(t.to_string());
            }
        } else {
            kept[i] = true;
            filtered.push(line);
        }
    }

    if filtered.len() == size {
        lyrics.lines = filtered;
        return lyrics;
    }

    lyrics.lines = filtered;
    filter_aligned_map(&mut lyrics.translations, &kept);
    filter_aligned_map(&mut lyrics.romanizations, &kept);
    for cl in credit_lines {
        if !lyrics.credits.contains(&cl) {
            lyrics.credits.push(cl);
        }
    }
    lyrics
}

/// Applies boilerplate lyrics text filter (`LyricsFilter.filterLyricsText`).
pub fn filter_lyrics_text(mut lyrics: Lyrics, custom_filter: Option<&str>) -> Lyrics {
    if lyrics.lines.is_empty() {
        return lyrics;
    }
    let custom_re = match custom_filter {
        Some(s) if s.trim().is_empty() => return lyrics,
        Some(s) => Regex::new(s).ok(),
        None => None,
    };
    let re = custom_re.as_ref().unwrap_or(&DEFAULT_TEXT_FILTER_RE);

    let size = lyrics.lines.len();
    let mut kept = vec![false; size];
    let mut out = Vec::with_capacity(size);
    let mut any_dropped = false;

    for (i, mut line) in lyrics.lines.into_iter().enumerate() {
        let orig_collapsed = MULTI_SPACE_RE
            .replace_all(line.text.trim(), " ")
            .into_owned();
        let cleaned = apply_decoration_regex_preserve_original(&line.text, Some(re));
        let cleaned_collapsed = MULTI_SPACE_RE.replace_all(cleaned.trim(), " ").into_owned();
        if cleaned_collapsed.is_empty() {
            any_dropped = true;
            continue;
        }
        kept[i] = true;
        if cleaned_collapsed == orig_collapsed {
            out.push(line);
            continue;
        }

        if let Some(words) = line.words.take() {
            let new_words: Vec<WordTiming> = words
                .into_iter()
                .filter_map(|w| {
                    let wc = apply_decoration_regex_preserve_original(&w.text, Some(re));
                    let wt = MULTI_SPACE_RE.replace_all(wc.trim(), " ").into_owned();
                    if wt.is_empty() {
                        None
                    } else {
                        let mut nw = create_word_timing(w.start, w.end, &wt);
                        nw.ends_with_space = w.ends_with_space;
                        nw.romaji = w.romaji;
                        nw.ruby = w.ruby;
                        Some(nw)
                    }
                })
                .collect();
            line.words = (!new_words.is_empty()).then_some(new_words);
        }
        line.text = cleaned_collapsed;
        out.push(line);
    }

    lyrics.lines = out;
    if any_dropped {
        filter_aligned_map(&mut lyrics.translations, &kept);
        filter_aligned_map(&mut lyrics.romanizations, &kept);
    }
    lyrics
}

fn find_matching_paren(s: &str, open_byte: usize, open_ch: char, close_ch: char) -> Option<usize> {
    let mut depth = 1usize;
    for (idx, ch) in s[open_byte + open_ch.len_utf8()..].char_indices() {
        if ch == open_ch {
            depth += 1;
        } else if ch == close_ch {
            depth -= 1;
            if depth == 0 {
                return Some(open_byte + open_ch.len_utf8() + idx);
            }
        }
    }
    None
}

fn is_sung_bracket_content(inner: &str) -> bool {
    let t = inner.trim();
    if t.is_empty() || NON_VOCAL_BRACKET_RE.is_match(t) {
        return false;
    }
    t.chars().any(char::is_alphabetic)
}

fn trim_word_brackets(mut w: WordTiming) -> Option<WordTiming> {
    let mut t = w.text.trim().to_string();
    while t.starts_with('(') || t.starts_with('（') {
        let ch = t.chars().next().unwrap();
        t = t[ch.len_utf8()..].trim_start().to_string();
    }
    while t.ends_with(')') || t.ends_with('）') {
        let ch = t.chars().next_back().unwrap();
        t.truncate(t.len() - ch.len_utf8());
        t = t.trim_end().to_string();
    }
    if t.is_empty() {
        return None;
    }
    let mut rebuilt = create_word_timing(w.start, w.end, &t);
    rebuilt.ends_with_space = w.ends_with_space;
    rebuilt.romaji = w.romaji.take();
    rebuilt.ruby = w.ruby.take();
    Some(rebuilt)
}

fn split_straddling_word(
    w: &WordTiming,
    split_byte: usize,
    left_is_bg: bool,
) -> (Option<WordTiming>, Option<WordTiming>) {
    let left_raw = w.text[..split_byte].trim();
    let right_raw = w.text[split_byte..].trim();
    let trim_bg = |raw: &str, ends_space: bool| {
        (!raw.is_empty())
            .then(|| {
                trim_word_brackets(WordTiming {
                    start: w.start,
                    end: w.end,
                    text: raw.to_string(),
                    ends_with_space: ends_space,
                    ..Default::default()
                })
            })
            .flatten()
    };
    let build_main = |raw: &str, ends_space: bool| {
        (!raw.is_empty()).then(|| {
            let mut mw = create_word_timing(w.start, w.end, raw);
            mw.ends_with_space = ends_space;
            mw
        })
    };
    if left_is_bg {
        (
            trim_bg(left_raw, false),
            build_main(right_raw, w.ends_with_space),
        )
    } else {
        (
            build_main(left_raw, false),
            trim_bg(right_raw, w.ends_with_space),
        )
    }
}

fn split_outer_bracket_words(
    words: &[WordTiming],
    has_head: bool,
    has_tail: bool,
) -> (Option<Vec<WordTiming>>, Option<Vec<WordTiming>>) {
    let n = words.len();
    let mut head_end = 0usize;
    let mut head_straddle_main: Option<WordTiming> = None;
    let mut head_bg_words: Vec<WordTiming> = Vec::new();

    if has_head {
        let mut depth = 0usize;
        let mut opened = false;
        'outer_head: for (i, w) in words.iter().enumerate() {
            for (b, ch) in w.text.char_indices() {
                if ch == '(' || ch == '（' {
                    depth += 1;
                    opened = true;
                } else if (ch == ')' || ch == '）') && depth > 0 {
                    depth -= 1;
                    if opened && depth == 0 {
                        let after_close = b + ch.len_utf8();
                        head_end = i + 1;
                        if !w.text[after_close..].trim().is_empty() {
                            let (bg_part, main_part) = split_straddling_word(w, after_close, true);
                            if let Some(bw) = bg_part {
                                head_bg_words.push(bw);
                            }
                            head_straddle_main = main_part;
                        } else if let Some(trimmed) = trim_word_brackets(w.clone()) {
                            head_bg_words.push(trimmed);
                        }
                        break 'outer_head;
                    }
                }
            }
            if let Some(trimmed) = trim_word_brackets(w.clone()) {
                head_bg_words.push(trimmed);
            }
        }
    }

    let mut tail_start = n;
    let mut tail_straddle_main: Option<WordTiming> = None;
    let mut tail_bg_words_rev: Vec<WordTiming> = Vec::new();

    if has_tail {
        let mut depth = 0usize;
        let mut closed = false;
        'outer_tail: for j in (head_end..n).rev() {
            let w = &words[j];
            for (b, ch) in w.text.char_indices().rev() {
                if ch == ')' || ch == '）' {
                    depth += 1;
                    closed = true;
                } else if (ch == '(' || ch == '（') && depth > 0 {
                    depth -= 1;
                    if closed && depth == 0 {
                        tail_start = j;
                        if !w.text[..b].trim().is_empty() {
                            let (main_part, bg_part) = split_straddling_word(w, b, false);
                            if let Some(bw) = bg_part {
                                tail_bg_words_rev.push(bw);
                            }
                            tail_straddle_main = main_part;
                        } else if let Some(trimmed) = trim_word_brackets(w.clone()) {
                            tail_bg_words_rev.push(trimmed);
                        }
                        break 'outer_tail;
                    }
                }
            }
            if let Some(trimmed) = trim_word_brackets(w.clone()) {
                tail_bg_words_rev.push(trimmed);
            }
        }
    }

    let mut bw = head_bg_words;
    bw.extend(tail_bg_words_rev.into_iter().rev());

    let mut mw = Vec::with_capacity(
        tail_start.saturating_sub(head_end)
            + usize::from(head_straddle_main.is_some())
            + usize::from(tail_straddle_main.is_some()),
    );
    if let Some(m) = head_straddle_main {
        mw.push(m);
    }
    mw.extend(words[head_end..tail_start].iter().cloned());
    if let Some(m) = tail_straddle_main {
        mw.push(m);
    }

    if let Some(last) = mw.last_mut() {
        last.ends_with_space = false;
    }
    if let Some(last) = bw.last_mut() {
        last.ends_with_space = false;
    }

    (
        (!mw.is_empty()).then_some(mw),
        (!bw.is_empty()).then_some(bw),
    )
}

/// Splits leading/trailing `(...)` or `（...）` background vocals onto dedicated `is_bg` lines
/// (`LyricsBracketSplitter.apply`).
pub fn split_bracket_background(mut lyrics: Lyrics) -> Lyrics {
    let size = lyrics.lines.len();
    if size == 0 {
        return lyrics;
    }

    let mut out = Vec::with_capacity(size + 4);
    let mut room = vec![1usize; size];

    for (idx, line) in lyrics.lines.iter().enumerate() {
        let next_is_bg = lyrics.lines.get(idx + 1).is_some_and(|n| n.is_bg);
        if line.is_bg || line.text.trim().is_empty() || next_is_bg {
            out.push(line.clone());
            continue;
        }

        let text = line.text.trim();
        let mut head_bg: Option<(usize, String)> = None;
        if let Some(first_ch) = text.chars().next()
            && (first_ch == '(' || first_ch == '（')
        {
            let close_ch = if first_ch == '(' { ')' } else { '）' };
            if let Some(close_byte) = find_matching_paren(text, 0, first_ch, close_ch) {
                let inner = text[first_ch.len_utf8()..close_byte].trim();
                let after = text[close_byte + close_ch.len_utf8()..].trim();
                let valid_after = first_ch == '（'
                    || after.is_empty()
                    || text[close_byte + close_ch.len_utf8()..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_whitespace() || is_unspaced_codepoint(c));
                if valid_after && !after.is_empty() && is_sung_bracket_content(inner) {
                    head_bg = Some((close_byte + close_ch.len_utf8(), inner.to_string()));
                }
            }
        }

        let body_start = head_bg.as_ref().map(|(e, _)| *e).unwrap_or(0);
        let rest = text[body_start..].trim();

        let mut tail_bg: Option<(usize, String)> = None;
        if let Some(last_ch) = rest.chars().next_back()
            && (last_ch == ')' || last_ch == '）')
        {
            let open_ch = if last_ch == ')' { '(' } else { '（' };
            // Find matching open paren scanning backward
            let mut depth = 0isize;
            let mut open_byte = None;
            for (b, ch) in rest.char_indices().rev() {
                if ch == last_ch {
                    depth += 1;
                } else if ch == open_ch {
                    depth -= 1;
                    if depth == 0 {
                        open_byte = Some(b);
                        break;
                    }
                }
            }
            if let Some(ob) = open_byte
                && ob > 0
            {
                let inner = rest[ob + open_ch.len_utf8()..rest.len() - last_ch.len_utf8()].trim();
                let valid_before = open_ch == '（'
                    || rest[..ob]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_whitespace() || is_unspaced_codepoint(c));
                if valid_before && !rest[..ob].trim().is_empty() && is_sung_bracket_content(inner) {
                    tail_bg = Some((ob, inner.to_string()));
                }
            }
        }

        if head_bg.is_none() && tail_bg.is_none() {
            out.push(line.clone());
            continue;
        }

        let main_end = tail_bg.as_ref().map(|(s, _)| *s).unwrap_or(rest.len());
        let main_text = rest[..main_end].trim().to_string();
        let bg_text = match (&head_bg, &tail_bg) {
            (Some((_, h)), Some((_, t))) => format!("{h} {t}"),
            (Some((_, h)), None) => h.clone(),
            (None, Some((_, t))) => t.clone(),
            (None, None) => unreachable!(),
        };

        if main_text.is_empty() || bg_text.is_empty() {
            out.push(line.clone());
            continue;
        }

        let (main_words, bg_words) = if let Some(words) = line.words.as_ref()
            && !words.is_empty()
        {
            split_outer_bracket_words(words, head_bg.is_some(), tail_bg.is_some())
        } else {
            (None, None)
        };

        let raw_main_start = main_words
            .as_ref()
            .and_then(|ws| ws.first())
            .map(|w| w.start)
            .unwrap_or(line.time);
        let main_end_t = main_words
            .as_ref()
            .and_then(|ws| ws.last())
            .map(|w| w.end)
            .or(line.end);

        let bg_start = bg_words
            .as_ref()
            .and_then(|ws| ws.first())
            .map(|w| w.start)
            .unwrap_or(line.time);
        let bg_end_t = bg_words
            .as_ref()
            .and_then(|ws| ws.last())
            .map(|w| w.end)
            .or(line.end);

        // Keep parent line start <= bg_start so leading head_bg keeps [main, bg] sorted
        // monotonically without detaching bg ahead of main during sorting/binary search.
        let main_start = raw_main_start.min(bg_start);

        let main_line = LyricLine {
            time: main_start,
            end: main_end_t,
            text: main_text,
            words: main_words,
            agent_id: line.agent_id.clone(),
            is_duet: line.is_duet,
            is_bg: false,
            song_part: line.song_part.clone(),
        };
        let bg_line = LyricLine {
            time: bg_start,
            end: bg_end_t,
            text: bg_text,
            words: bg_words,
            agent_id: line.agent_id.clone(),
            is_duet: line.is_duet,
            is_bg: true,
            song_part: None,
        };

        out.push(main_line);
        out.push(bg_line);
        room[idx] = 2;
    }

    // Widen aligned maps when any line split into 2
    if room.contains(&2) {
        let widen_map = |map: &mut HashMap<String, Vec<LyricLine>>| {
            for list in map.values_mut() {
                if list.len() != size {
                    continue;
                }
                let mut widened = Vec::with_capacity(out.len());
                for (i, item) in list.drain(..).enumerate() {
                    widened.push(item);
                    if room[i] == 2 {
                        widened.push(LyricLine::new(-1.0, ""));
                    }
                }
                *list = widened;
            }
        };
        widen_map(&mut lyrics.translations);
        widen_map(&mut lyrics.romanizations);
    }

    lyrics.lines = out;
    lyrics
}

/// Retimes line end bounds to merge/extend gaps cleanly (`LyricsRetiming.retimeLyrics`).
pub fn retime_lyrics(mut lyrics: Lyrics) -> Lyrics {
    let len = lyrics.lines.len();
    if len == 0 {
        return lyrics;
    }

    const OVERLAP_THRESHOLD: f64 = 0.250;
    const GAP_THRESHOLD: f64 = 0.001;
    const MAX_EXT: f64 = 1.300;
    const LAST_EXT: f64 = 0.600;
    const LONG_PAUSE: f64 = 7.0;
    const ASSUMED_MAX_GAP: f64 = 11.0;
    const MIN_HOLD: f64 = 5.0;
    const HOLD_PER_CHAR: f64 = 0.180;

    let mut start = vec![0.0f64; len];
    let mut orig_end = vec![0.0f64; len];
    let mut new_end = vec![0.0f64; len];
    let sec: Vec<bool> = lyrics.lines.iter().map(|l| l.is_bg).collect();

    for i in 0..len {
        let line = &lyrics.lines[i];
        start[i] = line.time;
        let end = line.sung_end().unwrap_or_else(|| {
            let next_diff = lyrics.lines[i + 1..]
                .iter()
                .find(|nl| nl.time > line.time)
                .map(|nl| nl.time);
            let hold = MIN_HOLD.max(line.text.chars().count() as f64 * HOLD_PER_CHAR);
            match next_diff {
                None => line.time + hold,
                Some(ns) if ns - line.time < ASSUMED_MAX_GAP => ns,
                Some(ns) => (line.time + hold).min(ns - LONG_PAUSE),
            }
        });
        orig_end[i] = end;
        new_end[i] = end;
    }

    let mut i = 0;
    while i < len {
        let mut cluster_end = i;
        let mut max_main_end = orig_end[i];
        while cluster_end + 1 < len {
            let next_idx = cluster_end + 1;
            if sec[next_idx] {
                cluster_end += 1;
                max_main_end = max_main_end.max(orig_end[cluster_end]);
                continue;
            }
            if max_main_end - start[next_idx] > OVERLAP_THRESHOLD {
                cluster_end += 1;
                max_main_end = max_main_end.max(orig_end[cluster_end]);
            } else {
                break;
            }
        }

        let cluster_base_end = orig_end[i..=cluster_end]
            .iter()
            .copied()
            .fold(orig_end[i], f64::max);
        let mut cluster_final_end = cluster_base_end;
        if cluster_end + 1 < len {
            let gap = start[cluster_end + 1] - cluster_base_end;
            if gap > GAP_THRESHOLD {
                cluster_final_end += gap.min(MAX_EXT);
            }
        } else {
            cluster_final_end += LAST_EXT;
        }

        for j in i..=cluster_end {
            if sec[j] {
                new_end[j] = orig_end[j];
                continue;
            }
            let mut j_eff = orig_end[j];
            for s in (j + 1)..=cluster_end {
                if !sec[s] {
                    break;
                }
                j_eff = j_eff.max(orig_end[s]);
            }
            let mut cutoff = None;
            let mut prev_eff = j_eff;
            for k in (j + 1)..=cluster_end {
                if sec[k] {
                    continue;
                }
                if j_eff - start[k] <= OVERLAP_THRESHOLD || prev_eff - start[k] <= OVERLAP_THRESHOLD
                {
                    cutoff = Some(start[k]);
                    break;
                }
                let mut k_eff = orig_end[k];
                for s in (k + 1)..=cluster_end {
                    if !sec[s] {
                        break;
                    }
                    k_eff = k_eff.max(orig_end[s]);
                }
                prev_eff = k_eff;
            }
            new_end[j] = cutoff.unwrap_or(cluster_final_end);
        }

        i = cluster_end + 1;
    }

    for (idx, line) in lyrics.lines.iter_mut().enumerate() {
        let candidate = if (new_end[idx] - orig_end[idx]).abs() > GAP_THRESHOLD {
            new_end[idx]
        } else {
            orig_end[idx]
        };
        let adj = orig_end[idx].max(candidate);
        if line.end.is_none_or(|e| adj > e) {
            line.end = Some(adj);
        }
    }

    lyrics
}

fn is_redundant_trailing_punct(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '。' | '，' | '、' | '；' | '：' | '﹒' | '．' | '｡' | '､' | '।'
    )
}

/// Formats line text (`LyricsTextFormatter`: capitalize, strip redundant trailing punctuation, Han conversion).
pub fn format_line_text(
    input: &str,
    capitalize: bool,
    strip_punct: bool,
    han_mode: HanMode,
) -> String {
    let mut text = match han_mode {
        HanMode::Simplified => to_simplified_chinese(input),
        HanMode::Traditional => to_traditional_chinese(input),
        HanMode::None => input.to_string(),
    };

    if strip_punct {
        let trimmed = text.trim_end();
        let chars: Vec<char> = trimmed.chars().collect();
        let mut end = chars.len();
        while end > 0 {
            let c = chars[end - 1];
            if c == '.' && end >= 2 && chars[end - 2] == '.' {
                break; // preserve ".." / "..."
            }
            if is_redundant_trailing_punct(c) {
                end -= 1;
            } else {
                break;
            }
        }
        text = chars[..end].iter().collect();
    }

    if capitalize {
        let mut out = String::with_capacity(text.len());
        let mut done = false;
        for c in text.chars() {
            if !done && c.is_alphabetic() {
                for up in c.to_uppercase() {
                    out.push(up);
                }
                done = true;
            } else {
                out.push(c);
            }
        }
        text = out;
    }

    text
}

fn has_wrapping_parens(s: &str) -> bool {
    let t = s.trim();
    if let Some(first) = t.chars().next()
        && (first == '(' || first == '（')
    {
        let close = if first == '(' { ')' } else { '）' };
        return find_matching_paren(t, 0, first, close)
            .is_some_and(|end_idx| end_idx + close.len_utf8() == t.len());
    }
    false
}

fn wrap_bg_parens(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() || has_wrapping_parens(t) {
        t.to_string()
    } else {
        format!("({t})")
    }
}

fn push_standalone_pipe_line(out: &mut Vec<LyricLine>, mut line: LyricLine) {
    line.text = wrap_bg_parens(&line.text);
    line.is_bg = false;
    if !line.text.trim().is_empty() {
        out.push(line);
    }
}

/// Folds parented `is_bg` lines back inline onto their parent line in sung chronological order
/// for `--pipe` consumers, and promotes standalone `is_bg` lines to printable `"(...)"` lines.
pub fn fold_background_for_pipe(mut lyrics: Lyrics) -> Lyrics {
    let lines = std::mem::take(&mut lyrics.lines);
    if lines.is_empty() {
        return lyrics;
    }

    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0usize;

    while i < lines.len() {
        if lines[i].is_bg {
            push_standalone_pipe_line(&mut out, lines[i].clone());
            i += 1;
            continue;
        }

        let mut main = lines[i].clone();
        let main_start = main.vocal_start();
        let main_raw_end = main.sung_end();

        let mut j = i + 1;
        let mut leading_bgs = Vec::new();
        let mut trailing_bgs = Vec::new();
        let mut merged_any = false;

        while j < lines.len() && lines[j].is_bg {
            let bg = &lines[j];
            let same_phrase = (bg.time - main.time).abs() <= 0.05
                || main_raw_end.is_some_and(|e| bg.time + 0.05 < e);
            if !same_phrase {
                break;
            }
            merged_any = true;
            main.time = main.time.min(bg.time);
            if let Some(bg_end) = bg.sung_end() {
                main.end = Some(main.end.map_or(bg_end, |me| me.max(bg_end)));
            }
            let wrapped = wrap_bg_parens(&bg.text);
            if !wrapped.is_empty() {
                if bg.vocal_start() + f64::EPSILON < main_start {
                    leading_bgs.push(wrapped);
                } else {
                    trailing_bgs.push(wrapped);
                }
            }
            j += 1;
        }

        if merged_any {
            let mut parts = Vec::with_capacity(leading_bgs.len() + trailing_bgs.len() + 1);
            parts.extend(leading_bgs);
            let main_trimmed = main.text.trim();
            if !main_trimmed.is_empty() {
                parts.push(main_trimmed.to_string());
            }
            parts.extend(trailing_bgs);
            main.text = parts.join(" ");
            main.words = None;
        }
        out.push(main);

        // Emit any remaining standalone delayed `is_bg` lines in their own timestamps.
        while j < lines.len() && lines[j].is_bg {
            push_standalone_pipe_line(&mut out, lines[j].clone());
            j += 1;
        }

        i = j;
    }

    lyrics.lines = out;
    lyrics
}

/// Applies full Section B display preparation (`LyricsFilter` -> `LyricsBracketSplitter` -> `LyricsTextFormatter` -> `LyricsRetiming`).
pub fn prepare_lyrics_for_display(lyrics: Lyrics, track: &TrackInfo, cfg: &FilterConfig) -> Lyrics {
    prepare_lyrics_with_mode(lyrics, track, cfg, false)
}

/// Applies `--pipe` preparation: keeps parenthesized vocals inline and folds `is_bg` vocals cleanly.
pub fn prepare_lyrics_for_pipe(lyrics: Lyrics, track: &TrackInfo, cfg: &FilterConfig) -> Lyrics {
    prepare_lyrics_with_mode(lyrics, track, cfg, true)
}

fn prepare_lyrics_with_mode(
    mut lyrics: Lyrics,
    track: &TrackInfo,
    cfg: &FilterConfig,
    pipe_mode: bool,
) -> Lyrics {
    lyrics = filter_credit_lines(lyrics, track, cfg.credit_line_markers.as_deref());
    lyrics = filter_lyrics_text(lyrics, cfg.lyrics_text_filter_regex.as_deref());
    if pipe_mode {
        lyrics = fold_background_for_pipe(lyrics);
    } else if cfg.split_background {
        lyrics = split_bracket_background(lyrics);
    }
    if cfg.force_capitalize || cfg.strip_trailing_punctuation || cfg.han_mode != HanMode::None {
        for line in &mut lyrics.lines {
            line.text = format_line_text(
                &line.text,
                cfg.force_capitalize,
                cfg.strip_trailing_punctuation,
                cfg.han_mode,
            );
            if let Some(words) = line.words.as_mut() {
                let word_count = words.len();
                for (wi, w) in words.iter_mut().enumerate() {
                    let wt = format_line_text(
                        &w.text,
                        cfg.force_capitalize && wi == 0,
                        cfg.strip_trailing_punctuation && wi + 1 == word_count,
                        cfg.han_mode,
                    );
                    let mut nw = create_word_timing(w.start, w.end, &wt);
                    nw.ends_with_space = w.ends_with_space;
                    nw.romaji = w.romaji.clone();
                    nw.ruby = w.ruby.clone();
                    *w = nw;
                }
            }
        }
    }
    lyrics.fix_anomalous_word_timings();
    lyrics.clamp_last_word_ends();
    if cfg.retime_overlaps && lyrics.synced {
        lyrics = retime_lyrics(lyrics);
    }
    lyrics
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_credit_lines_start_and_end() {
        let lines = vec![
            LyricLine::new(0.0, "Written By: Alice"),
            LyricLine::new(1.0, "Produced By: Bob"),
            LyricLine::new(5.0, "Actual first lyric"),
            LyricLine::new(10.0, "Actual second lyric"),
            LyricLine::new(15.0, "Mixed By: Carol"),
        ];
        let lyrics = Lyrics::new(lines, "LRCLIB", true);
        let filtered = filter_credit_lines(lyrics, &TrackInfo::new("Song", "Artist", "", 0), None);
        assert_eq!(filtered.lines.len(), 2);
        assert_eq!(filtered.lines[0].text, "Actual first lyric");
        assert_eq!(filtered.credits.len(), 3);
    }

    #[test]
    fn test_split_bracket_background() {
        let lines = vec![LyricLine::new(10.0, "I want you (Oh yeah)").with_end(Some(14.0))];
        let lyrics = Lyrics::new(lines, "Test", true);
        let split = split_bracket_background(lyrics);
        assert_eq!(split.lines.len(), 2);
        assert_eq!(split.lines[0].text, "I want you");
        assert!(!split.lines[0].is_bg);
        assert_eq!(split.lines[1].text, "Oh yeah");
        assert!(split.lines[1].is_bg);
    }

    #[test]
    fn test_format_line_text_punctuation_and_capitalize() {
        assert_eq!(
            format_line_text("hello world,", true, true, HanMode::None),
            "Hello world"
        );
        assert_eq!(
            format_line_text("wait...", true, true, HanMode::None),
            "Wait..."
        );
        assert_eq!(format_line_text("why?", true, true, HanMode::None), "Why?");
    }

    #[test]
    fn test_split_bracket_background_multiword_head_preserves_middle_words() {
        let words = vec![
            create_word_timing(10.0, 10.4, "(Yeah"),
            create_word_timing(10.4, 10.7, "I"),
            create_word_timing(10.7, 11.0, "know)"),
            create_word_timing(11.2, 12.0, "baby"),
        ];
        let lines = vec![
            LyricLine::new(10.0, "(Yeah I know) baby")
                .with_end(Some(12.0))
                .with_words(Some(words)),
        ];
        let lyrics = Lyrics::new(lines, "Test", true);
        let split = split_bracket_background(lyrics);
        assert_eq!(split.lines.len(), 2);

        let main = &split.lines[0];
        let bg = &split.lines[1];
        assert!(!main.is_bg);
        assert_eq!(main.text, "baby");
        assert!(
            main.time <= bg.time,
            "main.time ({}) must be <= bg.time ({}) so sorting stays attached",
            main.time,
            bg.time
        );
        let main_words: Vec<&str> = main
            .words
            .as_ref()
            .unwrap()
            .iter()
            .map(|w| w.text.as_str())
            .collect();
        assert_eq!(main_words, vec!["baby"]);

        assert!(bg.is_bg);
        assert_eq!(bg.text, "Yeah I know");
        let bg_words: Vec<&str> = bg
            .words
            .as_ref()
            .unwrap()
            .iter()
            .map(|w| w.text.as_str())
            .collect();
        assert_eq!(bg_words, vec!["Yeah", "I", "know"]);
    }

    #[test]
    fn test_filter_lyrics_text_preserves_bg_and_timing_metadata() {
        let lines = vec![
            LyricLine::new(5.0, "   Ooh,   yeah  ★ ")
                .with_end(Some(8.0))
                .with_agent(Some("v2".to_string()), true)
                .with_bg(true),
        ];
        let lyrics = Lyrics::new(lines, "Test", true);
        let filtered = filter_lyrics_text(lyrics, Some("★"));
        assert_eq!(filtered.lines.len(), 1);
        let line = &filtered.lines[0];
        assert_eq!(line.text, "Ooh, yeah");
        assert!(line.is_bg);
        assert_eq!(line.end, Some(8.0));
        assert_eq!(line.agent_id.as_deref(), Some("v2"));
        assert!(line.is_duet);
    }

    #[test]
    fn test_fold_background_for_pipe_merges_ttml_and_keeps_standalone_lrc() {
        let lines = vec![
            // TTML trailing x-bg (e.g. lrc.red L13)
            LyricLine::new(58.906, "We could've had it all")
                .with_end(Some(63.735))
                .with_words(Some(vec![create_word_timing(
                    58.906,
                    63.252,
                    "We could've had it all",
                )])),
            LyricLine::new(60.386, "You're gonna wish you never had met me")
                .with_end(Some(63.735))
                .with_bg(true),
            // TTML leading x-bg (e.g. lrc.red L38)
            LyricLine::new(170.072, "We could've had it all")
                .with_end(Some(177.443))
                .with_words(Some(vec![create_word_timing(173.177, 177.443, "We")])),
            LyricLine::new(170.072, "You're gonna wish you never had met me")
                .with_end(Some(173.050))
                .with_words(Some(vec![create_word_timing(170.072, 173.050, "You're")]))
                .with_bg(true),
            // LRC main + delayed standalone "(Never had met me)"
            LyricLine::new(200.0, "We could've had it all (you're gonna wish you)"),
            LyricLine::new(203.5, "(Never had met me)").with_bg(true),
            // Standalone bg-only line without outer parens
            LyricLine::new(210.0, "Ooh yeah").with_bg(true),
        ];
        let lyrics = Lyrics::new(lines, "Test", true);
        let pipe = prepare_lyrics_for_pipe(
            lyrics,
            &TrackInfo::new("Rolling in the Deep", "Adele", "21", 228),
            &FilterConfig::default(),
        );

        assert_eq!(pipe.lines.len(), 5);
        assert_eq!(
            pipe.lines[0].text,
            "We could've had it all (You're gonna wish you never had met me)"
        );
        assert_eq!(
            pipe.lines[1].text,
            "(You're gonna wish you never had met me) We could've had it all"
        );
        assert_eq!(
            pipe.lines[2].text,
            "We could've had it all (you're gonna wish you)"
        );
        assert_eq!(pipe.lines[3].text, "(Never had met me)");
        assert!(!pipe.lines[3].is_bg);
        assert_eq!(pipe.lines[4].text, "(Ooh yeah)");
        assert!(!pipe.lines[4].is_bg);
    }
}
