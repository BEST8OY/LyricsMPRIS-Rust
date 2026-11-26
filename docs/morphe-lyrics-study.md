# Morphe Music Lyrics Reference Study

Reference study of `morphe-patches` (`app.morphe.extension.music.patches.lyrics`, 156 Java files / ~74.8k LOC) and its feasibility for replication in **LyricsMPRIS-Rust** (Linux D-Bus MPRIS2 + Ratatui TUI + `--pipe` CLI).

---

## 1. High-Level Architecture & Package Map

| Package / File | Role | Linux / LyricsMPRIS-Rust Feasibility |
|---|---|---|
| Root (`LyricsManager`, `DisplayState`, `LyricsProviders`, `LyricsMatch`, `PlaybackClock`, `SkipSegments`, `LyricsPrefetcher`, `LyricsPresentation`, `QueueTracks`, `SessionQueue`, `LyricsPatch`, `LyricsButtonPatch`, `LyricsDirectories`) | Track lifecycle, 3-stage concurrent fetching, SponsorBlock timing, clock smoothing, queue prefetch, Android/YTM hooks | Orchestration, `DisplayState`, `PlaybackClock`, `SkipSegments` (when YouTube `videoId` present) are portable; YTM bytecode hooks are Android-only |
| `model/` (18 files) | `Lyrics`, `LyricsLine`, `Word`, `TrackInfo`, `TrackVersion`, `CreditInfo`, `LyricsPreference`, `LyricsMerge`, `LyricsRetiming`, `LyricsFilter`, `LyricsFilterDefaults`, `LyricsBracketSplitter`, `MetadataCleaner`, `LyricsTextFormatter`, `PairedPunctuation`, `MiniLyricsFormatter`, `MiniLyricsTimeline`, `ArtistLineCycle` | **100% portable** pure data structures & text/timing algorithms |
| `parsers/` (6 files) | `LRCParser`, `KRCParser`, `LyricifyParser`, `LyricsFileParser`, `TTMLParser`, `TTMLGenerator` | **100% portable** |
| `requests/` (28 files) | 22 providers + `LyricsProvider`, `ScoredSongProvider`, `LyricsRequests`, `LyricsEndpoints`, `CaptionsFetcher`, `LocalLyricsFetcher` | 19 zero-auth HTTP + 3 token-gated HTTP + 2 YouTube-ID (`YTMusic`, `Captions`) portable; `LocalLyricsFetcher` replaced by `lofty` |
| `derive/` (7 files) | `LyricsLanguageDetector`, `LyricsTranslator`, `LyricsRomanizer`, `OpenAIClient`, `ProcessingProviders`, `QuickLRCClient`, `SectionDetector` | **Portable** (`QuickLRCClient` requires YouTube `videoId`) |
| `romanizers/` (30 files) | `ScriptDetector`, `LocalRomanizer`, `HangulRomanizer`, `KanaRomanizer`, `Kuromoji*`, `JapaneseDictionaryStore` (Sudachi), `Pinyin*`, `Jieba*`, `UniDictionaryStore`, `Rime*`, `CldrTransforms*`, `AlphabetRomanizer`, `RubyGenerator`, `IcuRomanizer` | Pure-Java engines & runtime dictionary downloaders portable; `IcuRomanizer` requires `icu4c` or pure-Rust replacement |
| `storage/` (3 files) | `LyricsCache`, `LyricsFileSaver`, `TrackOffsetStore` | **Portable** (SQLite + XDG dirs replace Android `filesDir`/`MediaStore`/`SharedPreferences`) |
| `session/` (4 files) | `LockScreenLyrics`, `LyricsNotificationRefresh`, `MiniplayerLyrics`, `LyricsTicker` | **Android-only** (`MediaSession` / `TextView` mutation); Linux uses `--pipe` or desktop notifications |
| `utils/` (3 files) | `CharactersConverter`, `LyricsCrypto`, `TextTrim` | `LyricsCrypto` 100% portable; `CharactersConverter` via `unicode-normalization` + `zhconv`; `TextTrim` built into Rust |
| `ui/` (44 files) | Rendering, `WordSweepAnimator`, `LyricsScrollEngine`, `GapLineView`, `DynamicBackgroundView`, `BoxBlur`, state machines (`SeekState`, `FollowState`, `OnlyModeState`, `OffsetModeState`), dialogs (`LyricsSearchDialog`, `LyricsShareDialog`, `LyricsShareCardRenderer`, `SponsorBlockMusicSubmitDialog`), YTM hooks (`DeviceLyricsTab`, `LyricsPanelInstaller`) | Pure state machines, palette extraction, truecolor sweeps, gap dots, and TUI modals adaptable; Canvas/RenderEffect/YTM Litho hooks are Android-only |

