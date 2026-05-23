//! Minutes Madness — March Madness for corporate buzzwords.
//!
//! Seed 16 buzzwords into a single-elimination bracket before an all-hands
//! call. As the call is transcribed (live via `minutes live` or from a saved
//! transcript), each mention bumps a term's tally. When the game is scored,
//! every matchup is resolved by total mention count and a champion is crowned.
//! Players fill out a full bracket beforehand and earn round-weighted points
//! for each correctly predicted matchup winner.
//!
//! This module is pure logic + JSON persistence — no audio dependencies — so
//! it can be developed and tested against sample transcripts without a live
//! call. The CLI (`minutes madness`) is a thin shell over these functions.

use crate::config::Config;
use crate::error::MadnessError;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Number of seeded terms in a bracket.
pub const BRACKET_SIZE: usize = 16;

/// Total matchups in a 16-slot single-elimination bracket (8 + 4 + 2 + 1).
pub const MATCHUP_COUNT: u8 = 15;

/// Standard single-elimination seed order for a 16-slot bracket. Consecutive
/// pairs form the round-1 matchups, arranged so the 1 and 2 seeds can only
/// meet in the final.
pub const SEED_ORDER: [u8; 16] = [1, 16, 8, 9, 5, 12, 4, 13, 6, 11, 3, 14, 7, 10, 2, 15];

/// Points awarded for a correct pick in each round (round 1 → final).
pub const ROUND_POINTS: [u32; 4] = [1, 2, 4, 8];

/// A single seeded buzzword. `aliases` are alternate spellings the
/// transcriber might emit (e.g. "AI powered" for "AI-powered").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Term {
    /// Bracket seed, 1..=16.
    pub seed: u8,
    /// Canonical display label.
    pub label: String,
    /// Alternate spellings counted as the same term.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// A player's full-bracket prediction: matchup id (1..=15) → predicted
/// winning seed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Player {
    /// Player name (unique within a game).
    pub name: String,
    /// Predicted winning seed for each matchup id.
    pub picks: BTreeMap<u8, u8>,
    /// Optional tiebreaker: guess at total buzzword mentions across the call.
    #[serde(default)]
    pub tiebreaker_total: Option<u32>,
}

/// A resolved matchup in the scored bracket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Matchup {
    /// Matchup id, 1..=15 (round-major order).
    pub id: u8,
    /// Round number, 1..=4.
    pub round: u8,
    /// Seed entering as side A.
    pub a: u8,
    /// Seed entering as side B.
    pub b: u8,
    /// Seed that advanced.
    pub winner: u8,
}

/// A player's final score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlayerScore {
    /// Player name.
    pub name: String,
    /// Total round-weighted points.
    pub points: u32,
    /// Count of correctly predicted matchups.
    pub correct: u32,
    /// Correct picks per round (index 0 = round 1 / Round of 16 … 3 = Final).
    #[serde(default)]
    pub round_correct: [u32; 4],
    /// Round-weighted points earned per round.
    #[serde(default)]
    pub round_points: [u32; 4],
    /// The seed this player picked to win it all (their Final pick).
    #[serde(default)]
    pub predicted_champion: u8,
    /// Whether their predicted champion actually won the bracket.
    #[serde(default)]
    pub champion_correct: bool,
    /// The player's tiebreaker guess, if any.
    pub tiebreaker_total: Option<u32>,
    /// Absolute distance between the guess and actual total mentions.
    pub tiebreaker_delta: Option<u32>,
}

/// The computed outcome of a scored bracket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Results {
    /// Mentions per seed (all 16 seeds present, 0 if never said).
    pub counts: BTreeMap<u8, u32>,
    /// All 15 resolved matchups in round-major order.
    pub matchups: Vec<Matchup>,
    /// Champion seed.
    pub champion: u8,
    /// Total mentions across all 16 terms.
    pub total_mentions: u32,
    /// Player standings, highest score first.
    pub standings: Vec<PlayerScore>,
}

/// A saved Minutes Madness game.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BracketGame {
    /// Filesystem-safe identifier.
    pub slug: String,
    /// Human-readable title.
    pub title: String,
    /// Creation timestamp.
    pub created_at: DateTime<Local>,
    /// The 16 seeded terms.
    pub terms: Vec<Term>,
    /// Registered players and their picks.
    #[serde(default)]
    pub players: Vec<Player>,
    /// Scoring results, populated once scored.
    #[serde(default)]
    pub results: Option<Results>,
}

