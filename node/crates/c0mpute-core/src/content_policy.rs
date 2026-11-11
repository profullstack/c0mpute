//! Content policy for image and video generation.
//!
//! Two layers, both on every gateway AND every worker (third-party gateways
//! exist, so a worker never trusts the buyer's checks):
//!
//! 1. **Child safety, always on, not configurable.** A request whose text
//!    (prompt, negative prompt, any planner or storyboard text) combines
//!    sexual or nudity terms with a minor indicator is refused. In adult mode
//!    any minor indicator at all is refused, because the output may be
//!    sexual even when the words are not.
//! 2. **Adult mode is opt-in per request** (workers serve it unless their
//!    operator opts out; gateways opt in) (job field `adult`, default
//!    false). Without it, explicit prompts are refused, the worker adds NSFW
//!    terms to the negative prompt and checks the output with an NSFW image
//!    classifier ([`crate::nsfw`]). With it, the request must be text only
//!    (no image input of any kind) and must not name or point at a real,
//!    identifiable person.
//!
//! Matching is a heuristic over normalized text: lowercase, Unicode
//! compatibility decomposition with accents stripped, zero-width characters
//! removed, Cyrillic and Greek lookalikes folded to Latin, leetspeak digits
//! folded inside words (`t33n`), letter runs squeezed (`teeeen`), and
//! separated letters joined (`t.e.e.n`, `t e e n`). The word lists live in
//! `content_policy/terms.json`, shared byte-for-byte with the web app's
//! TypeScript twin, and both run the same `content_policy/vectors.json`, so
//! the two cannot drift. The real-person check in particular is a heuristic:
//! it catches the usual phrasings ("deepfake", "nude of", "my ex", "@handle",
//! "looks like a celebrity") and cannot know every name.
//!
//! Refusals carry a reason code for counting; the prompt itself is never
//! logged.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::Deserialize;
use unicode_normalization::UnicodeNormalization;

/// Capability tag of a worker that serves adult jobs (`[adult] serve`, on by default).
pub const ADULT_TAG: &str = "c0mpute:adult";

const TERMS_JSON: &str = include_str!("content_policy/terms.json");

/// Why a request was refused. `code()` is what gets counted and logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// Sexual content combined with a minor indicator, or any minor
    /// indicator in an adult request. Never allowed.
    MinorSexual,
    /// Explicit content without adult mode.
    ExplicitRequiresAdult,
    /// Adult mode with a real or identifiable person.
    RealPerson,
    /// Adult mode with an image input (img2img, img2vid, face reference).
    AdultImageInput,
    /// Adult mode on a gateway or worker that did not opt in, or for an
    /// account that is not verified 18+.
    AdultNotEnabled,
    /// Adult mode on a workload that never allows it.
    AdultNotSupported,
    /// A generated output the classifier marked explicit in SFW mode.
    ExplicitOutput,
}

impl Refusal {
    pub const ALL: [Refusal; 7] = [
        Refusal::MinorSexual,
        Refusal::ExplicitRequiresAdult,
        Refusal::RealPerson,
        Refusal::AdultImageInput,
        Refusal::AdultNotEnabled,
        Refusal::AdultNotSupported,
        Refusal::ExplicitOutput,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Refusal::MinorSexual => "minor_sexual",
            Refusal::ExplicitRequiresAdult => "explicit_requires_adult",
            Refusal::RealPerson => "real_person",
            Refusal::AdultImageInput => "adult_image_input",
            Refusal::AdultNotEnabled => "adult_not_enabled",
            Refusal::AdultNotSupported => "adult_not_supported",
            Refusal::ExplicitOutput => "explicit_output",
        }
    }

    /// The generic message a caller sees. It names the rule, never the words
    /// that tripped it.
    pub fn message(self) -> &'static str {
        match self {
            Refusal::MinorSexual => "This request was refused by the content policy.",
            Refusal::ExplicitRequiresAdult => {
                "Explicit content needs adult mode, which is only for verified 18+ accounts."
            }
            Refusal::RealPerson => {
                "Adult mode does not allow real or identifiable people (no deepfakes)."
            }
            Refusal::AdultImageInput => "Adult mode is text only: image inputs are not allowed.",
            Refusal::AdultNotEnabled => "Adult mode is not enabled for this account or gateway.",
            Refusal::AdultNotSupported => "Adult mode is not available for this kind of job.",
            Refusal::ExplicitOutput => {
                "The result was withheld by the content policy (explicit output without adult mode)."
            }
        }
    }

    /// `policy: <code>: <message>`, the form a worker fails a job with.
    pub fn job_error(self) -> String {
        format!("policy: {}: {}", self.code(), self.message())
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// What one request asks for, as text the policy reads.
#[derive(Clone, Debug, Default)]
pub struct PolicyRequest<'a> {
    pub prompt: &'a str,
    pub negative: &'a str,
    /// Planner / storyboard / lyric text that steers the same generation.
    pub extra: &'a [&'a str],
    pub adult: bool,
    /// Any image input: img2img, img2vid, a face or style reference.
    pub has_image_input: bool,
}