---

## 2. Orchestration & Fetching Pipeline

### Canonical Provider Priority (`LyricsProviders.PROVIDER_ORDER`)
1. `Apple` (Word/TTML via `amp-api` + `media-user-token`, or unauthenticated Paxsenix fallback)
2. `LyricsPlus` (TTML / v2 JSON, multi-host failover)
3. `LrcRed` (TTML + on-demand `/s/<isrc>/sync` AI alignment)
4. `bLyrics` (TTML)
5. `Lyricify` (ISRC via `credits.fm` -> `.lys` syllable / `.lyl` line)
6. `Unison` (TTML / LRC / plain via `videoId` or `/search`)
7. `PetitLyrics` (WSY word XML / LSY XOR binary / LRC)
8. `AMLL` (TTML via `api.amll.dev`)
9. `QQ` (QRC 3DES+zlib word-synced / LRC + `roma` / `trans`)
10. `SimpMusic` (RichSync / LRC / plain via `videoId` or `/search`)
11. `KuGou` (KRC XOR+zlib word-synced / LRC + `[language]` Base64 JSON)
12. `Musixmatch` (Richsync / Subtitles / Plain via `macro.subtitles.get`)
13. `YTMusic` (InnerTube `next` + `browse` `ANDROID_MUSIC` timed lyrics; requires YouTube `videoId`)
14. `LRCLIB` (`/api/get` + `/api/search` upgrade + `lyricsFile` YAML)
15. `NetEase` (EAPI AES-ECB anonymous session; YRC / mixed JSON / LRC + `romalrc` / `tlyric`)
16. `Luna` (Qishui/Douyin KRC/LRC + `lang_translations`)
17. `Kuwo` (XOR+zlib+GB18030 word LRC + mobile JSON fallback)
18. `LunaBeat` (Static GitHub Pages TTML hub)
19. `Captions` (YouTube `player` + `fmt=json3` timedtext; requires YouTube `videoId`)
20. `Spotify` (`sp_dc` cookie + WebPlayer TOTP + `color-lyrics/v2`)
21. `Deezer` (`arl` cookie + GraphQL `SynchronizedTrackLyrics`)
22. `RMM` (iTunes search + `lyrics.rmmreviv.al`)
23. `Genius` (HTML scrape; plain-text only + linked translations + credits)

Disabled syntax: comma-separated list where `-ProviderId` excludes a provider.

### 3-Stage Concurrent Racing & `DisplayState`
- **Query Tiers**:
  - `originalTier`: `(track, penalty=0)`, plus `MetadataCleaner.trustedDashSplit` `(penalty=1)`.
  - `derivedTier`: `CharactersConverter.variants` `(penalty=1)`, `splitArtists` `(penalty=2)`, `anyDashSplit` `(penalty=2)`, `anyDashSplitReversed` `(penalty=2)`.
- **Blind Providers** (`LyricsMatch.isBlindProvider`): `Spotify`, `AMLL`, `bLyrics`, `LyricsPlus`, `Lyricify` (deferred to Stage 3).
- **Stages**:
  - **Stage 1 (0–2s)**: Top 4 enabled non-blind providers × `originalTier`.
  - **Stage 2 (2–4s)**: Remaining non-blind providers × `originalTier` (skipped if Stage 1 closed the window).
  - **Stage 3 (4–9s)**: All providers × `derivedTier` + blind providers × `originalTier`.
  - **Endgame (5s)**: If no primary winner (`bestResult == null`), promotes `bestFallback`.