impl BracketGame {
    /// Create a new game from an ordered list of terms (seed = position + 1).
    /// Returns an error unless exactly 16 terms are supplied.
    pub fn new(title: &str, slug: Option<&str>, labels: Vec<Term>) -> Result<Self, MadnessError> {
        if labels.len() != BRACKET_SIZE {
            return Err(MadnessError::InvalidTerms(format!(
                "expected {BRACKET_SIZE} terms, got {}",
                labels.len()
            )));
        }
        let slug = match slug {
            Some(s) => slugify(s),
            None => slugify(title),
        };
        if slug.is_empty() {
            return Err(MadnessError::InvalidTerms(
                "could not derive a slug; pass --slug".into(),
            ));
        }
        Ok(Self {
            slug,
            title: title.to_string(),
            created_at: Local::now(),
            terms: labels,
            players: Vec::new(),
            results: None,
        })
    }

    /// Look up a term by seed.
    pub fn term(&self, seed: u8) -> Option<&Term> {
        self.terms.iter().find(|t| t.seed == seed)
    }

    /// Display label for a seed, falling back to `#<seed>` if unknown.
    pub fn label(&self, seed: u8) -> String {
        self.term(seed)
            .map(|t| t.label.clone())
            .unwrap_or_else(|| format!("#{seed}"))
    }

    /// Add or replace a player's picks. Validates that every matchup is
    /// predicted and that each pick is a legal participant of its matchup
    /// given the player's own earlier picks.
    pub fn set_player(&mut self, player: Player) -> Result<(), MadnessError> {
        validate_picks(&player.picks)?;
        self.players.retain(|p| p.name != player.name);
        self.players.push(player);
        Ok(())
    }

    /// Score the bracket against transcript text, populating `results`.
    pub fn score(&mut self, transcript: &str) -> &Results {
        let counts = count_mentions(transcript, &self.terms);
        let matchups = resolve_bracket(&counts);
        let champion = matchups
            .last()
            .map(|m| m.winner)
            .expect("bracket always has a final matchup");
        let total_mentions: u32 = counts.values().sum();
        let standings = score_players(&self.players, &matchups, total_mentions);
        self.results = Some(Results {
            counts,
            matchups,
            champion,
            total_mentions,
            standings,
        });
        self.results.as_ref().unwrap()
    }

    /// Remove a player by name. Returns true if a player was removed, and
    /// re-ranks the remaining players if the bracket was already scored.
    pub fn remove_player(&mut self, name: &str) -> bool {
        let before = self.players.len();
        self.players.retain(|p| p.name != name);
        let removed = self.players.len() != before;
        if removed {
            self.recompute_standings();
        }
        removed
    }

    /// Recompute player standings from already-stored results (e.g. after a
    /// player is added post-scoring), without needing the transcript again.
    /// No-op if the bracket has not been scored yet.
    pub fn recompute_standings(&mut self) {
        let Some((matchups, total)) = self
            .results
            .as_ref()
            .map(|r| (r.matchups.clone(), r.total_mentions))
        else {
            return;
        };
        let standings = score_players(&self.players, &matchups, total);
        if let Some(res) = self.results.as_mut() {
            res.standings = standings;
        }
    }
}

/// Lowercase and split text into alphanumeric tokens. Any non-alphanumeric
/// character (hyphens, punctuation, slashes) becomes a token boundary, so
/// "AI-powered" and "AI powered" both tokenize to `["ai", "powered"]`.
pub fn normalize_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Count non-overlapping mentions of a term in `haystack`. `needles` are the
/// term's distinct normalized variants, sorted longest-first so the longest
/// matching variant wins at each position and a consumed span is not
/// re-counted by another variant.
fn count_variants(haystack: &[String], needles: &[Vec<String>]) -> u32 {
    let mut count = 0;
    let mut i = 0;
    while i < haystack.len() {
        let mut step = 0;
        for needle in needles {
            let len = needle.len();
            if len > 0 && i + len <= haystack.len() && haystack[i..i + len] == needle[..] {
                step = len;
                break;
            }
        }
        if step > 0 {
            count += 1;
            i += step;
        } else {
            i += 1;
        }
    }
    count
}

