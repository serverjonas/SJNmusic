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
//! 1. [`query_variants`] expands `<artist> <title>` into 3 ytsearch
//!    queries: raw, "official video", "official audio". Each variant
//!    biases toward the channel pattern YouTube's official uploads
//!    follow ("Artist - Topic", "VEVO", "(Official)").
//! 2. [`rank_query`] runs every variant **in parallel** (each spawns
//!    its own yt-dlp subprocess), merges, dedupes, scores, and returns
//!    the top-`limit` sorted by score.
//! 3. [`pick_best`] adds the auto-pick decision on top: when the top
//!    score beats the runner-up by [`AUTO_PICK_MARGIN`] points, the
//!    caller can proceed straight to download without surfacing a
//!    picker.
//!
//! Scoring constants live next to the scorer so the spec in the repo
//! docs and the code can never drift apart — adjust the numbers here
//! when refining the heuristics.

use std::collections::HashSet;
use std::thread;

use log::{debug, warn};
use serde::{Deserialize, Serialize};
use strsim::jaro_winkler;

use crate::state::{DaemonState, YtCandidate};

// =====================================================================
// Scoring constants — tuned to the spec in /docs/INTERFACE.md (search
// ranking heuristics). Pull these into your editor and read them next
// to `score_candidate` when adjusting values.
// =====================================================================

/// Official-artist-channel marker. Bigger than every other positive
/// signal combined so a real official upload will always outrank even
/// the most popular fan upload.
pub const OFFICIAL_CHANNEL_BOOST: i32 = 100;
/// "Official Video" / "Official Music Video" / "Official Audio" phrase
/// in the title. Stacks with `OFFICIAL_CHANNEL_BOOST`.
pub const OFFICIAL_PHRASE_BOOST: i32 = 30;
/// All (non-trivial) tokens from the parsed query appear in the
/// candidate title. Stops "Best Of" compilations and audio-twin
/// decoys from outscoring the real track.
pub const TITLE_INCLUDES_QUERY_BOOST: i32 = 40;
/// Duration sits in the 2–4 minute "ideal" band.
pub const DURATION_IDEAL_BOOST: i32 = 20;
/// Duration sits in the 1.5–6 minute "acceptable" band (excluding
/// ideal).
pub const DURATION_ACCEPTABLE_BOOST: i32 = 10;
/// Duration is over 8 minutes (audiobooks, full live sets).
pub const DURATION_LONG_PENALTY: i32 = -30;
/// Duration is suspiciously short (< 1 minute).
pub const DURATION_SHORT_PENALTY: i32 = -15;
/// yt-dlp didn't report a duration at all (livestream, region-locked).
pub const DURATION_UNKNOWN_PENALTY: i32 = -5;

/// Tokens like "Lyrics", "Lyric Video" in the title.
pub const LYRICS_PENALTY: i32 = -40;
/// Reaction channels (Reaction / Reacts / First Listen).
pub const REACTION_PENALTY: i32 = -80;
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

/// Below this Jaro-Winkler similarity the artist match is treated as
/// "missing" → `ARTIST_MISMATCH_PENALTY`.
pub const ARTIST_MATCH_REJECT: f64 = 0.65;

// Duration thresholds (seconds). Spec bands:
//   ideal      = 2–4 min  → +20
//   acceptable = 2–6 min, excluding ideal → +10
//   long       = > 8 min  → penalty
pub const IDEAL_SONG_SECS: f64 = 120.0;
pub const IDEAL_SONG_SECS_MAX: f64 = 240.0;
pub const MIN_SONG_SECS: f64 = 120.0;
pub const MAX_SONG_SECS: f64 = 360.0;
pub const LONG_SONG_SECS: f64 = 480.0;
pub const VERY_SHORT_SECS: f64 = 60.0;

/// Margin (in score points) by which the top candidate must beat the
/// runner-up before `pick_best` decides to auto-select. Matches the
/// "best by ≥30" example in the spec.
pub const AUTO_PICK_MARGIN: i32 = 30;

/// Extra candidates fetched per variant beyond what the caller asked
/// for, to leave headroom after dedupe + scoring.
pub const PER_VARIANT_LIMIT_BUMP: usize = 2;
/// Lower bound on per-variant ytsearch limit even when the caller asks
/// for fewer.
pub const MIN_PER_VARIANT_LIMIT: usize = 3;

