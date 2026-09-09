//! YouTube candidate ranking + auto-pick heuristics.
//!
//! Wraps `DaemonState::search_yt_sync` with a layer that pushes the most
//! likely "official" track to the top of the candidate list, so the user
//! rarely needs to manually pick a result.
//!
//! ## Why
//!
//! `yt-dlp -j ytsearch5:"<artist> <title>"` returns whatever YouTube's
//! own search-order decides (closely tied to view count, recency, and
//! relative engagement) — which means a 5-year-old fan-upload with
//! "Lyrics" in the title often outranks the official 2024 upload from
//! the artist's own channel. The ranking heuristics here re-score
//! every candidate with explicit signals so the official track wins
//! in the common case.
//!
//! ## Pipeline
//!
//! 1. [`query_variants`] expands `<artist> <title>` into 4 ytsearch
//!    queries: raw, "official video", "official audio", and the raw
//!    query **in quotes** (exact-phrase match, which pulls in uploads
//!    whose titles only contain the full phrase). Each variant biases
//!    toward the channel pattern YouTube's official uploads follow
//!    ("Artist - Topic", "VEVO", "(Official)").
//! 2. [`rank_query`] runs every variant **in parallel** (each spawns
//!    its own yt-dlp subprocess), merges, dedupes, scores, applies the
//!    fan-upload + view-count post-passes, and returns the top-`limit`
//!    sorted by score.
//! 3. [`pick_best`] adds the auto-pick decision on top via
//!    [`auto_pick_decision`]: hard vetoes (lyrics videos, reactions,
//!    nightcore, …) can never be auto-picked regardless of their
//!    score, a minimum score floor rejects weak leaders, and the
//!    classic runner-up margin is relaxed when the runner-up is just
//!    another upload of the *same song* (identical title tokens).
//!    When the decision is genuinely uncertain, a **view-count
//!    enrichment pass** fetches popularity for the top candidates and
//!    re-ranks inside each title-equivalent group before deciding.
//! 4. [`resolve_search_source`] routes legacy `ytsearch1:` download
//!    sources (plain `/init`, `init_batch`) through the same pipeline
//!    so background downloads never depend on raw yt-dlp search order.
//!
//! Scoring constants live next to the scorer so the code is its own
//! spec — adjust the numbers here when refining the heuristics.

use std::collections::HashSet;
use std::thread;

use log::{debug, warn};
use serde::{Deserialize, Serialize};
use strsim::jaro_winkler;

use crate::state::{DaemonState, YtCandidate};

// =====================================================================
// Scoring constants — pull these into your editor and read them next
// to `score_candidate` when adjusting values.
// =====================================================================

/// Official-artist-channel marker. Bigger than every other positive
/// signal combined so a real official upload will always outrank even
/// the most popular fan upload.
pub const OFFICIAL_CHANNEL_BOOST: i32 = 100;
/// "Official Video" / "Official Music Video" / "Official Audio" phrase
/// in the title. Stacks with `OFFICIAL_CHANNEL_BOOST`.
pub const OFFICIAL_PHRASE_BOOST: i32 = 30;
/// All (non-trivial) tokens from the parsed query title appear in the
/// candidate title. Stops "Best Of" compilations and audio-twin decoys
/// from outscoring the real track.
pub const TITLE_INCLUDES_QUERY_BOOST: i32 = 40;
/// Extra bonus when all artist tokens appear in the candidate title
/// (the "Artist - Title" canonical naming pattern).
pub const TITLE_INCLUDES_ARTIST_BOOST: i32 = 40;
/// Graded title-similarity bonus: full credit (+100) when the title
/// tokens match the query exactly, scaled down for partial matches
/// (a "Greatest Hits" medley containing the title words scores ~20).
pub const TITLE_SIMILARITY_SCALE: f64 = 100.0;
/// Graded artist-similarity bonus: full credit (+30) when the uploader
/// (or title) matches the requested artist token-for-token, scaled
/// down for fuzzy matches. Only awarded when the match clears
/// [`ARTIST_MATCH_REJECT`] — below it the mismatch penalty fires.
pub const ARTIST_SIMILARITY_SCALE: f64 = 30.0;
/// Penalty when the query contains no `Artist - Title` separator: the
/// query is ambiguous (could be album, compilation, cover) and the
/// scorer should prefer candidates that look like a single studio
/// track through the *other* signals instead of assuming.
pub const NO_ARTIST_INFO_PENALTY: i32 = -20;
/// Duration sits in the 2–4 minute "ideal" band.
pub const DURATION_IDEAL_BOOST: i32 = 20;
/// Duration sits in the 2–6 minute "acceptable" band (excluding
/// ideal).
pub const DURATION_ACCEPTABLE_BOOST: i32 = 10;
/// Duration sits in the 6–8 minute "extended" band (long album tracks,
/// bonus editions) — mildly positive, unlike the >8 min penalty.
pub const DURATION_EXTENDED_BOOST: i32 = 5;
/// Duration is over 8 minutes (audiobooks, full live sets).
pub const DURATION_LONG_PENALTY: i32 = -30;
/// Duration is suspiciously short (< 1 minute).
pub const DURATION_SHORT_PENALTY: i32 = -15;
/// yt-dlp didn't report a duration at all (livestream, region-locked).
pub const DURATION_UNKNOWN_PENALTY: i32 = -5;

/// Tokens like "Lyrics", "Lyric Video" in the title.
pub const LYRICS_PENALTY: i32 = -40;
/// Reaction channels (Reaction / Reacts / First Listen).
pub const REACTION_PENALTY: i32 = -90;
/// "Remix" / "Edit" / "Mashup" — skipped when the user typed "remix".
pub const REMIX_PENALTY: i32 = -50;
/// "Live" / "Concert" — skipped when the user typed "live".
pub const LIVE_PENALTY: i32 = -40;
/// Nightcore / Bass Boosted / Slowed / Reverb. These are spam-flag
/// genres; skipping is unconditional because that's not how anybody
/// searches for music they actually want.
pub const SPAM_PENALTY: i32 = -80;
/// "Instrumental" — skipped when the user typed "instrumental".
pub const INSTRUMENTAL_PENALTY: i32 = -60;
/// "Karaoke" — always penalised.
pub const KARAOKE_PENALTY: i32 = -60;
/// "(Cover by …)" / "Cover" uploads — a different artist's rendition
/// of the requested song. Skipped when the user typed "cover".
pub const COVER_PENALTY: i32 = -60;
/// "FREE Type Beat" producer uploads.
pub const TYPE_BEAT_PENALTY: i32 = -50;
/// "Best Of" / "Greatest Hits" / "Compilation" track collections.
pub const COMPILATION_PENALTY: i32 = -45;
/// Requested artist does not fuzzy-match the candidate's
/// uploader / title at all.
pub const ARTIST_MISMATCH_PENALTY: i32 = -60;
/// Small penalty applied to fan uploads when at least one
/// genuinely-official upload (`VEVO`, `- Topic`, `(Official)`)
/// exists in the candidate set. Disappears when no official
/// candidate was found at all, so genuinely obscure tracks aren't
/// over-penalised. Spec: "Fan uploads: Small penalty unless no
/// official upload exists."
pub const FAN_UPLOAD_PENALTY: i32 = -10;

// View-count lean (popularity tie-breaking between uploads of the
// SAME song). Never compares different songs — see
// `apply_view_count_lean`.
/// View count at which an upload is considered the dominant upload of
/// a song (typical official Topic/VEVO video of a released single).
pub const VIEW_COUNT_DOMINANT_THRESHOLD: f64 = 5_000_000.0;
/// View count at which an upload is considered well-known.
pub const VIEW_COUNT_STRONG_THRESHOLD: f64 = 500_000.0;
/// Bonus for the dominant upload when no same-song rival reached the
/// dominant tier.
pub const VIEW_COUNT_DOMINANT_BONUS: i32 = 6;
/// Bonus for a well-known upload when no same-song rival reached the
/// strong tier.
pub const VIEW_COUNT_STRONG_BONUS: i32 = 4;
/// How many top candidates get view-count enrichment when the
/// auto-pick is uncertain.
pub const VIEW_COUNT_ENRICH_TOP_N: usize = 4;

/// Below this Jaro-Winkler similarity the artist match is treated as
/// "missing" → `ARTIST_MISMATCH_PENALTY`.
pub const ARTIST_MATCH_REJECT: f64 = 0.65;