/// Count mentions of every term in `text`, keyed by seed. A term's label and
/// aliases are deduplicated after normalization (so "AI-powered" and "AI
/// powered" don't each count the same phrase). All seeds present in `terms`
/// appear in the result (0 if never mentioned).
pub fn count_mentions(text: &str, terms: &[Term]) -> BTreeMap<u8, u32> {
    let haystack = normalize_tokens(text);
    let mut counts = BTreeMap::new();
    for term in terms {
        let mut needles: Vec<Vec<String>> = Vec::new();
        let mut variants = vec![term.label.as_str()];
        variants.extend(term.aliases.iter().map(|a| a.as_str()));
        for variant in variants {
            let needle = normalize_tokens(variant);
            if !needle.is_empty() && !needles.contains(&needle) {
                needles.push(needle);
            }
        }
        needles.sort_by_key(|n| std::cmp::Reverse(n.len()));
        counts.insert(term.seed, count_variants(&haystack, &needles));
    }
    counts
}

/// Decide which of two seeds advances: higher mention count wins; on a tie the
/// better (lower-numbered) seed advances.
fn advance(a: u8, b: u8, counts: &BTreeMap<u8, u32>) -> u8 {
    let ca = counts.get(&a).copied().unwrap_or(0);
    let cb = counts.get(&b).copied().unwrap_or(0);
    match ca.cmp(&cb) {
        std::cmp::Ordering::Greater => a,
        std::cmp::Ordering::Less => b,
        std::cmp::Ordering::Equal => a.min(b),
    }
}

/// Resolve the full 16-slot bracket from mention counts. Returns all 15
/// matchups in round-major order (round 1 ids 1-8, …, final id 15).
pub fn resolve_bracket(counts: &BTreeMap<u8, u32>) -> Vec<Matchup> {
    let mut slots: Vec<u8> = SEED_ORDER.to_vec();
    let mut matchups = Vec::with_capacity(MATCHUP_COUNT as usize);
    let mut id: u8 = 1;
    let mut round: u8 = 1;
    while slots.len() > 1 {
        let mut next = Vec::with_capacity(slots.len() / 2);
        let mut i = 0;
        while i < slots.len() {
            let a = slots[i];
            let b = slots[i + 1];
            let winner = advance(a, b, counts);
            matchups.push(Matchup {
                id,
                round,
                a,
                b,
                winner,
            });
            next.push(winner);
            id += 1;
            i += 2;
        }
        slots = next;
        round += 1;
    }
    matchups
}

/// The two child matchup ids feeding matchup `m` (only valid for m > 8).
fn feeder_matchups(m: u8) -> (u8, u8) {
    (2 * (m - 8) - 1, 2 * (m - 8))
}

/// The two seeds that contest matchup `m` given a player's picks. For round-1
/// matchups this is fixed by the seed order; for later rounds it is the
/// player's predicted winners of the two feeding matchups. Used both to
/// validate picks and to drive interactive pick entry.
pub fn matchup_participants(picks: &BTreeMap<u8, u8>, m: u8) -> Result<(u8, u8), MadnessError> {
    if (1..=8).contains(&m) {
        let idx = 2 * (m as usize - 1);
        Ok((SEED_ORDER[idx], SEED_ORDER[idx + 1]))
    } else {
        let (c1, c2) = feeder_matchups(m);
        let w1 = *picks
            .get(&c1)
            .ok_or_else(|| MadnessError::InvalidPicks(format!("missing pick for matchup {c1}")))?;
        let w2 = *picks
            .get(&c2)
            .ok_or_else(|| MadnessError::InvalidPicks(format!("missing pick for matchup {c2}")))?;
        Ok((w1, w2))
    }
}

/// Validate a full set of bracket picks: all 15 matchups predicted, and each
/// pick is one of the two legal participants of its matchup.
pub fn validate_picks(picks: &BTreeMap<u8, u8>) -> Result<(), MadnessError> {
    for m in 1..=MATCHUP_COUNT {
        let pick = *picks
            .get(&m)
            .ok_or_else(|| MadnessError::InvalidPicks(format!("missing pick for matchup {m}")))?;
        let (a, b) = matchup_participants(picks, m)?;
        if pick != a && pick != b {
            return Err(MadnessError::InvalidPicks(format!(
                "matchup {m}: seed {pick} is not a participant (expected {a} or {b})"
            )));
        }
    }
    Ok(())
}