- **Early Completion & First-Paint Hold**:
  - `syncRank`: `2` = word-synced, `1` = line-synced, `0` = plain.
  - When `wordSync` is enabled: closes early on Stage 1/2 if a `highMatch` with `syncRank == 2` arrives; holds any `rank < 2` result up to `800ms` (`FIRST_PAINT_HOLD_MS`) waiting for word-sync before painting.
  - When `wordSync` is off: closes early on Stage 1/2 `syncRank == 1`.
  - **Cut-off retry**: If track duration `>= 60s` and last lyric ends `< 15%` of track length, retries once.
  - **Background Queue Fill**: Remaining unqueried providers populate a `PriorityQueue<ScoredCandidate>` (`composite = matchScore * 10 + syncRank - penalty`) for cycling via Refresh (`r`).

### Metadata Scoring (`LyricsRequests.PreparedTrack`)
0–11 scale:
- **Title (0–5)**: `EQUAL` (5), `containsEither` (3), Jaccard `>= 0.6` (2), `>= 0.35` (1).
- **Artist (0–3)**: `EQUAL`/`containsEither` (3), `splitArtistHits` on `和|&|feat|ft|,|/|×` (1), mismatch (0).
- **Duration (0–2)**: `|Δ| <= 2s` (2), `<= 5s` (1), `<= 15s` (0), `> 15s` (`MISMATCH`).
- **Album (0–1)**: `EQUAL`/`containsEither` (1).
- **`isHighMatch`**: Vetoes `title == NONE/MISSING` or `artist == MISMATCH`. Passes if `score >= 7 && (duration != MISMATCH || artist == MATCH)` OR `title == EQUAL && duration != MISMATCH && album != MISMATCH`. `SOFT_MIN = 2`.

---

## 3. Data Model, Text Processing & Parsers (`model/`, `parsers/`)

### Core Model
- `Word(startMs, endMs, text, romaji, endsWithSpace, ruby)`
- `LyricsLine(startTimeMs, endTimeMs, text, words, agentId, isDuet, isBG, songPart)`
  - `hasWordTimings()` rejects single-word dummy spans or all-words matching full line `[startTimeMs, endTimeMs]`.
  - `isCentered()` matches `agentId` `"v1000"` or `"v2000"`.
- `Lyrics(lines, providerName, synced, romanization, translations, romanizations, songwriters, credits, rawFormat, formatType, sourceUrl, about, agentNames)`
  - Background vocal tracks in `translations`/`romanizations` use `"bg:"` key prefix.
  - `fixAnomalousWordTimestamps` + `clampLastWordEnds` repair `0`/negative/equal word timings and enforce `>= 200ms` tail visibility without crossing `nextNonBgLine`.
  - `indexForPosition` rewinds past `isBG()` lines.

### Text Processing Pipeline
- `MetadataCleaner`: normalizes via `CharactersConverter`, strips clutter (`Official Video`, `Remastered`, `【插曲】`, `- Topic`) via `LYRICS_CUSTOM_REGEX`, splits featured artists & `Artist - Title` dashes.
- `LyricsFilter`:
  - `filterCreditLines`: strips credit-role lines (`~300` EN/ZH markers) and `Artist - Title` headers from contiguous start/end blocks (`CREDIT_LINE_GAP_TOLERANCE = 3`), deduplicating into `credits`.
  - `filterLyricsText`: strips Chinese provider boilerplate/ads, keeping aligned translation/romanization lists synchronized.