#[derive(Deserialize)]
struct Terms {
    minor: Vec<String>,
    minor_phrases: Vec<String>,
    minor_standalone: Vec<String>,
    sexual: Vec<String>,
    sexual_phrases: Vec<String>,
    explicit: Vec<String>,
    explicit_phrases: Vec<String>,
    undress_negative: Vec<String>,
    adult_negative: Vec<String>,
    real_person: Vec<String>,
    real_person_phrases: Vec<String>,
    age_words: HashMap<String, u32>,
    ordinal_words: HashMap<String, u32>,
    age_units: Vec<String>,
    age_prefix: Vec<String>,
    under_words: Vec<String>,
    grade_words: Vec<String>,
    ordinal_suffixes: Vec<String>,
    nsfw_negative: String,
    adult_negative_prompt: String,
}

fn terms() -> &'static Terms {
    static TERMS: OnceLock<Terms> = OnceLock::new();
    TERMS.get_or_init(|| serde_json::from_str(TERMS_JSON).expect("content_policy/terms.json is valid"))
}

/// Terms the worker appends to the negative prompt of every SFW job.
pub fn nsfw_negative_terms() -> &'static str {
    &terms().nsfw_negative
}

/// Terms the worker appends to the negative prompt of every adult job.
pub fn adult_negative_terms() -> &'static str {
    &terms().adult_negative_prompt
}

/// `user` plus `extra`, comma separated, without doubling a separator.
pub fn with_negative(user: &str, extra: &str) -> String {
    let user = user.trim().trim_end_matches(',').trim();
    if user.is_empty() {
        extra.to_string()
    } else {
        format!("{user}, {extra}")
    }
}

// ── normalization ────────────────────────────────────────────────────────

fn fold_homoglyph(c: char) -> char {
    match c {
        // Cyrillic
        'а' => 'a', 'в' => 'b', 'с' => 'c', 'ԁ' => 'd', 'е' | 'ё' | 'є' => 'e', 'һ' | 'н' => 'h',
        'і' | 'ї' => 'i', 'ј' => 'j', 'к' => 'k', 'ӏ' => 'l', 'м' => 'm', 'о' => 'o', 'р' => 'p',
        'ԛ' => 'q', 'ѕ' => 's', 'т' => 't', 'ц' => 'u', 'ѵ' => 'v', 'ԝ' | 'ш' => 'w', 'х' => 'x',
        'у' => 'y', 'з' => 'z',
        // Greek
        'α' => 'a', 'β' => 'b', 'ε' => 'e', 'η' => 'n', 'ι' => 'i', 'κ' => 'k', 'ν' => 'v',
        'ο' => 'o', 'ρ' => 'p', 'τ' => 't', 'υ' => 'u', 'χ' => 'x', 'γ' => 'y', 'ω' => 'w',
        'μ' => 'u', 'σ' | 'ς' => 's',
        other => other,
    }
}

fn is_invisible(c: char) -> bool {
    matches!(c, '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}')
}

/// Lowercase, compatibility-decompose, strip accents and invisible
/// characters, fold lookalike letters. Public so the tests (and the TS twin)
/// can pin it.
pub fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    for c in lowered.nfkd() {
        if is_invisible(c) || unicode_normalization::char::is_combining_mark(c) {
            continue;
        }
        for l in fold_homoglyph(c).to_lowercase() {
            out.push(l);
        }
    }
    out
}

fn leet(c: char) -> char {
    match c {
        '0' => 'o',
        '1' => 'i',
        '3' => 'e',
        '4' => 'a',
        '5' => 's',
        '7' => 't',
        '8' => 'b',
        '@' => 'a',
        '$' => 's',
        other => other,
    }
}

/// Runs of 3+ identical characters squeezed to 2 (`teeeen` -> `teen`).
fn squeeze(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut last = None;
    let mut run = 0;
    for c in word.chars() {
        if Some(c) == last {
            run += 1;
        } else {
            last = Some(c);
            run = 1;
        }
        if run <= 2 {
            out.push(c);
        }
    }
    out
}