/// Score all players against the resolved matchups.
fn score_players(
    players: &[Player],
    matchups: &[Matchup],
    total_mentions: u32,
) -> Vec<PlayerScore> {
    let actual_champion = matchups.last().map(|m| m.winner).unwrap_or(0);
    let mut scores: Vec<PlayerScore> = players
        .iter()
        .map(|p| {
            let mut points = 0;
            let mut correct = 0;
            let mut round_correct = [0u32; 4];
            let mut round_points = [0u32; 4];
            for m in matchups {
                if p.picks.get(&m.id) == Some(&m.winner) {
                    let r = (m.round - 1) as usize;
                    points += ROUND_POINTS[r];
                    correct += 1;
                    round_correct[r] += 1;
                    round_points[r] += ROUND_POINTS[r];
                }
            }
            let predicted_champion = p.picks.get(&MATCHUP_COUNT).copied().unwrap_or(0);
            let tiebreaker_delta = p.tiebreaker_total.map(|g| g.abs_diff(total_mentions));
            PlayerScore {
                name: p.name.clone(),
                points,
                correct,
                round_correct,
                round_points,
                predicted_champion,
                champion_correct: predicted_champion == actual_champion,
                tiebreaker_total: p.tiebreaker_total,
                tiebreaker_delta,
            }
        })
        .collect();
    scores.sort_by(|x, y| {
        y.points
            .cmp(&x.points)
            .then(tiebreaker_cmp(x.tiebreaker_delta, y.tiebreaker_delta))
            .then(y.correct.cmp(&x.correct))
            .then(x.name.cmp(&y.name))
    });
    scores
}

/// Order tiebreaker deltas so a smaller delta ranks first and a missing
/// guess ranks last.
fn tiebreaker_cmp(x: Option<u32>, y: Option<u32>) -> std::cmp::Ordering {
    match (x, y) {
        (Some(a), Some(b)) => a.cmp(&b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

// ── Persistence ────────────────────────────────────────────────

/// Directory where games are stored (`~/.minutes/madness`).
pub fn games_dir() -> PathBuf {
    Config::minutes_dir().join("madness")
}

/// Path to a game's JSON file.
pub fn game_path(slug: &str) -> PathBuf {
    games_dir().join(format!("{slug}.json"))
}

/// Persist a game to disk (0600 on Unix).
pub fn save_game(game: &BracketGame) -> Result<(), MadnessError> {
    let dir = games_dir();
    fs::create_dir_all(&dir)?;
    let path = game_path(&game.slug);
    let json = serde_json::to_string_pretty(game)?;
    fs::write(&path, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Load a game by slug.
pub fn load_game(slug: &str) -> Result<BracketGame, MadnessError> {
    let path = game_path(slug);
    if !path.exists() {
        return Err(MadnessError::NotFound(slug.to_string()));
    }
    let json = fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&json)?)
}

/// List saved game slugs, sorted.
pub fn list_games() -> Result<Vec<String>, MadnessError> {
    let dir = games_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut slugs = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                slugs.push(stem.to_string());
            }
        }
    }
    slugs.sort();
    Ok(slugs)
}

// ── Input adapters ─────────────────────────────────────────────

/// Extract transcript text from a file. Recognizes:
/// - `.jsonl`: live transcript lines, joining each line's `text` field;
/// - `.md`: a meeting note — text after a `## Transcript` heading, or the
///   whole body minus YAML frontmatter if no such heading;
/// - anything else: the raw file contents.
pub fn transcript_text_from_file(path: &Path) -> Result<String, MadnessError> {
    let raw = fs::read_to_string(path)?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "jsonl" => {
            let mut texts = Vec::new();
            for line in raw.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let value: serde_json::Value = serde_json::from_str(line)
                    .map_err(|e| MadnessError::Parse(format!("invalid JSONL line: {e}")))?;
                if let Some(text) = value.get("text").and_then(|t| t.as_str()) {
                    texts.push(text.to_string());
                }
            }
            Ok(texts.join("\n"))
        }
        "md" => Ok(extract_markdown_transcript(&raw)),
        _ => Ok(raw),
    }
}