- `LyricsBracketSplitter`: splits leading/trailing `(...)` / `（...）` background vocals on word-timed and line-timed lines into `isBG = true` lines.
- `LyricsRetiming`: clusters overlapping lines (`250ms` threshold), extends clusters into gaps up to `1300ms` (`600ms` tail extension), estimates missing ends (`max(5000ms, len * 180ms)`), and caps active simultaneous main lines to `3`.
- `LyricsTextFormatter`: capitalizes first letter, strips redundant trailing punctuation (preserving `?`, `!`, `...`, quotes, brackets), and performs Simplified/Traditional Han conversion while building a UTF-16 offset map so word timings stay aligned.
- `MiniLyricsFormatter` + `MiniLyricsTimeline` + `ArtistLineCycle`: templates `{t}`, `{a}`, `{v}`, `{lrc}`, `{n}` with `:conditional:` blocks, full-width-weighted line splitting (`PairedPunctuation` 83 pairs + `BreakIterator` + `\X` graphemes), and instrumental break frames.

### Parsers & Generator
- `LRCParser`: standard LRC, Enhanced LRC (`<mm:ss.xx>`/`[mm:ss.xx]`), `[offset:]` (negated), multi-timestamp repetition, `[bg:1]`/`[duet]`/`[vN:]`/`[section]`, `[au:]`/`[by:]`, echo detection (`<= 1200ms`), and LRC serialization.
- `KRCParser`: decrypted KRC `[start,dur]<off,dur,0>word` and NetEase YRC `[start,dur](start,dur,0)word`.
- `LyricifyParser`: `[start,end]text` lines and `word(start,dur)` syllables.
- `LyricsFileParser`: custom YAML subset parser (`metadata`, `lines` with `words`, `plain`).
- `TTMLParser` & `TTMLGenerator`: Apple Music / AMLL / `lrc.red` / BetterLyrics TTML2 (`clock`/`offset`/`SMPTE` times, `itunes:lyricOffset`, `itunes:songPart`, `ttm:agent`, `ttm:role="x-bg"`, `tts:ruby` / `<ruby>`, sidecar `<translations>` & `<transliterations>` aligned by overlap/IoU).

---

## 4. Provider Wire Formats & Auth Summary

| Provider | Endpoint / Transport | Auth / Crypto | Output Formats |
|---|---|---|---|
| `AMLL` | `api.amll.dev/v1/lyrics/{search,get}` | None | TTML |
| `Apple` | `amp-api.music.apple.com` + fallback `itunes.apple.com` + `lyrics.paxsenix.org` | Web JWT scrape + user `media-user-token` (Paxsenix fallback is free) | TTML / ELRC |
| `bLyrics` | `lyrics-api.boidu.dev/getLyrics` | None | TTML |
| `Captions` | `youtubei/v1/player` + `fmt=json3` | YouTube `videoId`, PoToken, optional cookies (`SAPISIDHASH`) | JSON3 (word/line) |
| `Deezer` | `api.deezer.com/search` + `auth.deezer.com/login/arl` + `pipe.deezer.com/api` | User `arl` cookie -> 4m JWT | `dzr.json` / plain |
| `Genius` | `genius.com/api/{search/multi,songs/<id>}` + HTML scrape | Mobile Chrome UA | Plain + credits + translations |
| `KuGou` | `mobiles.kugou.com` + `lyrics.kugou.com/{search,download}` | XOR(`@Gaw^2tGQ61-ÎÒni`) + zlib | KRC (word) / LRC |
| `Kuwo` | `search.kuwo.cn` + `newlyric.kuwo.cn` + `m.kuwo.cn` | XOR(`yeelion`) + Base64 + zlib + `GB18030` | Kuwo word LRC / JSON |
| `LRCLIB` | `lrclib.net/api/{get,search}` | `Lrclib-App-Id` header | LRC / plain / YAML |
| `LRCRed` | `lrc.red/{match,search}.json` + `/s/<isrc>.song.json` + `/sync` | None | TTML |
| `Luna` | `api.qishui.com/luna/search/track` + `beta-luna.douyin.com` | Random device params | KRC / LRC / plain + trans |
| `LunaBeat` | `2755337087.github.io/ttml-hub/api/v1/{manifest,songs}.json` | None | TTML |
| `Lyricify` | `api.credits.fm/v1/resolve/track` + `api.lyricify.app` | Base64(ISRC) | `.lys` / `.lyl` + trans |
| `LyricsPlus` | `lyricsplus.{prjktla.my.id,binimum.org,prjktla.workers.dev}` | None (multi-host failover) | TTML / v2 JSON |
| `Musixmatch` | `apic.musixmatch.com/ws/1.1/` | `MUSIXMATCH_USERTOKEN` or auto `token.get` | Richsync / Subtitles / Plain |
| `NetEase` | `interface.music.163.com/eapi/*` | Anon register + AES-128-ECB (`e82ckenh8dichen8`) + MD5 | YRC / JSON+LRC / LRC |
| `PetitLyrics` | `p0.petitlyrics.com/api/GetPetitLyricsData.php` | `clientAppId=p1110417` | WSY XML / LSY XOR / LRC |
| `QQ` | `u.y.qq.com/cgi-bin/musicu.fcg` | Custom 3DES-ECB (`QRC_SBOX`) + zlib | QRC XML / LRC + `roma` / `trans` |
| `RMM` | `itunes.apple.com/search` + `lyrics.rmmreviv.al/lyrics` | None | TTML / JSON / LRC / plain |
| `SimpMusic` | `api-lyrics.simpmusic.org/v1/{search,<videoId>}` | None (`SimpMusicLyrics/1.0` UA) | RichSync / LRC / plain |
| `Spotify` | `open.spotify.com/api/token` + `pathfinder/v2` + `color-lyrics/v2` | User `sp_dc` + external TOTP secrets | `sp.json` (syllable/line) |
| `Unison` | `unison.boidu.dev/lyrics{/search,/<videoId>}` | Ephemeral P-256 `X-Key-ID` SHA-256 | TTML / LRC / plain |
| `YTMusic` | `music.youtube.com/youtubei/v1/{next,browse}` | Public InnerTube key + YouTube `videoId` | `ytm.json` / plain |