// Duration thresholds (seconds). Bands:
//   ideal      = 2–4 min  → +20
//   acceptable = 2–6 min, excluding ideal → +10
//   extended   = 6–8 min, excluding acceptable → +5
//   long       = > 8 min  → penalty
//   very short = < 1 min  → penalty
//   gaps (1–2, 4–6 crossing, 6–8 crossing) stay neutral on purpose.
pub const IDEAL_SONG_SECS: f64 = 120.0;
pub const IDEAL_SONG_SECS_MAX: f64 = 240.0;
pub const MIN_SONG_SECS: f64 = 120.0;
pub const MAX_SONG_SECS: f64 = 360.0;
pub const EXTENDED_SONG_SECS_MAX: f64 = 480.0;
pub const LONG_SONG_SECS: f64 = 480.0;
pub const VERY_SHORT_SECS: f64 = 60.0;

/// Margin (in score points) by which the top candidate must beat the
/// runner-up before `pick_best` decides to auto-select, unless the
/// runner-up is title-equivalent (another upload of the same song) or
/// the [`STRONG_PICK_MARGIN`] / [`EQUIVALENT_MARGIN`] floors apply.
pub const AUTO_PICK_MARGIN: i32 = 30;

/// Auto-pick decision floors — the auto-pick must beat these even when
/// the caller passes a smaller margin, because below them the scorer
/// simply has not seen enough signal to be sure the winner is the
/// right song:
/// - [`STRONG_PICK_MARGIN`] applies when the runner-up is a *different*
///   song (the normal case): the winner needs a clear, well-scored
///   lead.
/// - [`EQUIVALENT_MARGIN`] applies when the runner-up is another upload
///   of the *same* song (identical title tokens): the "is this the
///   right song?" question is already answered, so only a small margin
///   is needed to settle "which upload".
pub const STRONG_PICK_MARGIN: i32 = 25;
pub const EQUIVALENT_MARGIN: i32 = 15;

/// Auto-pick score floor: a winner below this total has too little
/// positive evidence (no official channel, weak title match, odd
/// duration …) to be trusted even with a large margin over the
/// runner-up — the whole field may simply be bad.
pub const HARD_VETO_MIN_SCORE: i32 = 60;

/// How many runner-ups are scanned for title-equivalence when relaxing
/// the auto-pick margin.
pub const EQUIVALENCE_SCAN_DEPTH: usize = 3;

/// Extra candidates fetched per variant beyond what the caller asked
/// for, to leave headroom after dedupe + scoring. Generous on purpose:
/// the scorer is cheap, yt-dlp network round-trips are not, and a
/// wider net catches the official upload even when the plain query
/// doesn't surface it.
pub const PER_VARIANT_LIMIT_BUMP: usize = 6;
/// Lower bound on per-variant ytsearch limit even when the caller asks
/// for fewer.
pub const MIN_PER_VARIANT_LIMIT: usize = 8;

// =====================================================================
// Token helpers — normalisation, decoration stripping, similarity.
//
// Matching notes:
// - Flag scans ("lyrics", "live", "cover", …) match against WHOLE
//   WORDS, not substrings: "Alive (Remaster)" must not trigger the
//   `live` penalty and "Discover" must not trigger `cover`.
// - Title similarity compares against the title with parenthesised /
//   bracketed decorations removed, so "(Official Video)" doesn't
//   dilute the token overlap of the actual song words. Flag scans use
//   the full title — "(Lyric Video)" still counts as a lyrics video.
// - Artist matching is token-based first (exact word overlap) and
//   falls back to per-word Jaro-Winkler, so "Haeftbefehl" still
//   matches "Haftbefehl" and "2hermanoz" does not match
//   "DOS HERMANOS" (the test below locks that regression in).
// =====================================================================