/// Every run collapsed to one character (`teen` -> `ten`).
fn single(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut last = None;
    for c in word.chars() {
        if Some(c) != last {
            out.push(c);
        }
        last = Some(c);
    }
    out
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit()
}

/// Split a word into its digit and letter runs (`15yo` -> `15`, `yo`).
fn alnum_runs(word: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut digit = None;
    for c in word.chars() {
        let d = c.is_ascii_digit();
        if digit.is_some_and(|x| x != d) && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        digit = Some(d);
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

struct Token {
    /// Leet-folded and squeezed: what term lists match.
    word: String,
    /// Built from separated single letters: also matched by substring.
    blob: bool,
    /// The raw digit/letter runs (not leet-folded), for ages and grades.
    runs: Vec<String>,
}

/// The analysed text: tokens in order, plus whether an `@handle` appeared.
struct Analysis {
    tokens: Vec<Token>,
    handle: bool,
}

fn analyse(text: &str) -> Analysis {
    let norm = normalize(text);
    // `@name`: a handle, read before `@` folds to `a`.
    let chars: Vec<char> = norm.chars().collect();
    let handle = chars.iter().enumerate().any(|(i, &c)| {
        c == '@'
            && (i == 0 || !is_word_char(chars[i - 1]))
            && chars[i + 1..].iter().take_while(|c| is_word_char(**c) || **c == '_').count() >= 2
    });
    // Words of each whitespace-separated chunk. A chunk made only of single
    // letters with separators (`t.e.e.n`) is one word.
    let mut raw: Vec<(String, bool)> = Vec::new();
    for chunk in norm.split_whitespace() {
        let mapped: String = chunk
            .chars()
            .map(|c| if c == '@' || c == '$' { leet(c) } else { c })
            .collect();
        let parts: Vec<&str> = mapped.split(|c: char| !is_word_char(c)).filter(|p| !p.is_empty()).collect();
        if parts.len() >= 2 && parts.iter().all(|p| p.chars().count() == 1) {
            raw.push((parts.concat(), true));
        } else {
            raw.extend(parts.into_iter().map(|p| (p.to_string(), false)));
        }
    }
    // Three or more single letters in a row (`t e e n`) join too.
    let mut joined: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        let mut j = i;
        while j < raw.len() && !raw[j].1 && raw[j].0.chars().count() == 1 {
            j += 1;
        }
        if j - i >= 3 {
            joined.push((raw[i..j].iter().map(|(w, _)| w.as_str()).collect(), true));
            i = j;
        } else {
            joined.push(raw[i].clone());
            i += 1;
        }
    }
    let tokens = joined
        .into_iter()
        .map(|(w, blob)| {
            let has_alpha = w.chars().any(|c| c.is_ascii_lowercase());
            let folded: String = if has_alpha { w.chars().map(leet).collect() } else { w.clone() };
            Token {
                word: squeeze(&folded),
                blob,
                runs: alnum_runs(&w).into_iter().map(|r| squeeze(&r)).collect(),
            }
        })
        .collect();
    Analysis { tokens, handle }
}

// ── matching ─────────────────────────────────────────────────────────────

fn word_matches(word: &str, term: &str) -> bool {
    if let Some(stem) = term.strip_suffix('*') {
        return word.starts_with(stem);
    }
    if word == term || word.strip_suffix('s') == Some(term) || word.strip_suffix("es") == Some(term) {
        return true;
    }
    let (w, t) = (single(word), single(term));
    t.len() >= 4 && (w == t || w.strip_suffix('s') == Some(t.as_str()))
}

fn blob_contains(word: &str, term: &str) -> bool {
    let stem = term.trim_end_matches('*');
    !stem.contains(' ') && stem.len() >= 3 && word.contains(stem)
}

impl Analysis {
    /// Every word a single term may match: each token's folded word and the
    /// letter runs inside mixed tokens (`1girl` -> `girl`).
    fn words(&self) -> impl Iterator<Item = (&str, bool)> {
        self.tokens.iter().flat_map(|t| {
            std::iter::once((t.word.as_str(), t.blob)).chain(
                t.runs
                    .iter()
                    .filter(move |r| t.runs.len() > 1 && r.chars().all(|c| c.is_ascii_lowercase()))
                    .map(|r| (r.as_str(), false)),
            )
        })
    }

    fn has_term(&self, terms: &[String]) -> bool {
        self.words().any(|(w, blob)| {
            terms.iter().any(|t| {
                if t.contains(' ') {
                    // A phrase written as one word (`schoolgirl`).
                    word_matches(w, &t.replace(' ', ""))
                } else {
                    word_matches(w, t) || (blob && blob_contains(w, t))
                }
            })
        })
    }

    fn has_phrase(&self, phrases: &[String]) -> bool {
        let words: Vec<&str> = self.tokens.iter().map(|t| t.word.as_str()).collect();
        phrases.iter().any(|phrase| {
            let parts: Vec<&str> = phrase.split(' ').collect();
            let joined = parts.concat();
            if self.words().any(|(w, blob)| word_matches(w, &joined) || (blob && blob_contains(w, &joined))) {
                return true;
            }
            parts.len() <= words.len()
                && words
                    .windows(parts.len())
                    .any(|win| win.iter().zip(&parts).all(|(w, p)| word_matches(w, p)))
        })
    }

    /// A stated age under 18 (`15yo`, `aged 12`, `fourteen years old`,
    /// `under 18`) or a school grade (`7th grade`).
    fn has_minor_age(&self) -> bool {
        let t = terms();
        let runs: Vec<&str> = self.tokens.iter().flat_map(|t| t.runs.iter().map(String::as_str)).collect();
        let number = |s: &str| -> Option<u32> {
            if s.chars().all(|c| c.is_ascii_digit()) && s.len() <= 3 {
                s.parse().ok()
            } else {
                t.age_words.get(s).copied()
            }
        };
        let is = |list: &[String], s: Option<&&str>| s.is_some_and(|s| list.iter().any(|x| x == s));
        for (i, run) in runs.iter().enumerate() {
            // `seventh grade`: ordinals only ever name a grade.
            if let Some(n) = t.ordinal_words.get(*run)
                && *n <= 12
                && is(&t.grade_words, runs.get(i + 1))
            {
                return true;
            }
            let Some(n) = number(run) else { continue };
            let next = runs.get(i + 1);
            let prev = i.checked_sub(1).and_then(|p| runs.get(p));
            let prev2 = i.checked_sub(2).and_then(|p| runs.get(p));
            if n <= 17 && is(&t.age_units, next) {
                return true;
            }
            if n <= 17 && is(&t.age_prefix, prev) {
                return true;
            }
            if n <= 18 && (is(&t.under_words, prev) || is(&t.under_words, prev2)) {
                return true;
            }
            let after_suffix = if is(&t.ordinal_suffixes, next) { runs.get(i + 2) } else { next };
            if n <= 12 && is(&t.grade_words, after_suffix) {
                return true;
            }
        }
        false
    }

    fn minor(&self) -> bool {
        let t = terms();
        self.has_term(&t.minor) || self.has_phrase(&t.minor_phrases) || self.has_minor_age()
    }

    fn sexual(&self) -> bool {
        let t = terms();
        self.has_term(&t.sexual) || self.has_phrase(&t.sexual_phrases)
    }

    fn explicit(&self) -> bool {
        let t = terms();
        self.has_term(&t.explicit) || self.has_phrase(&t.explicit_phrases)
    }

    fn real_person(&self) -> bool {
        let t = terms();
        self.handle || self.has_term(&t.real_person) || self.has_phrase(&t.real_person_phrases)
    }
}

/// Check one request. `Ok(())` means it may run.
pub fn check(req: &PolicyRequest<'_>) -> Result<(), Refusal> {
    let t = terms();
    let mut positive = String::from(req.prompt);
    for e in req.extra {
        positive.push('\n');
        positive.push_str(e);
    }
    let pos = analyse(&positive);
    let neg = analyse(req.negative);
    // A negative prompt steers away from what it names: excluding clothes
    // asks for nudity, excluding adult features asks for a younger subject.
    let undress = neg.has_term(&t.undress_negative);
    let sexual = pos.sexual() || undress;
    let minor = pos.minor() || neg.has_term(&t.adult_negative);
    if pos.has_term(&t.minor_standalone) || (minor && (sexual || req.adult)) {
        return Err(Refusal::MinorSexual);
    }
    if req.adult {
        if req.has_image_input {
            return Err(Refusal::AdultImageInput);
        }
        if pos.real_person() {
            return Err(Refusal::RealPerson);
        }
    } else if pos.explicit() || undress {
        return Err(Refusal::ExplicitRequiresAdult);
    }
    Ok(())
}

// ── counters and the worker switch ───────────────────────────────────────

static COUNTS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Count a refusal and log its code with a hashed requester id (never the
/// prompt).
pub fn record(refusal: Refusal, side: &str, requester: Option<&str>) {
    let i = Refusal::ALL.iter().position(|r| *r == refusal).unwrap_or(0);
    COUNTS[i].fetch_add(1, Ordering::Relaxed);
    let who = requester.map(hash_requester).unwrap_or_else(|| "-".into());
    tracing::warn!(reason = refusal.code(), side, requester = %who, "content policy refusal");
}

/// Refusal counts by reason code since this process started.
pub fn counts_json() -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (i, r) in Refusal::ALL.iter().enumerate() {
        map.insert(r.code().into(), COUNTS[i].load(Ordering::Relaxed).into());
    }
    serde_json::Value::Object(map)
}

/// A short, non-reversible id for a requester (first 12 hex of SHA-256).
pub fn hash_requester(id: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(format!("c0mpute-policy:{id}").as_bytes()))[..12].to_string()
}