/// Pull the transcript body out of a meeting markdown note.
fn extract_markdown_transcript(raw: &str) -> String {
    // Drop YAML frontmatter delimited by leading `---` fences.
    let body = if let Some(rest) = raw.strip_prefix("---") {
        match rest.find("\n---") {
            Some(end) => &rest[end + 4..],
            None => raw,
        }
    } else {
        raw
    };
    // Prefer the section under a `## Transcript` heading, if present.
    for (idx, _) in body.match_indices("## Transcript") {
        let after = &body[idx..];
        if let Some(nl) = after.find('\n') {
            let section = &after[nl + 1..];
            // Stop at the next `## ` heading.
            let end = section.find("\n## ").unwrap_or(section.len());
            return section[..end].trim().to_string();
        }
    }
    body.trim().to_string()
}

// ── Term suggestion (from word-frequency files) ────────────────

/// Common English words that dominate frequency lists but are never
/// buzzwords. Filtered out by [`suggest_terms`].
pub const DEFAULT_STOPWORDS: &[&str] = &[
    "the",
    "a",
    "an",
    "and",
    "or",
    "but",
    "if",
    "of",
    "to",
    "in",
    "on",
    "for",
    "with",
    "as",
    "at",
    "by",
    "from",
    "up",
    "down",
    "out",
    "off",
    "over",
    "under",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "am",
    "do",
    "does",
    "did",
    "have",
    "has",
    "had",
    "having",
    "i",
    "you",
    "he",
    "she",
    "it",
    "we",
    "they",
    "me",
    "him",
    "her",
    "us",
    "them",
    "my",
    "your",
    "his",
    "its",
    "our",
    "their",
    "this",
    "that",
    "these",
    "those",
    "here",
    "there",
    "what",
    "which",
    "who",
    "whom",
    "whose",
    "when",
    "where",
    "why",
    "how",
    "all",
    "any",
    "both",
    "each",
    "few",
    "more",
    "most",
    "other",
    "some",
    "such",
    "no",
    "nor",
    "not",
    "only",
    "own",
    "same",
    "so",
    "than",
    "too",
    "very",
    "can",
    "will",
    "just",
    "would",
    "should",
    "could",
    "now",
    "then",
    "also",
    "about",
    "into",
    "through",
    "after",
    "before",
    "again",
    "once",
    "because",
    "while",
    "get",
    "got",
    "going",
    "go",
    "really",
    "actually",
    "kind",
    "sort",
    "lot",
    "thing",
    "things",
    "okay",
    "ok",
    "yeah",
    "um",
    "uh",
    "like",
    "well",
    "right",
    "know",
    "think",
    "see",
    "want",
    "make",
    "made",
    "say",
    "said",
    "says",
    "one",
    "two",
    "us",
    "let",
    "gonna",
    "wanna",
    // Common verbs / discourse fillers that dominate speech but are not jargon.
    "talk",
    "talking",
    "talked",
    "look",
    "looking",
    "looked",
    "come",
    "came",
    "coming",
    "take",
    "takes",
    "taking",
    "took",
    "give",
    "gives",
    "giving",
    "gave",
    "given",
    "mean",
    "means",
    "meant",
    "need",
    "needs",
    "needed",
    "tell",
    "tells",
    "telling",
    "told",
    "ask",
    "asks",
    "asked",
    "feel",
    "feels",
    "felt",
    "find",
    "finds",
    "finding",
    "found",
    "keep",
    "keeps",
    "kept",
    "put",
    "use",
    "uses",
    "used",
    "using",
    "able",
    "doing",
    "done",
    "let's",
    "guys",
    // Generic adjectives / adverbs.
    "good",
    "great",
    "big",
    "small",
    "new",
    "old",
    "last",
    "first",
    "next",
    "better",
    "best",
    "sure",
    "clear",
    "hard",
    "easy",
    "long",
    "short",
    "full",
    "true",
    "real",
    "pretty",
    "much",
    "many",
    "maybe",
    "probably",
    "basically",
    "literally",
    "honestly",
    "frankly",
    "obviously",
    "definitely",
    // Time / quantity / generic nouns.
    "time",
    "times",
    "year",
    "years",
    "day",
    "days",
    "week",
    "weeks",
    "month",
    "months",
    "today",
    "tomorrow",
    "yesterday",
    "number",
    "numbers",
    "bit",
    "part",
    "parts",
    "point",
    "points",
    "way",
    "ways",
    "place",
    "places",
    "question",
    "questions",
    "people",
    "person",
    "everyone",
    "everybody",
    "something",
    "anything",
    "nothing",
    "everything",
    "someone",
    "stuff",
    "back",
    "around",
    "away",
    "together",
    // All-hands meeting mechanics (ubiquitous, not buzzwords).
    "team",
    "teams",
    "slide",
    "slides",
    "quarter",
    "quarters",
    "meeting",
    "meetings",
    "call",
    "calls",
    "agenda",
    "thanks",
    "thank",
    "please",
    // More generic verbs / adjectives that survive the first pass.
    "work",
    "works",
    "working",
    "worked",
    "continue",
    "continues",
    "continued",
    "continuing",
    "start",
    "starts",
    "started",
    "starting",
    "important",
    "every",
    "help",
    "helps",
    "helping",
    "helped",
    "try",
    "tries",
    "trying",
    "tried",
    "little",
];