---

## 5. Future Replication Backlog (Deferred Beyond Sections A & B)

For subsequent phases after Sections A & B:
1. **Section C — Translation, Romanization & Ruby (`derive/`, `romanizers/`)**:
   - Port `LyricsLanguageDetector`, `LyricsTranslator` (Google `gtx`, `OpenAIClient`, `DeepLClient`), `SectionDetector`, `HangulRomanizer`/`KanaRomanizer`, and runtime dictionary stores (`Kuromoji`/`Sudachi`/`Unihan`/`pinyin-pro`/`jieba`/`Rime`/`CLDR`) or pure-Rust equivalents (`lindera`, `jieba-rs`, `pinyin`, `wana_kana`), plus TUI dual/only-mode toggling (`OnlyModeState`).
2. **Section D — Richer TUI UX (`ui/`)**:
   - Truecolor RGB per-grapheme sweep (`WordSweepAnimator`), progressive distance dimming (`spotlightAlpha`), 3-dot instrumental gap fills (`GapLineView`), `SeekState` + `FollowState` + interactive MPRIS `Seek`/`PlayPause`, manual search modal (`LyricsSearchDialog` + MusicBrainz), offset ruler (`[-30s..+30s]`), album-art palette extraction (`extractPaletteFromPixels`), split-screen art pane (Kitty/Sixel/half-block), and optional PNG share-card export (`tiny-skia` + `cosmic-text`).
3. **Section E — Persistence & Offline Cache (`storage/`)**:
   - Restore SQLite + zstd cache with `.pref` candidate memory, FNV-1a `contentKey` derived-cache invalidation, `TrackOffsetStore`, and `.lrc`/`.ttml` file export (`TTMLGenerator`).
4. **Section F — YouTube & Pipe Extras (`SkipSegments`, `QuickLRCClient`, `MiniLyricsFormatter`)**:
   - SponsorBlock timeline subtraction/remapping (`SkipSegments`), `QuickLRCClient` forced alignment for YouTube `videoId` tracks, and `MiniLyricsFormatter` + `ArtistLineCycle` for `--pipe` status bars.