// =====================================================================
// ParsedQuery — split `<artist> <title>` into proper halves so we can
// do artist-matching without trusting only the title.
// =====================================================================

/// Parsed-out (artist, title, raw) for ranking and artist-matching.
/// `artist` is `None` when the query can't be split cleanly; the
/// scorer just skips the artist mismatch penalty in that case rather
/// than guessing.
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
// Query variants — expand a query into 3 ytsearch calls that together
// surface both the user's own-channel uploads and YouTube's
// auto-generated "Topic" channels.
// =====================================================================

/// Expand `<artist> <title>` into a small batch of ytsearch-friendly
/// queries. The first element is always the raw query (so dedup-by-id
/// can collapse matches across variants). Subsequent entries bias
/// toward the "tagged Artist's <Title> [Official…]" pattern that
/// official uploads follow.
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
/// are surfaced as badges.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ScoreBreakdown {
    pub total: i32,
    #[serde(default)]
    pub official_channel: i32,
    #[serde(default)]
    pub official_phrase: i32,
    #[serde(default)]
    pub title_match: i32,
    #[serde(default)]
    pub artist_match: i32,
    #[serde(default)]
    pub duration: i32,
    #[serde(default)]
    pub fan_upload: i32,
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
    /// `instrumental`, `karaoke`, `long`, `short`,
    /// `artist-mismatch`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
}

// =====================================================================
// score_candidate — the heart of the ranking. Returns (breakdown, flags).
// =====================================================================