/// Parse a word-frequency file. Tolerant of common shapes: `word,count`,
/// `count word`, `word: count`, `word\tcount`, `word count`. Lines without a
/// numeric field are skipped (so headers like `word,count` drop out).
pub fn parse_frequency_file(content: &str) -> Vec<(String, u64)> {
    let mut pairs = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line
            .split(|c: char| c == ',' || c == ':' || c == '\t' || c.is_whitespace())
            .filter(|f| !f.is_empty())
            .collect();
        let mut count: Option<u64> = None;
        let mut words: Vec<&str> = Vec::new();
        for f in fields {
            match f.parse::<u64>() {
                Ok(n) if count.is_none() => count = Some(n),
                _ => words.push(f),
            }
        }
        if let (Some(n), false) = (count, words.is_empty()) {
            pairs.push((words.join(" ").to_lowercase(), n));
        }
    }
    pairs
}

/// Rank candidate buzzwords from frequency pairs: drop stopwords (built-in +
/// `extra_stopwords`), pure numbers, and tokens shorter than 3 characters;
/// merge duplicates; return the top `limit` by descending count.
pub fn suggest_terms(
    freqs: &[(String, u64)],
    extra_stopwords: &[String],
    limit: usize,
) -> Vec<(String, u64)> {
    let stop: std::collections::HashSet<String> = DEFAULT_STOPWORDS
        .iter()
        .map(|s| s.to_string())
        .chain(extra_stopwords.iter().map(|s| s.to_lowercase()))
        .collect();
    let mut merged: BTreeMap<String, u64> = BTreeMap::new();
    for (word, count) in freqs {
        let w = word.trim().to_lowercase();
        if w.len() < 2 || stop.contains(&w) || w.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        *merged.entry(w).or_insert(0) += count;
    }
    let mut ranked: Vec<(String, u64)> = merged.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(limit);
    ranked
}

/// Parse a terms file: one term per line, `Label | alias | alias`. Blank lines
/// and lines beginning with `#` are ignored. Seeds are assigned by position.
pub fn parse_terms_file(content: &str) -> Result<Vec<Term>, MadnessError> {
    let mut terms = Vec::new();
    let mut seed: u8 = 1;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('|').map(|p| p.trim().to_string());
        let label = parts.next().unwrap_or_default();
        if label.is_empty() {
            continue;
        }
        let aliases: Vec<String> = parts.filter(|p| !p.is_empty()).collect();
        terms.push(Term {
            seed,
            label,
            aliases,
        });
        seed += 1;
    }
    Ok(terms)
}