/// Lowercase and collapse whitespace.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Lowercase and remove parenthesised/bracketed decoration segments —
/// `(Official Video)`, `[4K]`, `(Lyric Video)` … — so decorative spam
/// tokens don't dilute the title-similarity signal. The *raw* title is
/// still what flag checks scan, so a `(Lyric Video)` decoration keeps
/// triggering the lyrics penalty while no longer dragging down the
/// token similarity of the surrounding words.
fn strip_decorations(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut paren = 0usize;
    let mut brack = 0usize;
    for ch in lower.chars() {
        match ch {
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            '[' => brack += 1,
            ']' => brack = brack.saturating_sub(1),
            _ if paren == 0 && brack == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Whitespace word tokens with leading/trailing punctuation trimmed.
/// Kept whole otherwise — `don't` stays `don't`, `(Edit)` becomes
/// `edit`, and `Editor's` stays distinct from `edit`.
fn tokenize(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect()
}

/// Token-set similarity in [0,1]: |A∩B| / |A∪B| with multiplicity.
/// Order-free, so `Artist — Title (Official)` normalises to the same
/// set as `Artist Title`. Empty inputs score 0.
fn token_set_sim(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut matched = 0usize;
    let mut pool: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
    for t in a {
        if let Some(i) = pool.iter().position(|p| p == t) {
            matched += 1;
            pool.remove(i);
        }
    }
    let union = a.len() + b.len() - matched;
    matched as f64 / union.max(1) as f64
}

/// Best per-word Jaro-Winkler similarity between `word` and any token
/// of `tokens`. Catches typos and spelling variants the token-set
/// comparison misses ("Haeftbefehl" ≈ "Haftbefehl").
fn word_similarity(word: &str, tokens: &[String]) -> f64 {
    tokens
        .iter()
        .map(|t| jaro_winkler(word, t))
        .fold(0.0_f64, |acc, x| acc.max(x))
}

// =====================================================================
// ParsedQuery — split `<artist> <title>` into proper halves so we can
// do artist-matching without trusting only the title.
// =====================================================================

/// Parsed-out (artist, title, raw) for ranking and artist-matching.
/// `artist` is `None` when the query can't be split cleanly; the
/// scorer applies `NO_ARTIST_INFO_PENALTY` and leans on the remaining
/// signals rather than guessing.
#[derive(Clone, Debug)]
pub struct ParsedQuery {
    pub artist: Option<String>,
    pub title: String,
    /// The full input the user typed, kept verbatim so we can detect
    /// `remix`, `live`, `instrumental`, etc. user-intent modifiers.
    pub raw: String,
}

impl ParsedQuery {
    /// Try common separators in order. Falls back to "all of the
    /// query is the title, no artist info" when nothing matches.
    /// Returns `None` for empty/whitespace input so the ranker
    /// can short-circuit (ytsearch has nothing useful to ask for).
    ///
    /// Accepts both spaced and bare separators so copy-pasted
    /// metadata like `Artist:Title` (no surrounding spaces) and
    /// `Artist - Title` both parse cleanly. Tokenizing beyond
    /// the first occurrence is intentional — callers can always
    /// pass the raw query if the heuristic misfires.
    pub fn parse(query: &str) -> Option<Self> {
        let q = query.trim();
        if q.is_empty() {
            return None;
        }
        for sep in [" - ", " | "] {
            if let Some(idx) = q.find(sep) {
                let a = q[..idx].trim();
                let t = q[idx + sep.len()..].trim();
                if !a.is_empty() && !t.is_empty() {
                    return Some(Self {
                        artist: Some(a.to_string()),
                        title: t.to_string(),
                        raw: q.to_string(),
                    });
                }
            }
        }
        // Colon: any of ` : `, ` :`, `: `, or bare `:` — handlers
        // trim afterwards, so whitespace variants collapse to the
        // same split.
        if let Some(idx) = q.find(':') {
            let a = q[..idx].trim();
            let t = q[idx + 1..].trim();
            if !a.is_empty() && !t.is_empty() {
                return Some(Self {
                    artist: Some(a.to_string()),
                    title: t.to_string(),
                    raw: q.to_string(),
                });
            }
        }
        Some(Self {
            artist: None,
            title: q.to_string(),
            raw: q.to_string(),
        })
    }
}

// =====================================================================
// Query variants — expand a query into 4 ytsearch calls that together
// surface both the user's own-channel uploads and YouTube's
// auto-generated "Topic" channels.
// =====================================================================

/// Expand `<artist> <title>` into a small batch of ytsearch-friendly
/// queries. The first element is always the raw query (so dedup-by-id
/// can collapse matches across variants). Subsequent entries bias
/// toward the "tagged Artist's <Title> [Official…]" pattern that
/// official uploads follow; the final quoted variant forces an
/// exact-phrase match for uploads whose titles contain the full
/// phrase but rank poorly under YouTube's loose matching.
pub fn query_variants(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    let stripped = strip_official_suffix(raw);
    let mut out = vec![
        stripped.to_string(),
        format!("{stripped} official video"),
        format!("{stripped} official audio"),
        format!("\"{stripped}\""),
    ];
    let mut seen = HashSet::new();
    out.retain(|v| seen.insert(v.clone()));
    out
}

/// Strip a trailing "official video" / "official music video" /
/// "official audio" suffix from the query so re-adding it doesn't
/// double the term.
fn strip_official_suffix(s: &str) -> &str {
    let lower = s.to_ascii_lowercase();
    for suf in ["official music video", "official video", "official audio"] {
        if let Some(rest) = lower.strip_suffix(suf) {
            let trimmed = rest.trim_end_matches(|c: char| c == '-' || c.is_whitespace());
            return s[..trimmed.len()].trim_end();
        }
    }
    s
}

// =====================================================================
// ScoreBreakdown — every scoring field, kept around for the GUI/CLI
// to render "why was this picked" tooltips.
// =====================================================================

/// Per-candidate score breakdown that the ranker and the GUI/CLI both
/// read. `total` is what sorting + auto-pick decisions use; the rest
/// are surfaced as badges. Older clients (GUI mirror struct) simply
/// ignore fields they don't know — everything is `#[serde(default)]`.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ScoreBreakdown {
    pub total: i32,
    #[serde(default)]
    pub official_channel: i32,
    #[serde(default)]
    pub official_phrase: i32,
    #[serde(default)]
    pub title_match: i32,
    /// Graded token-set similarity between query and title (0–100).
    #[serde(default)]
    pub title_similarity: i32,
    /// Penalty bucket: artist mismatch, or "no artist info" when the
    /// query couldn't be split.
    #[serde(default)]
    pub artist_match: i32,
    /// Graded artist-vs-uploader/title similarity bonus (0–30).
    #[serde(default)]
    pub artist_similarity: i32,
    #[serde(default)]
    pub duration: i32,
    #[serde(default)]
    pub fan_upload: i32,
    /// View-count lean between uploads of the same song (post-pass).
    #[serde(default)]
    pub view_count: i32,
    #[serde(default)]
    pub lyrics: i32,
    #[serde(default)]
    pub reaction: i32,
    #[serde(default)]
    pub remix: i32,
    #[serde(default)]
    pub live: i32,
    #[serde(default)]
    pub spam: i32,
    #[serde(default)]
    pub instrumental: i32,
    #[serde(default)]
    pub karaoke: i32,
    #[serde(default)]
    pub cover: i32,
    #[serde(default)]
    pub type_beat: i32,
    #[serde(default)]
    pub compilation: i32,
}

/// YtCandidate + score + flags. `score` is the breakdown `total` so
/// consumers can sort cheaply; the full `breakdown` is included for
/// the GUI's "why was this picked?" tooltip.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RankedCandidate {
    #[serde(flatten)]
    pub base: YtCandidate,
    pub score: i32,
    /// Full score breakdown so the GUI/CLI can render "official +35",
    /// "lyrics -40" etc. `None` if the scorer short-circuited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breakdown: Option<ScoreBreakdown>,
    /// Detected badges (`official`, `lyrics`, `live`, `reaction`,
    /// `remix`, `nightcore`, `slowed`, `bass-boosted`, `reverb`,
    /// `instrumental`, `karaoke`, `cover`, `type-beat`, `compilation`,
    /// `long`, `short`, `artist-mismatch`, `fan-upload`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

// =====================================================================
// score_candidate — the heart of the ranking. Returns (breakdown, flags).
// =====================================================================

/// Score a single yt-dlp candidate against the parsed query.
/// Populates every breakdown field except `fan_upload` (owned by
/// [`apply_fan_upload_penalty`]) and `view_count` (owned by the
/// view-count post-passes).
pub fn score_candidate(pq: &ParsedQuery, c: &YtCandidate) -> (ScoreBreakdown, Vec<String>) {
    let mut s = ScoreBreakdown::default();
    let mut flags: Vec<String> = Vec::new();

    let raw_lower = pq.raw.to_ascii_lowercase();
    let title_lower = c.title.to_ascii_lowercase();
    let uploader_lower = c.uploader.to_ascii_lowercase();

    // Token prep — see the matching-notes block above `token_set_sim`.
    let query_tokens: Vec<String> = pq
        .title
        .split_whitespace()
        .filter(|w| w.chars().count() > 2)
        .map(|w| w.to_ascii_lowercase())
        .collect();
    let artist_tokens: Vec<String> = pq
        .artist
        .as_deref()
        .map(|a| {
            a.split_whitespace()
                .filter(|w| w.chars().count() > 2)
                .map(|w| w.to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();
    let title_core_tokens = tokenize(&strip_decorations(&c.title));
    let title_all_tokens = tokenize(&normalize(&c.title));

    // ---- Positive signals ----------------------------------------

    // Official artist channel.
    // YouTube's auto-generated "Topic" channels (e.g. "Artist - Topic")
    // are official uploads by definition — the copyright holder
    // registered them via YouTube's Content ID. VEVO is the legacy
    // official channel provider. "(Official)" / "Official Artist
    // Channel" appear in the uploader string when YouTube has flagged
    // the channel.
    let official_markers = ["vevo", " - topic", "(official)", "official artist channel"];
    if official_markers.iter().any(|kw| uploader_lower.contains(kw)) {
        s.official_channel = OFFICIAL_CHANNEL_BOOST;
        flags.push("official".to_string());
    }

    // Title contains ALL non-trivial tokens from the parsed title, and
    // separately all artist tokens (canonical "Artist - Title" naming).
    if !query_tokens.is_empty() && query_tokens.iter().all(|t| title_core_tokens.contains(t)) {
        s.title_match += TITLE_INCLUDES_QUERY_BOOST;
    }
    if !artist_tokens.is_empty() && artist_tokens.iter().all(|t| title_core_tokens.contains(t)) {
        s.title_match += TITLE_INCLUDES_ARTIST_BOOST;
    }

    // Graded title similarity: exact token overlap → +100, partial
    // overlap (compilations, medleys, decoys sharing one word) scales
    // down toward 0.
    let title_sim = token_set_sim(&query_tokens, &title_core_tokens);
    s.title_similarity = (title_sim * TITLE_SIMILARITY_SCALE).floor() as i32;

    // "Official Video" / "Official Music Video" / "Official Audio" in title.
    let official_phrases = ["official music video", "official video", "official audio"];
    if official_phrases.iter().any(|p| title_lower.contains(p)) {
        s.official_phrase = OFFICIAL_PHRASE_BOOST;
        if !flags.iter().any(|f| f == "official") {
            // Only push the "official-upload" tag if we didn't already
            // tag the channel as official — keeps the badge list short.
            flags.push("official-upload".to_string());
        }
    }

    // Artist matching: requested artist vs uploader / title / channel.
    // `pick_uploader` in state.rs prefers the channel string, so the
    // `uploader` field IS the channel-name-equivalent for most videos.
    // Token-set similarity first (exact word overlap), per-word
    // Jaro-Winkler second (typos / spelling variants).
    match &pq.artist {
        Some(artist) => {
            let artist_lower = artist.to_ascii_lowercase();
            if !artist_lower.is_empty() {
                let up_tokens = tokenize(&normalize(&c.uploader));
                let word_sim = artist
                    .split_whitespace()
                    .filter(|w| !w.is_empty())
                    .map(|w| {
                        let w = w.to_ascii_lowercase();
                        word_similarity(&w, &up_tokens).max(word_similarity(&w, &title_all_tokens))
                    })
                    .fold(0.0_f64, f64::max);
                let token_sim = token_set_sim(&artist_tokens, &up_tokens)
                    .max(token_set_sim(&artist_tokens, &title_all_tokens));
                let best = token_sim.max(word_sim);
                if best < ARTIST_MATCH_REJECT {
                    s.artist_match = ARTIST_MISMATCH_PENALTY;
                    flags.push("artist-mismatch".to_string());
                } else {
                    s.artist_similarity = (best * ARTIST_SIMILARITY_SCALE).floor() as i32;
                }
            }
        }
        None => {
            // Ambiguous query — no separator to split artist from
            // title. Don't guess the artist, but don't hand out a free
            // pass either; the remaining signals decide.
            if !query_tokens.is_empty() {
                s.artist_match = NO_ARTIST_INFO_PENALTY;
            }
        }
    }

    // Duration scoring — check the outlier bands FIRST so the
    // neutral gaps between them don't accidentally swallow penalties
    // (a 10-minute podcast used to fall through the 2–6 window checks
    // unpenalised when the bands were evaluated in the wrong order).
    let dur = c.duration_secs;
    if dur <= 0.0 {
        s.duration = DURATION_UNKNOWN_PENALTY;
    } else if dur >= IDEAL_SONG_SECS && dur <= IDEAL_SONG_SECS_MAX {
        s.duration = DURATION_IDEAL_BOOST;
    } else if dur > LONG_SONG_SECS {
        s.duration = DURATION_LONG_PENALTY;
        flags.push("long".to_string());
    } else if dur < VERY_SHORT_SECS {
        s.duration = DURATION_SHORT_PENALTY;
        flags.push("short".to_string());
    } else if dur >= MIN_SONG_SECS && dur <= MAX_SONG_SECS {
        s.duration = DURATION_ACCEPTABLE_BOOST;
    } else if dur <= EXTENDED_SONG_SECS_MAX {
        s.duration = DURATION_EXTENDED_BOOST;
    }

    // ---- Negative signals -----------------------------------------
    // Each has a corresponding "user opted in" guard so a search for
    // "song remix" doesn't penalise the remix itself. Spam-genre flags
    // (nightcore & co) are unconditional.

    let user_wants = |kw: &str| raw_lower.contains(kw);
    // Whole-word checks against the full title tokens — substring
    // matching here caused "Alive" → live and "Discover" → cover.
    let word_has = |terms: &[&str]| title_all_tokens.iter().any(|t| terms.contains(&t.as_str()));

    if !user_wants("lyric") && title_lower.contains("lyric") {
        s.lyrics = LYRICS_PENALTY;
        flags.push("lyrics".to_string());
    }
    if !user_wants("reaction")
        && (title_lower.contains("reaction") || word_has(&["reacts", "reacting"]))
    {
        s.reaction = REACTION_PENALTY;
        flags.push("reaction".to_string());
    }
    if !user_wants("remix") && (word_has(&["remix", "mashup", "edit"]) || title_lower.contains("mashup"))
    {
        s.remix = REMIX_PENALTY;
        flags.push("remix".to_string());
    }
    if !user_wants("live") && word_has(&["live", "concert"]) {
        s.live = LIVE_PENALTY;
        flags.push("live".to_string());
    }
    if title_lower.contains("nightcore") {
        s.spam += SPAM_PENALTY;
        flags.push("nightcore".to_string());
    }
    if title_lower.contains("bass boosted") || word_has(&["bass-boosted", "bassboosted"]) {
        s.spam += SPAM_PENALTY;
        flags.push("bass-boosted".to_string());
    }
    if word_has(&["slowed", "slowed+reverb", "sped"]) {
        s.spam += SPAM_PENALTY;
        flags.push("slowed".to_string());
    }
    if word_has(&["reverb"]) {
        s.spam += SPAM_PENALTY;
        flags.push("reverb".to_string());
    }
    if !user_wants("instrumental") && word_has(&["instrumental"]) {
        s.instrumental = INSTRUMENTAL_PENALTY;
        flags.push("instrumental".to_string());
    }
    if word_has(&["karaoke"]) {
        s.karaoke = KARAOKE_PENALTY;
        flags.push("karaoke".to_string());
    }
    if !user_wants("cover") && word_has(&["cover", "covers"]) {
        s.cover = COVER_PENALTY;
        flags.push("cover".to_string());
    }
    if !(user_wants("type beat") || user_wants("type-beat"))
        && (title_lower.contains("type beat") || title_lower.contains("type-beat"))
    {
        s.type_beat = TYPE_BEAT_PENALTY;
        flags.push("type-beat".to_string());
    }
    if !user_wants("compilation")
        && (title_lower.contains("compilation")
            || title_lower.contains("best of")
            || word_has(&["compilation"]))
    {
        s.compilation = COMPILATION_PENALTY;
        flags.push("compilation".to_string());
    }

    // `fan_upload` is updated by `apply_fan_upload_penalty` after
    // scoring; deliberately omitted from the initial total sum so
    // the post-pass owns the bookkeeping and syncs `rc.score = b.total`.
    s.total = s.official_channel
        + s.official_phrase
        + s.title_match
        + s.title_similarity
        + s.artist_match
        + s.artist_similarity
        + s.duration
        + s.lyrics
        + s.reaction
        + s.remix
        + s.live
        + s.spam
        + s.instrumental
        + s.karaoke
        + s.cover
        + s.type_beat
        + s.compilation;
    (s, flags)
}

// =====================================================================
// dedupe_candidates — drop exact-id duplicates first, then near-by
// title + uploader copies so VEVO + Topic + "Full Album" copies of
// the same track collapse into one ranked row.
// =====================================================================

pub fn dedupe_candidates(candidates: Vec<YtCandidate>) -> Vec<YtCandidate> {
    let mut out: Vec<YtCandidate> = Vec::new();
    for c in candidates {
        if out.iter().any(|existing| existing.id == c.id) {
            continue;
        }
        let dup = out.iter().any(|existing| {
            let t_sim = jaro_winkler(
                &existing.title.to_ascii_lowercase(),
                &c.title.to_ascii_lowercase(),
            );
            let u_sim = jaro_winkler(
                &existing.uploader.to_ascii_lowercase(),
                &c.uploader.to_ascii_lowercase(),
            );
            t_sim > 0.85 && u_sim > 0.70
        });
        if !dup {
            out.push(c);
        }
    }
    out
}

// =====================================================================
// Fan-upload penalty — small -10 to non-official candidates when at
// least one official upload exists in the result set. Spec line:
// "Fan uploads: Small penalty unless no official upload exists."
// =====================================================================

/// True when the candidate's flags mark it as an official channel
/// upload or an official-phrase upload.
fn is_official_flags(flags: &[String]) -> bool {
    flags.iter().any(|f| f == "official" || f == "official-upload")
}

/// Apply `FAN_UPLOAD_PENALTY` to every candidate that is NOT marked
/// as an official channel upload, when the result set contains at
/// least one such official candidate. No-op otherwise.
pub fn apply_fan_upload_penalty(ranked: &mut [RankedCandidate]) {
    if !ranked.iter().any(|rc| is_official_flags(&rc.flags)) {
        return;
    }
    for rc in ranked.iter_mut() {
        if is_official_flags(&rc.flags) {
            continue;
        }
        // Single-source the bookkeeping: mutate the breakdown first,
        // then sync `rc.score` to it so the two never drift.
        if let Some(b) = rc.breakdown.as_mut() {
            b.fan_upload += FAN_UPLOAD_PENALTY;
            b.total += FAN_UPLOAD_PENALTY;
            rc.score = b.total;
        } else {
            rc.score += FAN_UPLOAD_PENALTY;
        }
        if !rc.flags.iter().any(|f| f == "fan-upload") {
            rc.flags.push("fan-upload".to_string());
        }
    }
}

// =====================================================================
// View-count lean — popularity tie-breaking WITHIN groups of
// candidates that share identical title tokens (i.e. several uploads
// of the same song). Never compares different songs: popularity is
// meaningless for "is this the right song at all", which is what the
// score decides — but between two uploads of the same track, the
// 40M-view video is virtually always the canonical one.
// =====================================================================

/// Apply `VIEW_COUNT_*` bonuses to candidates whose title tokens match
/// another candidate's but whose view count towers over every rival in
/// that group. Only fires on candidates that actually carry a
/// view count (enriched via `enrich_with_view_counts`); a no-op
/// otherwise.
pub fn apply_view_count_lean(ranked: &mut [RankedCandidate]) {
    if ranked.len() < 2 {
        return;
    }
    let token_sets: Vec<Vec<String>> = ranked
        .iter()
        .map(|rc| tokenize(&strip_decorations(&rc.base.title)))
        .collect();

    for i in 0..ranked.len() {
        // Idempotence guard: this pass runs once in `rank_query` and
        // again after view-count enrichment in `pick_best` — the
        // breakdown field records that a lean was already applied so
        // the bonus can never stack.
        if ranked[i]
            .breakdown
            .as_ref()
            .is_some_and(|b| b.view_count != 0)
        {
            continue;
        }
        let vc = ranked[i].base.view_count;
        if vc <= 0.0 {
            continue;
        }
        // Largest view count among title-equivalent rivals.
        let best_rival = (0..ranked.len())
            .filter(|&j| j != i && token_sets[j] == token_sets[i])
            .map(|j| ranked[j].base.view_count)
            .fold(0.0_f64, f64::max);
        if best_rival <= 0.0 {
            continue;
        }
        let lean = if vc >= VIEW_COUNT_DOMINANT_THRESHOLD && best_rival < VIEW_COUNT_DOMINANT_THRESHOLD
        {
            VIEW_COUNT_DOMINANT_BONUS
        } else if vc >= VIEW_COUNT_STRONG_THRESHOLD && best_rival < VIEW_COUNT_STRONG_THRESHOLD {
            VIEW_COUNT_STRONG_BONUS
        } else {
            0
        };
        if lean != 0 {
            if let Some(b) = ranked[i].breakdown.as_mut() {
                b.view_count += lean;
                b.total += lean;
            }
            ranked[i].score += lean;
        }
    }
}

/// Fetch view counts for the top `VIEW_COUNT_ENRICH_TOP_N` candidates
/// (one batched yt-dlp call) and fill any missing `view_count` values.
/// Callers re-run [`apply_view_count_lean`] (idempotent) afterwards to
/// turn the fresh numbers into score adjustments. Used by `pick_best`
/// when the auto-pick decision is uncertain: the extra network
/// round-trip is only paid when it can actually change the outcome.
/// Failures are logged and leave the ranking untouched.
fn enrich_with_view_counts(ranked: &mut [RankedCandidate]) {
    let head = ranked.len().min(VIEW_COUNT_ENRICH_TOP_N);
    let ids: Vec<String> = ranked[..head].iter().map(|rc| rc.base.id.clone()).collect();
    let counts = match DaemonState::search_yt_view_counts(&ids) {
        Ok(m) => m,
        Err(e) => {
            debug!("view-count enrichment skipped: {e}");
            return;
        }
    };
    for rc in ranked[..head].iter_mut() {
        let Some(vc) = counts.get(&rc.base.id).copied() else {
            continue;
        };
        if vc > 0.0 && rc.base.view_count <= 0.0 {
            rc.base.view_count = vc;
        }
    }
}

// =====================================================================
// rank_query — fetch all variants in parallel, merge, dedupe, score.
// =====================================================================

/// Run every variant in parallel, merge + dedupe + score, return the
/// top-`limit` candidates sorted by score descending. An empty query
/// returns an empty list (no error — the ranker is happy to be
/// no-op'd). yt-dlp subprocess failures are logged and the surviving
/// variants still produce results.
pub fn rank_query(query: &str, limit: usize) -> Result<Vec<RankedCandidate>, String> {
    let pq = match ParsedQuery::parse(query) {
        Some(pq) => pq,
        None => return Ok(Vec::new()),
    };
    let variants = query_variants(&pq.raw);
    if variants.is_empty() {
        return Ok(Vec::new());
    }

    let per_variant_limit = (limit + PER_VARIANT_LIMIT_BUMP).max(MIN_PER_VARIANT_LIMIT);
    debug!(
        "rank_query: query={:?}, {} variants, per-variant limit {}",
        query,
        variants.len(),
        per_variant_limit
    );

    // Spawn one thread per variant. Each thread calls yt-dlp
    // independently and returns its parsed candidate list. yt-dlp
    // subprocesses don't share state, so this is safe and skips the
    // 4x latency hit of fetching variants serially.
    let threads: Vec<_> = variants
        .iter()
        .map(|v| {
            let v = v.clone();
            thread::Builder::new()
                .name(format!("sjnmusic-rank-{}", short_token(&v)))
                .spawn(move || {
                    DaemonState::search_yt_sync(&v, per_variant_limit).unwrap_or_default()
                })
        })
        .collect();

    let mut all: Vec<YtCandidate> = Vec::new();
    for t in threads {
        match t {
            Ok(handle) => match handle.join() {
                Ok(mut r) => all.append(&mut r),
                Err(_) => warn!("rank_query: variant fetcher thread panicked"),
            },
            Err(e) => warn!("rank_query: spawn failed: {e}"),
        }
    }

    let deduped = dedupe_candidates(all);
    debug!(
        "rank_query: {} unique candidates after dedupe (from {} raw)",
        deduped.len(),
        variants.len() * per_variant_limit
    );

    let mut ranked: Vec<RankedCandidate> = deduped
        .into_iter()
        .map(|c| {
            let (score, flags) = score_candidate(&pq, &c);
            RankedCandidate {
                base: c,
                score: score.total,
                breakdown: Some(score),
                flags,
            }
        })
        .collect();

    // Post-passes, in order: fan-upload penalty first (it needs the
    // raw official flags), then the view-count lean on top of the
    // settled scores.
    apply_fan_upload_penalty(&mut ranked);
    apply_view_count_lean(&mut ranked);

    // Stable-sort descending by score → yt-dlp's original order is
    // the tiebreaker so the most-viewcount-heavy track wins among
    // equals (e.g. all four variants returning the same video).
    ranked.sort_by(|a, b| b.score.cmp(&a.score));

    if limit > 0 && ranked.len() > limit {
        ranked.truncate(limit);
    }
    Ok(ranked)
}

/// Truncate a query for use in a thread name. yt-dlp may spit out
/// queries that confuse thread-name limits otherwise.
fn short_token(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .take(20)
        .collect::<String>()
        .to_ascii_lowercase()
}

// =====================================================================
// Auto-pick decision — veto flags, score floor, equivalence-aware
// margin.
// =====================================================================

/// Flags that disqualify a candidate from AUTO-picking no matter how
/// high it scored. The user can still pick it manually from the
/// ranked list — the veto only stops the daemon from silently
/// downloading the wrong kind of upload.
const AUTO_PICK_VETO_FLAGS: [&str; 12] = [
    "lyrics",
    "reaction",
    "nightcore",
    "bass-boosted",
    "slowed",
    "reverb",
    "live",
    "karaoke",
    "remix",
    "cover",
    "instrumental",
    "artist-mismatch",
];

/// What [`auto_pick_decision`] decided and why. Serialized into
/// `PickResponse::Auto::decision` so the GUI/CLI can show the user
/// *how confident* the auto-pick was without re-deriving it.
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
pub struct AutoPickMeta {
    /// Actual margin between winner and runner-up (`i32::MAX` when
    /// only one candidate exists).
    pub margin: i32,
    /// Margin that was required for a confident auto-pick.
    pub required_margin: i32,
    /// How many runner-ups are title-equivalent uploads of the same
    /// song (0 when the runner-up is a different song).
    pub equivalent_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AutoPickDecision {
    Auto(AutoPickMeta),
    NoPick,
}

/// Decide whether the ranked list is confident enough to auto-pick.
///
/// Order of checks (each strictly cheap-to-expensive):
/// 1. **Hard vetoes** — lyric videos, reactions, nightcore/spam,
///    covers, artist mismatches … can never be auto-picked, even with
///    a huge margin: a +90 lead over other garbage just means the
///    whole result field is garbage.
/// 2. **Fan uploads** never auto-pick while an official channel
///    exists — which is exactly when the `fan-upload` flag is set.
/// 3. **Score floor** — below `HARD_VETO_MIN_SCORE` there isn't
///    enough positive evidence (no official channel, weak title
///    match, odd duration) to trust the leader.
/// 4. **Margin** — the winner must beat the runner-up by
///    `max(margin, STRONG_PICK_MARGIN)`; when the runner-up is just
///    another upload of the same song (identical title tokens), the
///    "which song?" question is already answered and only
///    `EQUIVALENT_MARGIN` is required to settle "which upload".
pub fn auto_pick_decision(ranked: &[RankedCandidate], margin: i32) -> AutoPickDecision {
    if ranked.is_empty() {
        return AutoPickDecision::NoPick;
    }
    let top = &ranked[0];

    if top
        .flags
        .iter()
        .any(|f| AUTO_PICK_VETO_FLAGS.contains(&f.as_str()))
    {
        return AutoPickDecision::NoPick;
    }
    if top.flags.iter().any(|f| f == "fan-upload") {
        return AutoPickDecision::NoPick;
    }
    if top.score < HARD_VETO_MIN_SCORE {
        return AutoPickDecision::NoPick;
    }

    // Title-equivalence scan: are any of the near-top runner-ups just
    // other uploads of the same song?
    let top_tokens = tokenize(&strip_decorations(&top.base.title));
    let equivalents = if top_tokens.is_empty() {
        0
    } else {
        ranked[1..]
            .iter()
            .take(EQUIVALENCE_SCAN_DEPTH)
            .filter(|rc| {
                let t = tokenize(&strip_decorations(&rc.base.title));
                !t.is_empty() && t == top_tokens
            })
            .count()
    };

    // Margin requirement: different songs → caller's margin floored by
    // STRONG_PICK_MARGIN. Title-equivalent runner-ups → the "which song?"
    // question is already answered, so a small fixed EQUIVALENT_MARGIN
    // settles "which upload" (the caller's margin only governs the
    // different-song case).
    let required = if equivalents > 0 {
        EQUIVALENT_MARGIN
    } else {
        margin.max(STRONG_PICK_MARGIN)
    };
    let actual = if ranked.len() == 1 {
        i32::MAX
    } else {
        top.score - ranked[1].score
    };
    if actual < required {
        return AutoPickDecision::NoPick;
    }

    AutoPickDecision::Auto(AutoPickMeta {
        margin: actual,
        required_margin: required,
        equivalent_count: equivalents,
    })
}

// =====================================================================
// PickResponse — the wire shape of the /pick endpoint. Tagged enum so
// CLI/GUI can branch on `kind` without merging two response shapes.
// =====================================================================

/// Response from the `/pick` endpoint. Tagged enum → JSON like
/// `{"kind": "auto", ...}` or `{"kind": "needs_choice", ...}` so the
/// CLI can branch on the discriminator alone.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PickResponse {
    /// Single confident pick — caller can proceed straight to
    /// `/init` with `url` and skip showing the picker.
    Auto {
        url: String,
        title: String,
        uploader: String,
        score: i32,
        duration_secs: f64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        flags: Vec<String>,
        /// Why the auto-pick fired (margin vs requirement,
        /// equivalence info). `None` for older daemons / trivial
        /// single-candidate picks. Unknown to older clients, which
        /// simply ignore it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<AutoPickMeta>,
    },
    /// Two or more candidates are close in score — caller should
    /// show a picker.
    NeedsChoice {
        candidates: Vec<RankedCandidate>,
        top_score: i32,
        runner_up_score: i32,
        margin: i32,
    },
    /// yt-dlp returned nothing usable; caller should report an error.
    Empty { query: String, message: String },
}

/// Decide whether to auto-pick or surface the picker. Uses
/// [`rank_query`] under the hood. `limit` controls how many ranked
/// candidates the picker case carries; `margin` is the auto-pick
/// threshold (floored by [`STRONG_PICK_MARGIN`] / [`EQUIVALENT_MARGIN`]).
///
/// When the decision is genuinely uncertain, a view-count enrichment
/// pass runs once (one batched yt-dlp call for the top candidates)
/// and the decision is retried on the re-ranked list — popularity
/// between uploads of the same song is the strongest signal the
/// scorer can't see in titles alone.
pub fn pick_best(query: &str, limit: usize, margin: i32) -> Result<PickResponse, String> {
    // Fetch a couple extra candidates so the runner-up comparison has
    // real signal rather than fighting with tail-end noise.
    let fetch = (limit.max(2) + 2).max(5);
    let mut ranked = rank_query(query, fetch)?;

    if ranked.is_empty() {
        return Ok(PickResponse::Empty {
            query: query.to_string(),
            message: format!("no yt-dlp results for {query:?}"),
        });
    }

    // Uncertain? Enrich the top candidates with view counts (one
    // batched yt-dlp call), re-apply the (idempotent) lean, re-rank,
    // then decide on the improved list. Single-candidate lists skip
    // this (nothing to compare).
    if ranked.len() > 1 && auto_pick_decision(&ranked, margin) == AutoPickDecision::NoPick {
        enrich_with_view_counts(&mut ranked);
        apply_view_count_lean(&mut ranked);
        ranked.sort_by(|a, b| b.score.cmp(&a.score));
    }

    if let AutoPickDecision::Auto(meta) = auto_pick_decision(&ranked, margin) {
        let top = &ranked[0];
        return Ok(PickResponse::Auto {
            url: top.base.url.clone(),
            title: top.base.title.clone(),
            uploader: top.base.uploader.clone(),
            score: top.score,
            duration_secs: top.base.duration_secs,
            flags: top.flags.clone(),
            decision: Some(meta),
        });
    }

    // Detach borrows before assembling the NeedsChoice payload.
    let top_score = ranked[0].score;
    let runner_up_score = ranked
        .get(1)
        .map(|rc| rc.score)
        .unwrap_or(top_score);
    let actual_margin = top_score - runner_up_score;
    let trimmed = if limit > 0 && ranked.len() > limit {
        ranked[..limit].to_vec()
    } else {
        ranked
    };
    Ok(PickResponse::NeedsChoice {
        candidates: trimmed,
        top_score,
        runner_up_score,
        margin: actual_margin,
    })
}

// =====================================================================
// Legacy ytsearch1: resolver — route plain /init + init_batch through
// the scoring pipeline.
// =====================================================================

/// Resolve a legacy `ytsearchN:<query>` source (the shape `/init` and
/// `init_batch` hand to the download worker) into a concrete video URL
/// via the scoring pipeline. Only `ytsearch1:` is rewritten — larger N
/// implies an explicit yt-dlp-side pick count we don't own. Returns
/// `None` when the source should be passed through verbatim (explicit
/// URLs, unexpected shapes) or when ranking failed / returned nothing,
/// in which case the legacy behaviour is the best available fallback.
pub fn resolve_search_source(source: &str) -> Option<String> {
    let rest = source.strip_prefix("ytsearch")?;
    let (count_str, query) = rest.split_once(':')?;
    if count_str.parse::<usize>().ok() != Some(1) {
        return None;
    }
    if query.trim().is_empty() {
        return None;
    }
    match pick_best(query, 1, AUTO_PICK_MARGIN) {
        Ok(PickResponse::Auto { url, .. }) => Some(url),
        // Not confident enough to auto-pick: fall back to the top
        // scorer, which is still a large upgrade over raw ytsearch1 —
        // yt-dlp then sees exactly one pre-ranked URL and downloads it.
        Ok(PickResponse::NeedsChoice { candidates, .. }) => {
            candidates.first().map(|c| c.base.url.clone())
        }
        _ => None,
    }
}

// =====================================================================
// Tests.
//
// Pure-function logic tests; the IO-heavy paths (rank_query, pick_best,
// resolve_search_source, enrich_with_view_counts) are covered manually
// via the daemon's `/search/yt/ranked` and `/pick` endpoints, since
// mocking yt-dlp in-process adds more complexity than the integration
// value is worth. The scorer / dedupe / parser / decision paths are
// deterministic and below.
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(title: &str, uploader: &str, dur: f64) -> YtCandidate {
        YtCandidate {
            id: title.to_string(),
            title: title.to_string(),
            uploader: uploader.to_string(),
            duration_secs: dur,
            url: format!("https://example/{title}"),
            thumbnail: None,
            view_count: 0.0,
        }
    }

    fn rc(base: YtCandidate, score: i32, flags: &[&str]) -> RankedCandidate {
        RankedCandidate {
            base,
            score,
            breakdown: Some(ScoreBreakdown {
                total: score,
                ..Default::default()
            }),
            flags: flags.iter().map(|f| f.to_string()).collect(),
        }
    }

    #[test]
    fn parses_artist_title_with_dash_separator() {
        let pq = ParsedQuery::parse("Haftbefehl - RADW").unwrap();
        assert_eq!(pq.artist.as_deref(), Some("Haftbefehl"));
        assert_eq!(pq.title, "RADW");
    }

    #[test]
    fn parses_artist_title_with_colon_separator() {
        let pq = ParsedQuery::parse("Bonez MC: Erde").unwrap();
        assert_eq!(pq.artist.as_deref(), Some("Bonez MC"));
        assert_eq!(pq.title, "Erde");
    }

    #[test]
    fn parses_no_separator_artist_is_none() {
        let pq = ParsedQuery::parse("Just A Title").unwrap();
        assert_eq!(pq.artist, None);
        assert_eq!(pq.title, "Just A Title");
    }

    #[test]
    fn empty_query_returns_none() {
        assert!(ParsedQuery::parse("   ").is_none());
        assert!(ParsedQuery::parse("").is_none());
    }

    #[test]
    fn query_variants_strips_duplicate_official_suffix() {
        let v = query_variants("Haftbefehl - RADW official audio");
        // First entry has the suffix removed; the rest re-add it. The
        // trailing "official audio" duplication is collapsed.
        assert!(v[0] == "Haftbefehl - RADW");
        assert!(v.contains(&"Haftbefehl - RADW official video".to_string()));
        assert_eq!(v.len(), 4);
    }

    #[test]
    fn query_variants_includes_quoted_exact_phrase() {
        let v = query_variants("Haftbefehl - RADW");
        assert_eq!(v[0], "Haftbefehl - RADW");
        assert_eq!(v[1], "Haftbefehl - RADW official video");
        assert_eq!(v[2], "Haftbefehl - RADW official audio");
        assert_eq!(v[3], "\"Haftbefehl - RADW\"");
    }

    #[test]
    fn query_variants_empty_returns_empty() {
        assert!(query_variants("").is_empty());
        assert!(query_variants("   ").is_empty());
    }

    #[test]
    fn official_topic_channel_boosts_hard() {
        let pq = ParsedQuery::parse("Haftbefehl - RADW").unwrap();
        let c = cand("Haftbefehl - RADW", "Haftbefehl - Topic", 200.0);
        let (s, flags) = score_candidate(&pq, &c);
        // Official +100, title match 40+40, graded similarity +50
        // (title has one extra token), artist similarity +30,
        // duration ideal +20.
        assert_eq!(s.total, 100 + 40 + 40 + 50 + 30 + 20);
        assert!(flags.contains(&"official".to_string()));
    }

    #[test]
    fn graded_title_similarity_scales_partial_matches_down() {
        let pq = ParsedQuery::parse("Haftbefehl - RADW").unwrap();
        // Medley decoy: contains the title word but is a different
        // song — must score far below an exact match.
        let (s, _) = score_candidate(&pq, &cand("RADW Megamix", "Random User", 900.0));
        // 1 of 1 query tokens matched → similarity 0.5 → +50, plus the
        // exact-token boost +40, artist mismatch -60, long duration -30.
        assert_eq!(s.title_similarity, 50);
        assert_eq!(s.total, 50 + 40 - 60 - 30);
    }

    #[test]
    fn lyrics_video_gets_lyric_penalty() {
        let pq = ParsedQuery::parse("Haftbefehl - RADW").unwrap();
        let c = cand("Haftbefehl - RADW (Lyric Video)", "Random User", 200.0);
        let (s, flags) = score_candidate(&pq, &c);
        // Title match 40+40, similarity +50, lyrics -40, artist +30,
        // duration +20. The decoration doesn't dilute similarity but
        // the lyrics flag still fires.
        assert_eq!(s.total, 40 + 40 + 50 - 40 + 30 + 20);
        assert_eq!(s.lyrics, LYRICS_PENALTY);
        assert!(flags.contains(&"lyrics".to_string()));
    }

    #[test]
    fn lyrics_penalty_skipped_when_user_typed_lyric() {
        let pq = ParsedQuery::parse("Haftbefehl - RADW lyric video").unwrap();
        let c = cand("Haftbefehl - RADW (Lyric Video)", "Random User", 200.0);
        let (s, _flags) = score_candidate(&pq, &c);
        // Title match: artist tokens only (+40, the query title words
        // live outside the stripped decoration), similarity +25
        // (1 of 4 union tokens), artist +30, duration +20; no lyrics
        // penalty because the user opted in.
        assert_eq!(s.total, 40 + 25 + 30 + 20);
        assert_eq!(s.lyrics, 0);
    }

    #[test]
    fn reaction_video_heavily_penalised() {
        let pq = ParsedQuery::parse("Haftbefehl - RADW").unwrap();
        let c = cand(
            "Haftbefehl - RADW REACTION! First Listen",
            "Reaction Channel",
            300.0,
        );
        let (s, _flags) = score_candidate(&pq, &c);
        // Title match 40+40, similarity +20 (1/5 tokens), reaction -90,
        // artist +30 (full artist in title), duration acceptable +10.
        assert_eq!(s.total, 40 + 40 + 20 - 90 + 30 + 10);
        assert_eq!(s.reaction, REACTION_PENALTY);
    }

    #[test]
    fn artist_mismatch_penalises_wrong_artist() {
        let pq = ParsedQuery::parse("Sido - Narcos").unwrap();
        let c = cand("Narcos", "DOS HERMANOS - Topic", 200.0);
        let (s, _flags) = score_candidate(&pq, &c);
        // Official +100, title exact + similarity +100, duration +20,
        // but the artist doesn't fuzzy-match → -60.
        assert_eq!(s.total, 100 + 140 + 20 - 60);
        assert_eq!(s.artist_match, ARTIST_MISMATCH_PENALTY);
        assert_eq!(s.artist_similarity, 0);
    }

    #[test]
    fn stylized_artist_spelling_still_matches() {
        // "2hermanoz" is the stylized spelling of "Dos Hermanos" — the
        // per-word Jaro-Winkler pass must recognize the relationship the
        // whole-string comparison used to miss.
        let pq = ParsedQuery::parse("2hermanoz - Narcos").unwrap();
        let c = cand("Narcos", "DOS HERMANOS - Topic", 200.0);
        let (s, flags) = score_candidate(&pq, &c);
        assert_eq!(s.artist_match, 0);
        assert!(s.artist_similarity > 0);
        assert!(!flags.contains(&"artist-mismatch".to_string()));
    }

    #[test]
    fn artist_typo_still_matches_via_word_similarity() {
        let pq = ParsedQuery::parse("Haeftbefehl - Song").unwrap();
        let c = cand("Song", "Haftbefehl - Topic", 200.0);
        let (s, flags) = score_candidate(&pq, &c);
        // No mismatch penalty; the fuzzy artist similarity pays a
        // (reduced) bonus instead.
        assert_eq!(s.artist_match, 0);
        assert!(s.artist_similarity > 0);
        assert!(!flags.contains(&"artist-mismatch".to_string()));
    }

    #[test]
    fn nightcore_spam_penalty() {
        let pq = ParsedQuery::parse("Some Song").unwrap();
        let c = cand("Some Song - Nightcore", "Spam Uploader", 180.0);
        let (s, _flags) = score_candidate(&pq, &c);
        // Title match +40, similarity +66, spam -80, no-artist -20,
        // duration +20.
        assert_eq!(s.total, 40 + 66 - 80 - 20 + 20);
        assert_eq!(s.spam, SPAM_PENALTY);
    }

    #[test]
    fn cover_upload_penalised() {
        let pq = ParsedQuery::parse("Drake - Hotline Bling").unwrap();
        let c = cand("Hotline Bling (Cover by SomeArtist)", "SomeArtist", 200.0);
        let (s, flags) = score_candidate(&pq, &c);
        // Title exact + similarity +100, cover -60, artist mismatch
        // -60, duration +20.
        assert_eq!(s.total, 40 + 100 - 60 - 60 + 20);
        assert_eq!(s.cover, COVER_PENALTY);
        assert!(flags.contains(&"cover".to_string()));
    }

    #[test]
    fn cover_penalty_skipped_when_user_typed_cover() {
        let pq = ParsedQuery::parse("Hotline Bling cover").unwrap();
        let c = cand("Hotline Bling (Cover)", "SomeArtist", 200.0);
        let (s, _flags) = score_candidate(&pq, &c);
        assert_eq!(s.cover, 0);
    }

    #[test]
    fn type_beat_penalised() {
        let pq = ParsedQuery::parse("Some Song").unwrap();
        let c = cand("Some Song FREE Type Beat", "Producer", 200.0);
        let (s, flags) = score_candidate(&pq, &c);
        // Title match +40, similarity +40 (2/5 tokens), type beat -50,
        // no-artist -20, duration +20.
        assert_eq!(s.total, 40 + 40 - 50 - 20 + 20);
        assert!(flags.contains(&"type-beat".to_string()));
    }

    #[test]
    fn compilation_penalised() {
        let pq = ParsedQuery::parse("Sample Artist - Song").unwrap();
        let c = cand(
            "DJ Mix 2024 Mega Mix Best Of",
            "Mix Channel",
            3600.0,
        );
        let (s, flags) = score_candidate(&pq, &c);
        // No title match, artist mismatch -60, compilation -45,
        // long duration -30.
        assert_eq!(s.total, -60 - 45 - 30);
        assert!(flags.contains(&"compilation".to_string()));
    }

    #[test]
    fn word_flags_survive_word_boundaries_but_not_substrings() {
        let pq = ParsedQuery::parse("Some Song").unwrap();
        // "Alive" contains "live" and "Discover" contains "cover" —
        // neither must trigger a penalty.
        let (s, flags) = score_candidate(&pq, &cand("Alive And Discover Mix", "User", 200.0));
        assert_eq!(s.live, 0);
        assert_eq!(s.cover, 0);
        assert!(!flags.contains(&"live".to_string()));
        assert!(!flags.contains(&"cover".to_string()));
        // But an actual word hit still fires.
        let (s2, flags2) = score_candidate(&pq, &cand("Some Song Live At Wembley", "User", 400.0));
        assert_eq!(s2.live, LIVE_PENALTY);
        assert!(flags2.contains(&"live".to_string()));
    }

    #[test]
    fn very_long_audiobook_penalised() {
        let pq = ParsedQuery::parse("Some Song").unwrap();
        // Title deliberately omits "song" so title_match stays 0.
        let c = cand("Some Long Audiobook Podcast", "User", 7200.0);
        let (s, _flags) = score_candidate(&pq, &c);
        // similarity +20, no-artist -20, long -30.
        assert_eq!(s.total, 20 - 20 - 30);
    }

    #[test]
    fn fan_upload_penalty_applied_when_official_present() {
        let mut ranked = vec![
            rc(cand("Haftbefehl - RADW", "HaftbefehlVEVO", 200.0), 160, &["official"]),
            rc(cand("Haftbefehl - RADW", "Random User", 200.0), 60, &[]),
        ];
        apply_fan_upload_penalty(&mut ranked);
        // Fan upload: 60 - 10 = 50, with a "fan-upload" flag attached.
        assert_eq!(ranked[1].score, 50);
        assert!(ranked[1].flags.iter().any(|f| f == "fan-upload"));
        // Official: untouched.
        assert_eq!(ranked[0].score, 160);
        assert!(!ranked[0].flags.iter().any(|f| f == "fan-upload"));
    }

    #[test]
    fn fan_upload_penalty_skipped_when_no_official() {
        let mut ranked = vec![
            rc(cand("Some Song", "Random User", 200.0), 60, &[]),
            rc(cand("Another Song", "Other User", 200.0), 60, &[]),
        ];
        apply_fan_upload_penalty(&mut ranked);
        assert_eq!(ranked[0].score, 60);
        assert_eq!(ranked[1].score, 60);
    }

    #[test]
    fn view_count_lean_promotes_dominant_upload_of_same_song() {
        let mut a = rc(cand("Same Song", "Channel A", 200.0), 60, &[]);
        a.base.view_count = 6_000_000.0;
        let mut b = rc(cand("Same Song", "Channel B", 200.0), 55, &[]);
        b.base.view_count = 30_000.0;
        let mut ranked = vec![b, a];
        apply_view_count_lean(&mut ranked);
        // 6M views towers over the 30k rival → +6, flipping the order.
        assert_eq!(ranked[1].score, 66);
        assert_eq!(ranked[1].breakdown.as_ref().unwrap().view_count, 6);
        assert_eq!(ranked[0].score, 55);
    }

    #[test]
    fn view_count_lean_never_compares_different_songs() {
        let mut a = rc(cand("Song One", "Channel A", 200.0), 60, &[]);
        a.base.view_count = 9_000_000.0;
        let b = rc(cand("Song Two", "Channel B", 200.0), 55, &[]);
        let mut ranked = vec![b, a];
        apply_view_count_lean(&mut ranked);
        assert_eq!(ranked[1].score, 60);
        assert_eq!(ranked[0].score, 55);
    }

    #[test]
    fn auto_pick_rejects_vetoed_top_candidate_despite_margin() {
        let ranked = vec![
            rc(cand("Song (Lyric Video)", "Channel A", 200.0), 400, &["lyrics"]),
            rc(cand("Other Song", "Channel B", 200.0), 100, &[]),
        ];
        assert_eq!(auto_pick_decision(&ranked, AUTO_PICK_MARGIN), AutoPickDecision::NoPick);
    }

    #[test]
    fn auto_pick_rejects_weak_leader_below_score_floor() {
        let ranked = vec![rc(cand("Obscure Song", "Tiny Channel", 200.0), 40, &[])];
        assert_eq!(auto_pick_decision(&ranked, AUTO_PICK_MARGIN), AutoPickDecision::NoPick);
    }

    #[test]
    fn auto_pick_rejects_fan_upload_leader() {
        let ranked = vec![
            rc(cand("Song", "Fan Uploads", 200.0), 200, &["fan-upload"]),
            rc(cand("Other Thing", "Channel B", 200.0), 80, &[]),
        ];
        assert_eq!(auto_pick_decision(&ranked, AUTO_PICK_MARGIN), AutoPickDecision::NoPick);
    }

    #[test]
    fn auto_pick_needs_margin_for_different_songs() {
        let ranked = vec![
            rc(cand("Song A", "Channel A", 200.0), 100, &[]),
            rc(cand("Song B", "Channel B", 200.0), 85, &[]),
        ];
        // 100-85=15 < max(30, 25) → picker.
        assert_eq!(auto_pick_decision(&ranked, AUTO_PICK_MARGIN), AutoPickDecision::NoPick);
    }

    #[test]
    fn auto_pick_confident_for_clear_leader() {
        let ranked = vec![
            rc(cand("Song A", "Artist - Topic", 200.0), 240, &["official"]),
            rc(cand("Song B", "Channel B", 200.0), 80, &[]),
        ];
        let d = auto_pick_decision(&ranked, AUTO_PICK_MARGIN);
        match d {
            AutoPickDecision::Auto(meta) => {
                assert_eq!(meta.margin, 160);
                assert_eq!(meta.required_margin, AUTO_PICK_MARGIN);
                assert_eq!(meta.equivalent_count, 0);
            }
            AutoPickDecision::NoPick => panic!("expected confident auto-pick"),
        }
    }

    #[test]
    fn auto_pick_relaxes_margin_for_title_equivalent_runner_up() {
        let ranked = vec![
            rc(cand("Song A", "Artist - Topic", 200.0), 160, &["official"]),
            rc(cand("Song A (Official Audio)", "Fan Channel", 200.0), 145, &[]),
        ];
        // 160-145=15 < 30 normally — but the runner-up is the same song
        // (identical title tokens after decoration stripping), so the
        // equivalence floor applies.
        let d = auto_pick_decision(&ranked, AUTO_PICK_MARGIN);
        match d {
            AutoPickDecision::Auto(meta) => {
                assert_eq!(meta.required_margin, EQUIVALENT_MARGIN);
                assert_eq!(meta.equivalent_count, 1);
            }
            AutoPickDecision::NoPick => panic!("expected equivalence-relaxed auto-pick"),
        }
    }

    #[test]
    fn auto_pick_single_candidate_still_needs_score_floor() {
        let strong = vec![rc(cand("Song A", "Artist - Topic", 200.0), 240, &["official"])];
        assert!(matches!(
            auto_pick_decision(&strong, AUTO_PICK_MARGIN),
            AutoPickDecision::Auto(_)
        ));
        let weak = vec![rc(cand("Song A", "Artist - Topic", 200.0), 50, &["official"])];
        assert_eq!(auto_pick_decision(&weak, AUTO_PICK_MARGIN), AutoPickDecision::NoPick);
    }

    #[test]
    fn dedupe_keeps_distinct_videos() {
        let mut v = vec![
            cand("Haftbefehl - RADW", "VEVO", 180.0),
            cand("Haftbefehl - RADW", "VEVO", 180.0), // exact dup id-less: dedups by title+uploader
            cand("Bonez MC - Spray", "Bonez MC - Topic", 200.0),
        ];
        v = dedupe_candidates(v);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn dedupe_id_dedupes_with_same_id() {
        let mut a = cand("Some Song", "Topic Channel", 200.0);
        a.id = "abc123".to_string();
        let mut b = cand("Some Song (Official)", "Topic Channel", 200.0);
        b.id = "abc123".to_string();
        let mut c = cand("Other Song", "Topic Channel", 200.0);
        c.id = "zzz999".to_string();
        let v = dedupe_candidates(vec![a, b, c]);
        // a and b share id → only one survives; c is distinct.
        assert_eq!(v.len(), 2);
        assert!(v.iter().any(|c| c.id == "zzz999"));
    }
}