static WORKER_ADULT: AtomicBool = AtomicBool::new(false);

/// Set once at worker start from `[adult] serve` / `C0MPUTE_ADULT`.
pub fn set_worker_adult(on: bool) {
    WORKER_ADULT.store(on, Ordering::Relaxed);
}

/// This worker serves adult jobs (`[adult] serve`, on unless opted out).
pub fn worker_adult() -> bool {
    WORKER_ADULT.load(Ordering::Relaxed)
}

/// The worker-side gate every generation runner calls before running a job:
/// the same text check as the gateway, plus refusing adult jobs unless this
/// node opted in.
pub fn worker_check(req: &PolicyRequest<'_>) -> Result<(), Refusal> {
    if req.adult && !worker_adult() {
        return Err(Refusal::AdultNotEnabled);
    }
    check(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Vectors {
        cases: Vec<Case>,
    }
    #[derive(Deserialize)]
    struct Case {
        prompt: String,
        #[serde(default)]
        negative: String,
        #[serde(default)]
        extra: Vec<String>,
        adult: bool,
        #[serde(default)]
        image: bool,
        expect: String,
    }

    #[test]
    fn shared_vectors() {
        let v: Vectors = serde_json::from_str(include_str!("content_policy/vectors.json")).unwrap();
        assert!(v.cases.len() >= 30);
        let mut failures = Vec::new();
        for c in &v.cases {
            let extra: Vec<&str> = c.extra.iter().map(String::as_str).collect();
            let got = check(&PolicyRequest {
                prompt: &c.prompt,
                negative: &c.negative,
                extra: &extra,
                adult: c.adult,
                has_image_input: c.image,
            });
            let got = match got {
                Ok(()) => "ok",
                Err(r) => r.code(),
            };
            if got != c.expect {
                failures.push(format!("{:?} (adult={}, neg={:?}): want {}, got {got}", c.prompt, c.adult, c.negative, c.expect));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[test]
    fn normalization_folds_lookalikes_and_invisibles() {
        assert_eq!(normalize("Ｔｅｅｎ"), "teen");
        assert_eq!(normalize("t\u{0435}\u{0435}n"), "teen");
        assert_eq!(normalize("te\u{200b}en"), "teen");
        assert_eq!(normalize("Ñúdé"), "nude");
        assert_eq!(squeeze("teeeeen"), "teen");
        assert_eq!(alnum_runs("15yo"), vec!["15", "yo"]);
    }

    #[test]
    fn words_inside_other_words_do_not_match() {
        for benign in ["canteen", "essex sussex", "skidding", "pedometer reading", "torpedo"] {
            let a = analyse(benign);
            assert!(!a.minor() && !a.sexual(), "{benign}");
        }
    }

    #[test]
    fn worker_refuses_adult_unless_opted_in() {
        let req = PolicyRequest { prompt: "a lighthouse", adult: true, ..Default::default() };
        set_worker_adult(false);
        assert_eq!(worker_check(&req), Err(Refusal::AdultNotEnabled));
        set_worker_adult(true);
        assert_eq!(worker_check(&req), Ok(()));
        set_worker_adult(false);
        let sfw = PolicyRequest { prompt: "a lighthouse", ..Default::default() };
        assert_eq!(worker_check(&sfw), Ok(()));
    }

    #[test]
    fn requester_hash_is_short_and_stable() {
        let h = hash_requester("subject-1");
        assert_eq!(h.len(), 12);
        assert_eq!(h, hash_requester("subject-1"));
        assert_ne!(h, hash_requester("subject-2"));
    }

    #[test]
    fn negative_terms_append_cleanly() {
        assert_eq!(with_negative("", "a, b"), "a, b");
        assert_eq!(with_negative("blurry,", "a"), "blurry, a");
    }
}