/// Convert a title into a filesystem-safe slug.
pub fn slugify(title: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in title.trim().chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
    }
    slug.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms_16() -> Vec<Term> {
        (1..=16)
            .map(|s| Term {
                seed: s,
                label: format!("term{s}"),
                aliases: vec![],
            })
            .collect()
    }

    #[test]
    fn tokens_split_on_punctuation_and_lowercase() {
        assert_eq!(normalize_tokens("AI-powered!"), vec!["ai", "powered"]);
        assert_eq!(
            normalize_tokens("Synergy, synergy."),
            vec!["synergy", "synergy"]
        );
    }

    #[test]
    fn counts_phrases_with_word_boundaries() {
        let terms = vec![
            Term {
                seed: 1,
                label: "AI".into(),
                aliases: vec![],
            },
            Term {
                seed: 2,
                label: "circle back".into(),
                aliases: vec![],
            },
        ];
        let text = "Let's circle back on AI. The AI strategy again, said Sarah. Circle back!";
        let counts = count_mentions(text, &terms);
        // "AI" appears twice; "again"/"said" must NOT match the AI token.
        assert_eq!(counts[&1], 2);
        // "circle back" appears twice (case-insensitive).
        assert_eq!(counts[&2], 2);
    }

    #[test]
    fn aliases_are_counted() {
        let terms = vec![Term {
            seed: 1,
            label: "AI-powered".into(),
            aliases: vec!["AI powered".into(), "A.I. powered".into()],
        }];
        // All three normalize to ["ai","powered"]; 3 occurrences total.
        let counts = count_mentions("AI-powered, AI powered, A.I. powered", &terms);
        assert_eq!(counts[&1], 3);
    }

    #[test]
    fn round1_pairings_keep_top_seeds_apart() {
        // With counts equal everywhere, every matchup is won by the better seed,
        // so the champion is seed 1 and the final is 1 vs 2.
        let counts: BTreeMap<u8, u32> = (1..=16).map(|s| (s, 0)).collect();
        let matchups = resolve_bracket(&counts);
        assert_eq!(matchups.len(), 15);
        let final_m = matchups.last().unwrap();
        assert_eq!(final_m.round, 4);
        assert_eq!((final_m.a, final_m.b), (1, 2));
        assert_eq!(final_m.winner, 1);
    }

    #[test]
    fn higher_count_causes_upset() {
        // Seed 16 outshouts everyone → wins it all.
        let mut counts: BTreeMap<u8, u32> = (1..=16).map(|s| (s, 1)).collect();
        counts.insert(16, 100);
        let matchups = resolve_bracket(&counts);
        assert_eq!(matchups.last().unwrap().winner, 16);
    }

    #[test]
    fn ties_favor_better_seed() {
        assert_eq!(advance(3, 14, &BTreeMap::new()), 3);
        let counts: BTreeMap<u8, u32> = [(3, 5), (14, 5)].into_iter().collect();
        assert_eq!(advance(3, 14, &counts), 3);
    }

    #[test]
    fn validate_picks_rejects_illegal_participant() {
        let mut picks: BTreeMap<u8, u8> = BTreeMap::new();
        // Round 1 winners (all better seeds).
        for (m, w) in [
            (1, 1),
            (2, 8),
            (3, 5),
            (4, 4),
            (5, 6),
            (6, 3),
            (7, 7),
            (8, 2),
        ] {
            picks.insert(m, w);
        }
        // Round 2.
        for (m, w) in [(9, 1), (10, 4), (11, 3), (12, 2)] {
            picks.insert(m, w);
        }
        // Round 3 + final.
        picks.insert(13, 1);
        picks.insert(14, 2);
        picks.insert(15, 1);
        assert!(validate_picks(&picks).is_ok());

        // Illegal: matchup 15 (1 vs 2) cannot be won by seed 5.
        picks.insert(15, 5);
        assert!(validate_picks(&picks).is_err());
    }

    #[test]
    fn scoring_awards_round_weighted_points() {
        let mut game = BracketGame::new("Test", Some("test"), terms_16()).unwrap();
        // A chalk bracket: better seed always advances.
        let mut chalk: BTreeMap<u8, u8> = BTreeMap::new();
        for (m, w) in [
            (1, 1),
            (2, 8),
            (3, 5),
            (4, 4),
            (5, 6),
            (6, 3),
            (7, 7),
            (8, 2),
        ] {
            chalk.insert(m, w);
        }
        for (m, w) in [(9, 1), (10, 4), (11, 3), (12, 2), (13, 1), (14, 2), (15, 1)] {
            chalk.insert(m, w);
        }
        game.set_player(Player {
            name: "Chalk".into(),
            picks: chalk,
            tiebreaker_total: Some(0),
        })
        .unwrap();
        // Empty transcript → all counts 0 → chalk result. Perfect bracket = 32.
        let results = game.score("");
        assert_eq!(results.champion, 1);
        assert_eq!(results.standings[0].name, "Chalk");
        assert_eq!(results.standings[0].points, 32);
        assert_eq!(results.standings[0].correct, 15);
    }

    #[test]
    fn remove_player_drops_and_reranks() {
        let mut game = BracketGame::new("Test", Some("test"), terms_16()).unwrap();
        let mut chalk: BTreeMap<u8, u8> = BTreeMap::new();
        for (m, w) in [
            (1, 1),
            (2, 8),
            (3, 5),
            (4, 4),
            (5, 6),
            (6, 3),
            (7, 7),
            (8, 2),
        ] {
            chalk.insert(m, w);
        }
        for (m, w) in [(9, 1), (10, 4), (11, 3), (12, 2), (13, 1), (14, 2), (15, 1)] {
            chalk.insert(m, w);
        }
        game.set_player(Player {
            name: "A".into(),
            picks: chalk.clone(),
            tiebreaker_total: None,
        })
        .unwrap();
        game.set_player(Player {
            name: "B".into(),
            picks: chalk,
            tiebreaker_total: None,
        })
        .unwrap();
        game.score("");
        assert_eq!(game.results.as_ref().unwrap().standings.len(), 2);

        assert!(game.remove_player("A"));
        assert_eq!(game.players.len(), 1);
        // Standings re-ranked to the remaining player only.
        assert_eq!(game.results.as_ref().unwrap().standings.len(), 1);
        assert_eq!(game.results.as_ref().unwrap().standings[0].name, "B");

        // Removing a non-existent player is a no-op.
        assert!(!game.remove_player("nobody"));
    }

    #[test]
    fn frequency_parser_handles_multiple_shapes() {
        let pairs =
            parse_frequency_file("word,count\nsynergy,42\nheadwinds: 18\nleverage\t7\nai 99");
        assert!(pairs.contains(&("synergy".into(), 42)));
        assert!(pairs.contains(&("headwinds".into(), 18)));
        assert!(pairs.contains(&("leverage".into(), 7)));
        assert!(pairs.contains(&("ai".into(), 99)));
        // The "word,count" header has no numeric field → dropped.
        assert!(!pairs.iter().any(|(w, _)| w == "word count"));
    }

    #[test]
    fn suggest_terms_drops_stopwords_and_ranks() {
        let freqs = vec![
            ("the".into(), 1000),
            ("to".into(), 900),
            ("team".into(), 500),
            ("talk".into(), 400),
            ("synergy".into(), 42),
            ("headwinds".into(), 18),
            ("ai".into(), 99),
        ];
        let ranked = suggest_terms(&freqs, &[], 10);
        assert_eq!(ranked[0].0, "ai");
        assert_eq!(ranked[1].0, "synergy");
        // Common words and meeting-mechanics terms are filtered out.
        assert!(!ranked
            .iter()
            .any(|(w, _)| w == "the" || w == "to" || w == "team" || w == "talk"));
    }

    #[test]
    fn terms_file_parses_labels_and_aliases() {
        let terms =
            parse_terms_file("# comment\nAI-powered | AI powered\nSynergy\n\nHeadwinds").unwrap();
        assert_eq!(terms.len(), 3);
        assert_eq!(terms[0].seed, 1);
        assert_eq!(terms[0].label, "AI-powered");
        assert_eq!(terms[0].aliases, vec!["AI powered".to_string()]);
        assert_eq!(terms[2].seed, 3);
    }

    #[test]
    fn markdown_transcript_extraction() {
        let md = "---\ntitle: x\n---\n\n## Summary\nblah\n\n## Transcript\n\n[0:00] We will leverage synergy.\n[0:04] Circle back later.\n";
        let text = extract_markdown_transcript(md);
        assert!(text.contains("leverage synergy"));
        assert!(text.contains("Circle back"));
        assert!(!text.contains("blah"));
    }
}