/// Score a single yt-dlp candidate against the parsed query.
/// Mutates everyone-but-flags of `ScoreBreakdown::total` at the end.
pub fn score_candidate(pq: &ParsedQuery, c: &YtCandidate) -> (ScoreBreakdown, Vec<String>) {
    let mut s = ScoreBreakdown::default();
    let mut flags: Vec<String> = Vec::new();

    let raw_lower = pq.raw.to_ascii_lowercase();
    let title_lower = c.title.to_ascii_lowercase();
    let uploader_lower = c.uploader.to_ascii_lowercase();

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

    // Title contains ALL non-trivial tokens from the parsed query.
    let mut required: Vec<String> = pq
        .title
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .map(|w| w.to_ascii_lowercase())
        .collect();
    if let Some(a) = &pq.artist {
        for w in a.split_whitespace().filter(|w| w.len() > 2) {
            required.push(w.to_ascii_lowercase());
        }
    }
    if !required.is_empty() && required.iter().all(|t| title_lower.contains(t)) {
        s.title_match = TITLE_INCLUDES_QUERY_BOOST;
    }

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
    if let Some(artist) = &pq.artist {
        let artist_lower = artist.to_ascii_lowercase();
        if !artist_lower.is_empty() {
            let sim_uploader = jaro_winkler(&artist_lower, &uploader_lower);
            let sim_title = jaro_winkler(&artist_lower, &title_lower);
            let best = sim_uploader.max(sim_title);
            if best < ARTIST_MATCH_REJECT {
                s.artist_match = ARTIST_MISMATCH_PENALTY;
                flags.push("artist-mismatch".to_string());
            }
        }
    }

    // Duration scoring.
    let dur = c.duration_secs;
    if dur <= 0.0 {
        s.duration = DURATION_UNKNOWN_PENALTY;
    } else if dur >= IDEAL_SONG_SECS && dur <= IDEAL_SONG_SECS_MAX {
        s.duration = DURATION_IDEAL_BOOST;
    } else if dur >= MIN_SONG_SECS && dur <= MAX_SONG_SECS {
        s.duration = DURATION_ACCEPTABLE_BOOST;
    } else if dur > LONG_SONG_SECS {
        s.duration = DURATION_LONG_PENALTY;
        flags.push("long".to_string());
    } else if dur < VERY_SHORT_SECS {
        s.duration = DURATION_SHORT_PENALTY;
        flags.push("short".to_string());
    }

    // ---- Negative signals -----------------------------------------
    // Each has a corresponding "user opted in" guard so a search for
    // "song remix" doesn't penalise the remix itself.

    let user_wants = |kw: &str| raw_lower.contains(kw);
    let title_has = |terms: &[&str]| terms.iter().any(|t| title_lower.contains(t));

    if !user_wants("lyric") && title_has(&["lyric", "lyrics"]) {
        s.lyrics = LYRICS_PENALTY;
        flags.push("lyrics".to_string());
    }
    if !user_wants("reaction") && title_has(&["reaction", "reacts", "first listen"]) {
        s.reaction = REACTION_PENALTY;
        flags.push("reaction".to_string());
    }
    if !user_wants("remix") && title_has(&["remix", "edit", "mashup"]) {
        s.remix = REMIX_PENALTY;
        flags.push("remix".to_string());
    }
    if !user_wants("live") && title_has(&["live", "concert"]) {
        s.live = LIVE_PENALTY;
        flags.push("live".to_string());
    }
    if title_has(&["nightcore"]) {
        s.spam += SPAM_PENALTY;
        flags.push("nightcore".to_string());
    }
    if title_has(&["bass boosted"]) {
        s.spam += SPAM_PENALTY;
        flags.push("bass-boosted".to_string());
    }
    if title_has(&["slowed"]) {
        s.spam += SPAM_PENALTY;
        flags.push("slowed".to_string());
    }
    if title_has(&["reverb"]) {
        s.spam += SPAM_PENALTY;
        flags.push("reverb".to_string());
    }
    if !user_wants("instrumental") && title_has(&["instrumental"]) {
        s.instrumental = INSTRUMENTAL_PENALTY;
        flags.push("instrumental".to_string());
    }
    if title_has(&["karaoke"]) {
        s.karaoke = KARAOKE_PENALTY;
        flags.push("karaoke".to_string());
    }

    // `fan_upload` is updated by `apply_fan_upload_penalty` after
    // scoring; deliberately omitted from the initial total sum so
    // the post-pass owns the bookkeeping and syncs `rc.score = b.total`.
    s.total = s.official_channel
        + s.official_phrase
        + s.title_match
        + s.artist_match
        + s.duration
        + s.lyrics
        + s.reaction
        + s.remix
        + s.live
        + s.spam
        + s.instrumental
        + s.karaoke;
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

/// Apply `FAN_UPLOAD_PENALTY` to every candidate that is NOT marked
/// as an official channel upload, when the result set contains at
/// least one such official candidate. No-op otherwise.
pub fn apply_fan_upload_penalty(ranked: &mut [RankedCandidate]) {
    let has_official = ranked.iter().any(|rc| {
        rc.flags
            .iter()
            .any(|f| f == "official" || f == "official-upload")
    });
    if !has_official {
        return;
    }
    for rc in ranked.iter_mut() {
        let is_official = rc
            .flags
            .iter()
            .any(|f| f == "official" || f == "official-upload");
        if is_official {
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
    // 3x latency hit of fetching variants serially.
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

    // Fan-upload penalty pass: small -10 to non-official candidates
    // when at least one official upload exists. Spec signal: "Fan
    // uploads: Small penalty unless no official upload exists."
    apply_fan_upload_penalty(&mut ranked);

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
/// threshold (defaults to [`AUTO_PICK_MARGIN`]).
pub fn pick_best(query: &str, limit: usize, margin: i32) -> Result<PickResponse, String> {
    // Fetch a couple extra candidates so the runner-up comparison has
    // real signal rather than fighting with tail-end noise.
    let fetch = (limit.max(2) + 2).max(5);
    let ranked = rank_query(query, fetch)?;

    if ranked.is_empty() {
        return Ok(PickResponse::Empty {
            query: query.to_string(),
            message: format!("no yt-dlp results for {query:?}"),
        });
    }

    let top = &ranked[0];
    if ranked.len() == 1 {
        return Ok(PickResponse::Auto {
            url: top.base.url.clone(),
            title: top.base.title.clone(),
            uploader: top.base.uploader.clone(),
            score: top.score,
            duration_secs: top.base.duration_secs,
            flags: top.flags.clone(),
        });
    }

    let runner_up = ranked[1].score;
    let actual_margin = top.score - runner_up;
    if actual_margin >= margin {
        return Ok(PickResponse::Auto {
            url: top.base.url.clone(),
            title: top.base.title.clone(),
            uploader: top.base.uploader.clone(),
            score: top.score,
            duration_secs: top.base.duration_secs,
            flags: top.flags.clone(),
        });
    }

    // Detach the borrow on `ranked` before moving it into `trimmed` —
    // otherwise Rust insists the auto branches above have already
    // borrowed the very same slot we're trying to move into the
    // NeedsChoice payload.
    let top_score = top.score;
    let trimmed = if limit > 0 && ranked.len() > limit {
        ranked[..limit].to_vec()
    } else {
        ranked
    };
    Ok(PickResponse::NeedsChoice {
        candidates: trimmed,
        top_score,
        runner_up_score: runner_up,
        margin: actual_margin,
    })
}

// =====================================================================
// Tests.
//
// Pure-function logic tests; the IO-heavy paths (rank_query, pick_best)
// are covered manually via the daemon's `/search/yt/ranked` and
// `/pick` endpoints, since mocking yt-dlp in-process adds more complexity
// than the integration value is worth. The scorer / dedupe / parser
// paths are deterministic and below.
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
        assert_eq!(v.len(), 3);
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
        // Official +100, title match +40, duration ideal +20 = 160.
        assert_eq!(s.total, 100 + 40 + 20);
        assert!(flags.contains(&"official".to_string()));
    }

    #[test]
    fn lyrics_video_gets_lyric_penalty() {
        let pq = ParsedQuery::parse("Haftbefehl - RADW").unwrap();
        let c = cand("Haftbefehl - RADW (Lyric Video)", "Random User", 200.0);
        let (s, _flags) = score_candidate(&pq, &c);
        // No official, title match +40, duration +20, lyrics -40 = 20.
        assert_eq!(s.total, 40 + 20 - 40);
        assert_eq!(s.lyrics, LYRICS_PENALTY);
    }

    #[test]
    fn lyrics_penalty_skipped_when_user_typed_lyric() {
        let pq = ParsedQuery::parse("Haftbefehl - RADW lyric video").unwrap();
        let c = cand("Haftbefehl - RADW (Lyric Video)", "Random User", 200.0);
        let (s, _flags) = score_candidate(&pq, &c);
        // No official, title match +40, duration +20 = 60.
        assert_eq!(s.total, 40 + 20);
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
        // Title contains all terms, duration acceptable, but reaction.
        assert_eq!(s.total, 40 + 10 - 80);
        assert_eq!(s.reaction, REACTION_PENALTY);
    }

    #[test]
    fn artist_mismatch_penalises_dos_hermanos_for_2hermanoz_query() {
        let pq = ParsedQuery::parse("2hermanoz - Narcos").unwrap();
        let c = cand("Narcos", "DOS HERMANOS - Topic", 200.0);
        let (s, _flags) = score_candidate(&pq, &c);
        // Official +100, duration +20, but artist doesn't match → -60.
        assert_eq!(s.total, 100 + 20 - 60);
        assert_eq!(s.artist_match, ARTIST_MISMATCH_PENALTY);
    }

    #[test]
    fn nightcore_spam_penalty() {
        let pq = ParsedQuery::parse("Some Song").unwrap();
        let c = cand("Some Song - Nightcore", "Spam Uploader", 180.0);
        let (s, _flags) = score_candidate(&pq, &c);
        // Title match +40, duration ideal +20, spam -80 = -20.
        assert_eq!(s.total, 40 + 20 - 80);
        assert_eq!(s.spam, SPAM_PENALTY);
    }

    #[test]
    fn very_long_audiobook_penalised() {
        let pq = ParsedQuery::parse("Some Song").unwrap();
        // Title deliberately omits "song" so title_match stays 0; the
        // only signal active is the long-duration penalty.
        let c = cand("Some Long Audiobook Podcast", "User", 7200.0);
        let (s, _flags) = score_candidate(&pq, &c);
        assert_eq!(s.total, DURATION_LONG_PENALTY);
    }

    #[test]
    fn fan_upload_penalty_applied_when_official_present() {
        let mut ranked = vec![
            RankedCandidate {
                base: cand("Haftbefehl - RADW", "HaftbefehlVEVO", 200.0),
                score: 160,
                breakdown: None,
                flags: vec!["official".into()],
            },
            RankedCandidate {
                base: cand("Haftbefehl - RADW", "Random User", 200.0),
                score: 60,
                breakdown: None,
                flags: vec![],
            },
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
            RankedCandidate {
                base: cand("Some Song", "Random User", 200.0),
                score: 60,
                breakdown: None,
                flags: vec![],
            },
            RankedCandidate {
                base: cand("Another Song", "Other User", 200.0),
                score: 60,
                breakdown: None,
                flags: vec![],
            },
        ];
        apply_fan_upload_penalty(&mut ranked);
        assert_eq!(ranked[0].score, 60);
        assert_eq!(ranked[1].score, 60);
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
