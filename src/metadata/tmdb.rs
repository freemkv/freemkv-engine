use crate::metadata::MediaKind;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TmdbResult {
    pub title: String,
    pub year: u16,
    pub poster_url: String,
    pub overview: String,
    #[serde(default, deserialize_with = "crate::metadata::lenient")]
    pub media_type: MediaKind,
    /// TMDB numeric id for this match (0 = unknown). Carried so a consumer —
    /// autorip's own metadata enrichment, or kdb resolving a volume id via
    /// [`lookup`] — can fetch anything else it wants straight from TMDB by id,
    /// without re-searching. `serde(default)` so markers written before this
    /// field existed still deserialize.
    #[serde(default)]
    pub tmdb_id: u64,
}

/// TMDB naming client. Diagnostics go to the caller's normal engine sink.
/// It never selects disc titles or changes their playback ranges.
pub struct Client<'a> {
    api_key: &'a str,
    sink: &'a dyn crate::Sink,
}

impl<'a> Client<'a> {
    pub fn new(api_key: &'a str, sink: &'a dyn crate::Sink) -> Self {
        Self { api_key, sink }
    }
    pub fn lookup(&self, label: &str) -> Result<Option<TmdbResult>, LookupFailed> {
        lookup_with(label, self.api_key, self.sink)
    }
    pub fn search(&self, query: &str, limit: usize) -> Vec<TmdbResult> {
        search_with(query, self.api_key, limit, self.sink)
    }
    pub fn season_episodes(&self, tv_id: u64, season: u16) -> Vec<Episode> {
        season_episodes_with(tv_id, season, self.api_key, self.sink)
    }
}

/// Identify a disc label without a front-end diagnostic sink.
pub fn lookup(label: &str, api_key: &str) -> Result<Option<TmdbResult>, LookupFailed> {
    Client::new(api_key, &crate::NoopSink).lookup(label)
}
/// Search naming candidates without a front-end diagnostic sink.
pub fn search(query: &str, api_key: &str, limit: usize) -> Vec<TmdbResult> {
    Client::new(api_key, &crate::NoopSink).search(query, limit)
}
/// Fetch episode names and numbers without a front-end diagnostic sink.
pub fn season_episodes(tv_id: u64, season: u16, api_key: &str) -> Vec<Episode> {
    Client::new(api_key, &crate::NoopSink).season_episodes(tv_id, season)
}

// Shared agent for all TMDB calls: ureq sets NO connect/read timeout by default, so a hung
// connection would wedge the rip thread or a web handler indefinitely. The body gets its own
// bound: headers arriving in time says nothing about a body that then stalls.
static AGENT: std::sync::LazyLock<ureq::Agent> =
    std::sync::LazyLock::new(|| build_agent(std::time::Duration::from_secs(10)));

fn build_agent(recv_body: std::time::Duration) -> ureq::Agent {
    let config = ureq::config::Config::builder()
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(10)))
        .timeout_recv_body(Some(recv_body))
        // Follow NO redirects: the URL carries the operator's api_key in its
        // query string, so a 3xx (TMDB compromised/tampered on-path) would
        // hand that key to an arbitrary host; `fetch_multi` reports it as "no result".
        .max_redirects(0)
        .build();
    ureq::Agent::new_with_config(config)
}

// API origin. Tests point it at a loopback server (per thread).
#[cfg(not(test))]
fn api_base() -> &'static str {
    "https://api.themoviedb.org"
}
#[cfg(test)]
thread_local! {
    pub(super) static TEST_API_BASE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn api_base() -> String {
    TEST_API_BASE
        .with(|b| b.borrow().clone())
        .unwrap_or_else(|| "https://api.themoviedb.org".to_string())
}

// Build the `search/multi` URL. Both `api_key` and `query` are percent-encoded:
// a stray space/&/#/= in a copy-pasted key would otherwise yield a malformed
// or silently-wrong URL, and `query` is untrusted disc-label content.
fn search_multi_url(query: &str, api_key: &str) -> String {
    format!(
        "{}/3/search/multi?api_key={}&query={}&page=1",
        api_base(),
        urlencoded(api_key),
        urlencoded(query)
    )
}

// Cap on the TMDB response body we'll buffer. A real `search/multi` response
// is tens of KB; 2 MiB is generous headroom. Bounding it stops a hostile or
// broken endpoint from streaming an unbounded body into memory (DoS).
const MAX_TMDB_BYTES: u64 = 2 * 1024 * 1024;

// Read at most `cap` bytes, rejecting anything over: an oversized body reads `cap+1` bytes
// successfully then fails the boundary check below, rather than being silently truncated.
fn read_capped_bytes(reader: impl std::io::Read, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    reader.take(cap + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "tmdb response exceeded size cap",
        ));
    }
    Ok(buf)
}

/// Read at most `MAX_TMDB_BYTES` from the response body, rejecting anything
/// over the cap, then parse as JSON. Replaces `resp.into_json()`, which reads
/// the whole body with no upper bound.
fn read_capped_json(resp: ureq::http::Response<ureq::Body>) -> std::io::Result<serde_json::Value> {
    let buf = read_capped_bytes(resp.into_body().into_reader(), MAX_TMDB_BYTES)?;
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// Run a `search/multi` request via the shared timeout-bounded AGENT. Splits
// out 401 (bad key, throttled warning) from other status/transport errors
// instead of collapsing every failure to "no results", hiding the cause.
fn fetch_multi(query: &str, api_key: &str, sink: &dyn crate::Sink) -> Fetched {
    fetch_url_checked(&search_multi_url(query, api_key), query, sink)
}

/// Why a fetch failed, as far as the query-variant loop cares.
#[derive(Debug, PartialEq)]
enum Failure {
    /// No other query can fix it (transport, 401, 429, 5xx): stop asking.
    Stop,
    /// Specific to this query (other 4xx, unreadable body): a peeled one may match.
    Next,
}

type Fetched = Result<serde_json::Value, Failure>;

fn status_failure(code: u16) -> Failure {
    if matches!(code, 401 | 429 | 500..=599) {
        Failure::Stop
    } else {
        Failure::Next
    }
}

fn fetch_url(url: &str, query: &str, sink: &dyn crate::Sink) -> Option<serde_json::Value> {
    fetch_url_checked(url, query, sink).ok()
}

fn fetch_url_checked(url: &str, query: &str, sink: &dyn crate::Sink) -> Fetched {
    match AGENT.get(url).call() {
        Ok(resp) => match read_capped_json(resp) {
            Ok(json) => Ok(json),
            Err(e) => {
                tracing::warn!(query = %query, error = %e, "tmdb: response was not valid JSON");
                sink.log(
                    crate::Level::Warn,
                    "TMDB: response could not be read or parsed",
                );
                Err(Failure::Next)
            }
        },
        Err(ureq::Error::StatusCode(401)) => {
            warn_bad_key_throttled(sink);
            Err(Failure::Stop)
        }
        Err(ureq::Error::StatusCode(code)) => {
            tracing::warn!(query = %query, status = code, "tmdb: HTTP error status");
            sink.log(
                crate::Level::Warn,
                &format!("TMDB: HTTP error status {code}"),
            );
            Err(status_failure(code))
        }
        Err(e) => {
            // Do NOT log `e` directly: the URL has the api_key in its query
            // string, and `BadUri`'s Display still prints the rejected URI,
            // so masking stays even though ureq 3 is URL-free elsewhere.
            let error_kind = super::transport_error_kind(&e);
            tracing::warn!(query = %query, error_kind = %error_kind, "tmdb: request failed (network/transport)");
            sink.log(
                crate::Level::Warn,
                &format!("TMDB: request failed (network/transport: {error_kind})"),
            );
            Err(Failure::Stop)
        }
    }
}

/// One-per-minute warning that the configured TMDB API key was rejected.
fn warn_bad_key_throttled(sink: &dyn crate::Sink) {
    if bad_key_warning_due() {
        tracing::warn!(
            "tmdb: API key rejected (HTTP 401) — check the TMDB API key in Settings; \
             titles will fall through to the needs-review queue until it is fixed"
        );
        sink.log(
            crate::Level::Warn,
            "TMDB API key rejected (HTTP 401) — check the TMDB API key in Settings",
        );
    }
}

// True at most once a minute. Per thread under test, so each test starts fresh.
fn bad_key_warning_due() -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    fn due(last_warn: &AtomicU64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let last = last_warn.load(Ordering::Relaxed);
        now.saturating_sub(last) >= 60
            && last_warn
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }
    #[cfg(not(test))]
    {
        static LAST_WARN_SECS: AtomicU64 = AtomicU64::new(0);
        due(&LAST_WARN_SECS)
    }
    #[cfg(test)]
    {
        thread_local! {
            static LAST_WARN_SECS: AtomicU64 = const { AtomicU64::new(0) };
        }
        LAST_WARN_SECS.with(due)
    }
}

/// Resolve a RAW disc volume `label` to a TMDB movie/TV entry. Cleaning and fallback
/// trimming live here, so the lookup and the auto-file gate ([`is_confident_match`]) never
/// disagree on what was searched: the cleaned label, then, with no confident match, with
/// junk-shaped trailing tokens peeled one at a time (`query_variants`). Returns the first
/// confident match (exact title + year), else the best guess; a failed query with no
/// confident match is an error, not "no match".
fn lookup_with(
    label: &str,
    api_key: &str,
    sink: &dyn crate::Sink,
) -> Result<Option<TmdbResult>, LookupFailed> {
    if api_key.is_empty() {
        return Ok(None);
    }
    lookup_using(label, |variant| fetch_multi(variant, api_key, sink))
}

/// TMDB could not be asked (network, HTTP or response failure), as opposed to "no match".
/// Carries the best non-confident guess the queries that did answer found.
#[derive(Debug)]
pub struct LookupFailed(pub Option<TmdbResult>);

/// The lookup's answer for a caller that must not wait on TMDB: a failed query still
/// yields the best guess found.
pub fn best_effort(found: Result<Option<TmdbResult>, LookupFailed>) -> Option<TmdbResult> {
    found.unwrap_or_else(|failed| failed.0)
}

// `lookup`'s variant loop over any `search/multi` source.
fn lookup_using(
    label: &str,
    fetch: impl Fn(&str) -> Fetched,
) -> Result<Option<TmdbResult>, LookupFailed> {
    // A separator-only label yields no query variants; short-circuit rather
    // than firing `query=&...` (TMDB answers HTTP 422). A season marker
    // ("… Season 5") means TV — bias the pick so it can't be outranked.
    let prefer_tv = season_from_label(label).is_some();
    let (_, years) = strip_paren_year(label);
    let mut fallback: Option<TmdbResult> = None;
    let mut failed = false;
    for variant in query_variants(label) {
        if variant.trim().is_empty() {
            continue;
        }
        let resp = match fetch(&variant) {
            Ok(resp) => resp,
            Err(Failure::Stop) => {
                failed = true;
                break;
            }
            Err(Failure::Next) => {
                failed = true;
                continue;
            }
        };
        let Some(results) = resp["results"].as_array() else {
            failed = true;
            continue;
        };
        if let Some(best) = pick_best(&variant, results, prefer_tv, &years) {
            // Confidence also requires every explicit label year to agree.
            if year_matches(best.year, &years) && norm(&best.title) == norm(&variant) {
                return Ok(Some(best));
            }
            if fallback.is_none() {
                fallback = Some(best);
            }
        }
    }
    match fallback {
        Some(best) if is_confident_match(label, &best.title, best.year) => Ok(Some(best)),
        found if failed => Err(LookupFailed(found)),
        found => Ok(found),
    }
}

/// Normalize a title for comparison: lowercase, every run of non-alphanumerics
/// collapses to one space, trimmed. So "Top Gun: Maverick" and the disc label
/// "Top Gun Maverick" both become "top gun maverick".
fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut sep = true; // leading: suppress a leading space
    for c in s.chars() {
        // Unicode-aware: keep accented letters/digits (so "Amélie" and
        // "Pokémon" can match exactly) instead of stripping all non-ASCII.
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            sep = false;
        } else if !sep {
            out.push(' ');
            sep = true;
        }
    }
    out.trim_end().to_string()
}

/// Is the resolved `title`/`year` a CONFIDENT match for the disc `label`?
///
/// Requires a nonzero year agreeing with every parenthesized four-digit year in the RAW label,
/// and an exact normalized title match to its cleaned label or a progressively-trimmed variant
/// that [`lookup`] searches. Conflicting explicit years are never confident. Non-confident
/// matches (or those overwriting an existing file) are held for operator review.
pub fn is_confident_match(label: &str, title: &str, year: u16) -> bool {
    let (_, years) = strip_paren_year(label);
    year_matches(year, &years) && query_variants(label).iter().any(|v| norm(v) == norm(title))
}

fn year_matches(year: u16, explicit_years: &[u16]) -> bool {
    year > 0 && explicit_years.iter().all(|&explicit| explicit == year)
}

/// The one result a typed name names: exactly one of `kind` whose title is `name` (as
/// `is_confident_match` compares titles), and of `year` when the operator gave one.
/// Two remakes or a name no title carries leave it a plain name.
pub fn exact_match<'a>(
    results: &'a [TmdbResult],
    name: &str,
    kind: MediaKind,
    year: u16,
) -> Option<&'a TmdbResult> {
    let mut hits = results.iter().filter(|r| {
        r.media_type == kind && norm(&r.title) == norm(name) && (year == 0 || r.year == year)
    });
    let hit = hits.next()?;
    hits.next().is_none().then_some(hit)
}

/// Return up to `limit` candidate matches for `query`, best first (exact dated
/// title → dated → popularity). Powers the "needs review" correction picker.
fn search_with(
    query: &str,
    api_key: &str,
    limit: usize,
    sink: &dyn crate::Sink,
) -> Vec<TmdbResult> {
    if api_key.is_empty() || query.trim().is_empty() {
        return Vec::new();
    }
    let Ok(json) = fetch_multi(query, api_key, sink) else {
        return Vec::new();
    };
    let Some(results) = json["results"].as_array() else {
        return Vec::new();
    };
    rank_search_results(query, results, limit)
}

// The pure ranking half of `search`: parse every movie/tv entry, sort exact-dated-match first,
// dated second, popularity as tiebreaker, cap at `limit`.
fn rank_search_results(
    query: &str,
    results: &[serde_json::Value],
    limit: usize,
) -> Vec<TmdbResult> {
    let want = norm(query);
    let mut parsed: Vec<(TmdbResult, f64, bool)> = results
        .iter()
        .filter_map(parse_result)
        .map(|(r, pop)| {
            let exact = r.year > 0 && norm(&r.title) == want;
            (r, pop, exact)
        })
        .collect();
    parsed.sort_by(|a, b| {
        b.2.cmp(&a.2) // exact first
            .then((b.0.year > 0).cmp(&(a.0.year > 0))) // then dated
            .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)) // then popularity
    });
    parsed.into_iter().take(limit).map(|(r, _, _)| r).collect()
}

// Choose the best entry from a TMDB `search/multi` response: keep only movie/TV entries, prefer
// ones with a release year, break ties on popularity.
fn pick_best(
    query: &str,
    results: &[serde_json::Value],
    prefer_tv: bool,
    years: &[u16],
) -> Option<TmdbResult> {
    let want = norm(query);
    // Rank exact title with the requested year first, then the original
    // (exact, dated, tv_preferred, popularity) order for best-effort guesses.
    let key = |cand: &TmdbResult, pop: f64| {
        let exact = cand.year > 0 && !want.is_empty() && norm(&cand.title) == want;
        (
            u8::from(exact) + u8::from(exact && year_matches(cand.year, years)),
            cand.year > 0,
            prefer_tv && cand.media_type.is_tv(),
            pop,
        )
    };
    let mut best: Option<(TmdbResult, (u8, bool, bool, f64))> = None;
    for v in results {
        let Some((cand, popularity)) = parse_result(v) else {
            continue;
        };
        let k = key(&cand, popularity);
        let better = best.as_ref().is_none_or(|(_, bk)| key_gt(k, *bk));
        if better {
            best = Some((cand, k));
        }
    }
    best.map(|(r, _)| r)
}

/// Lexicographic "is `a` a better rank than `b`" for [`pick_best`]'s key. Hand
/// rolled because the key ends in an `f64` (popularity), which is not `Ord`.
fn key_gt(a: (u8, bool, bool, f64), b: (u8, bool, bool, f64)) -> bool {
    if a.0 != b.0 {
        return a.0 > b.0;
    }
    if a.1 != b.1 {
        return a.1;
    }
    if a.2 != b.2 {
        return a.2;
    }
    a.3 > b.3
}

/// Parse one `search/multi` result into a `TmdbResult` + its popularity.
/// Returns `None` for non-movie/TV entries (people, collections) and for
/// entries missing a usable title.
fn parse_result(v: &serde_json::Value) -> Option<(TmdbResult, f64)> {
    // Default to "" (not "movie") so an entry that is missing media_type is
    // rejected by the guard below rather than silently admitted as a movie.
    let media_type = v["media_type"].as_str().unwrap_or("");
    if media_type != "movie" && media_type != "tv" {
        return None;
    }
    let kind = MediaKind::parse(media_type);
    let title = v
        .get(if kind.is_tv() { "name" } else { "title" })?
        .as_str()?
        .to_string();
    if title.is_empty() {
        return None;
    }
    let date = v
        .get(if kind.is_tv() {
            "first_air_date"
        } else {
            "release_date"
        })
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let year: u16 = date.get(..4).and_then(|y| y.parse().ok()).unwrap_or(0);
    // TMDB poster_path is always a host-absolute path ("/abc.jpg"). Guard
    // the leading slash so a slashless or unexpected value can't produce a
    // malformed/host-relative image URL — keeps the empty-path behavior.
    let poster = v["poster_path"]
        .as_str()
        .filter(|p| p.starts_with('/'))
        .map(|p| format!("https://image.tmdb.org/t/p/w300{p}"))
        .unwrap_or_default();
    let overview = v["overview"].as_str().unwrap_or("").to_string();
    let tmdb_id = v["id"].as_u64().unwrap_or(0);
    Some((
        TmdbResult {
            title,
            year,
            poster_url: poster,
            overview,
            media_type: kind,
            tmdb_id,
        },
        v["popularity"].as_f64().unwrap_or(0.0),
    ))
}

// Strip and retain all parenthesized four-digit years; bare title numbers stay.
// Char-based so a multibyte label never panics.
fn strip_paren_year(s: &str) -> (String, Vec<u16>) {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut years = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // Match "(dddd)" exactly: '(' then 4 ASCII digits then ')'.
        if chars[i] == '('
            && i + 5 < chars.len()
            && chars[i + 5] == ')'
            && chars[i + 1..i + 5].iter().all(|c| c.is_ascii_digit())
        {
            years.push(
                chars[i + 1..i + 5]
                    .iter()
                    .fold(0, |year, c| year * 10 + (*c as u16 - u16::from(b'0'))),
            );
            i += 6; // skip the whole "(dddd)" group
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    (out, years)
}

/// Clean a disc label for TMDB search: "AURORA_DRIFT_TWO" -> "Aurora Drift Two"
/// Strips common disc suffixes like "4K Ultra HD", "Blu-ray", "DVD", etc., and
/// a parenthesized release year (see `strip_paren_year`), and peels a trailing
/// season marker ("Season 5") so a series disc resolves to the show title.
pub fn clean_title(label: &str) -> String {
    let s = label.replace(['_', '-'], " ");
    let s = strip_paren_year(&s).0;

    // Strip common disc format suffixes (case-insensitive)
    let suffixes = [
        "4k ultra hd",
        "4k uhd",
        "ultra hd",
        "blu ray",
        "bluray",
        "dvd",
    ];
    // Search AND slice the SAME (lowercased) string: `to_lowercase()` can
    // change byte length, so an offset in `lower` may not index into `s`.
    // Strip only END-anchored suffixes, never embedded; repeat to peel groups.
    let lower = s.to_lowercase();
    let mut clipped = lower.as_str();
    loop {
        // Trim trailing whitespace AND non-alphanumeric junk (™/®, punctuation)
        // before testing the END-anchor: retail labels carry such trailing
        // chars ("Ultra HD™"), and whitespace-only trimming left it un-anchored.
        let trimmed = clipped.trim_end_matches(|c: char| !c.is_alphanumeric());
        let mut next: Option<&str> = None;
        for suffix in &suffixes {
            if let Some(pos) = trimmed.rfind(suffix)
                && pos + suffix.len() == trimmed.len()
            {
                next = Some(&trimmed[..pos]);
                break;
            }
        }
        // Also peel a trailing season marker ("Season 5") so a series disc
        // resolves to the base show title, only when no format suffix
        // matched this round — the two peels interleave across the loop.
        if next.is_none() {
            next = strip_trailing_season(trimmed).or_else(|| strip_trailing_disc(trimmed));
        }
        match next {
            Some(rest) => clipped = rest,
            None => {
                clipped = trimmed;
                break;
            }
        }
    }
    let trimmed = clipped.trim();

    trimmed
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// Unambiguous TV season markers. A trailing "<word> <number>" of one of these is peeled by
// `clean_title` so a series disc resolves to the base show title.
const SEASON_WORDS: &[&str] = &["season", "series", "saison", "staffel", "seizoen"];
const DISC_WORDS: &[&str] = &["disc", "disk"];

// If `s` (lowercased, trailing-junk-trimmed) ends with a season marker like
// "season 5", return `s` with that marker removed; else None. The number is
// required — a bare trailing "season" is left alone (e.g. "Silly Season").
fn strip_trailing_season(s: &str) -> Option<&str> {
    strip_trailing_word_number(s, SEASON_WORDS, |_| true)
}

// Same for a trailing "disc N"/"disk N", N in the range `disc_from_label` accepts.
fn strip_trailing_disc(s: &str) -> Option<&str> {
    strip_trailing_word_number(s, DISC_WORDS, number_in_range)
}

fn number_in_range(digits: &str) -> bool {
    digits.parse::<u16>().is_ok_and(|n| (1..=99).contains(&n))
}

fn strip_trailing_word_number<'a>(
    s: &'a str,
    words: &[&str],
    number_ok: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    let digits_start = s.trim_end_matches(|c: char| c.is_ascii_digit());
    if digits_start.len() == s.len() || !number_ok(&s[digits_start.len()..]) {
        return None; // no trailing digits, or out of range
    }
    let head = digits_start.trim_end(); // strip the space(s) before the number
    for word in words {
        if let Some(pos) = head.rfind(word)
            && pos + word.len() == head.len()
            // Must be a whole word: start of string or a non-alphanumeric before it.
            && (pos == 0 || !head[..pos].ends_with(|c: char| c.is_alphanumeric()))
        {
            return Some(head[..pos].trim_end());
        }
    }
    None
}

/// Parse a TV season number from a disc `label`: "Longacre Season 5 Disc 2"
/// → 5, "GAMEOFTHRONES_S3_DISC1" → 3. Returns 1..=99, else `None`.
///
/// A season marker is the signal that a disc is TV rather than a film, and the
/// number is what the mover uses to place the rip under `Show (Year)/Season NN/`.
/// Recognizes the spelled-out `SEASON_WORDS` followed by a number, or a
/// compact `S<n>` token.
pub fn season_from_label(label: &str) -> Option<u16> {
    number_after_word(label, SEASON_WORDS).or_else(|| compact_token_number(label, &["s"]))
}

/// Parse a disc number from a `label`: "… Disc 2" → 2, "GOT_S3_D4" → 4,
/// "BD1" → 1. Returns 1..=99, else `None`. Used to sequence a multi-disc set.
pub fn disc_from_label(label: &str) -> Option<u16> {
    number_after_word(label, DISC_WORDS)
        .or_else(|| compact_token_number(label, &["disc", "disk", "bd", "d"]))
}

/// The number immediately following any of `words` in `s` (case-insensitive,
/// tolerating `_ - . :` separators): "Season 5" → 5, "series2" → 2. 1..=99.
fn number_after_word(s: &str, words: &[&str]) -> Option<u16> {
    let low = s.to_lowercase();
    for word in words {
        let mut from = 0;
        while let Some(rel) = low[from..].find(word) {
            let pos = from + rel;
            let digits: String = low[pos + word.len()..]
                .trim_start_matches([' ', '_', '-', '.', ':'])
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(n) = digits.parse::<u16>()
                && (1..=99).contains(&n)
            {
                return Some(n);
            }
            from = pos + word.len();
        }
    }
    None
}

/// A compact standalone token `<prefix><digits>` — "S3", "D2", "BD1", "DISC1".
fn compact_token_number(s: &str, prefixes: &[&str]) -> Option<u16> {
    for tok in s.split([' ', '_', '-', '.', ':']) {
        let low = tok.to_lowercase();
        for pfx in prefixes {
            if let Some(rest) = low.strip_prefix(pfx)
                && !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_digit())
                && let Ok(n) = rest.parse::<u16>()
                && (1..=99).contains(&n)
            {
                return Some(n);
            }
        }
    }
    None
}

// Edition / release qualifiers that annotate a cut but are not part of the
// film's TMDB title ("Ultimate Edition", "Director's Cut"). Peeled only when
// TRAILING (see `is_trailing_junk`) so an interior word is never removed.
const EDITION_WORDS: &[&str] = &[
    "ultimate",
    "extended",
    "theatrical",
    "director",
    "directors",
    "special",
    "collector",
    "collectors",
    "anniversary",
    "final",
    "unrated",
    "remastered",
    "limited",
    "deluxe",
    "steelbook",
    "edition",
    "cut",
    "version",
];

/// Region / market codes that retail volume labels append.
const REGION_WORDS: &[&str] = &["uk", "usa", "us", "eu", "na", "ww", "aus", "region"];

/// Disc-format words. `clean_title` already strips multi-word variants
/// ("4k ultra hd", "blu ray"); these single tokens are what a trailing-token
/// peel sees ("BD", "UHD", "3D"-style tokens are caught by the alnum rule).
const FORMAT_WORDS: &[&str] = &[
    "bd", "bdrom", "uhd", "4k", "hd", "sd", "dvd", "bluray", "video",
];

/// Is `s` (lowercased) composed only of roman-numeral letters? Used only to
/// PREVENT trimming a trailing roman numeral ("Rocky II"): a false positive on
/// a real word like "mix" merely keeps that token, which is always safe.
fn is_roman_numeral(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| matches!(c, 'i' | 'v' | 'x' | 'l' | 'c' | 'd' | 'm'))
}

// May this TRAILING token be safely peeled off a disc label? Edition/region/ format words and
// obvious codes — but NEVER a bare number or roman numeral, which are sequel markers ("Alien
// 3", "Rocky II").
fn is_trailing_junk(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    // Sequel markers are meaningful — never peel.
    if tok.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let low = tok.to_lowercase();
    if is_roman_numeral(&low) {
        return false;
    }
    if EDITION_WORDS.contains(&low.as_str())
        || REGION_WORDS.contains(&low.as_str())
        || FORMAT_WORDS.contains(&low.as_str())
    {
        return true;
    }
    // Mixed-alphanumeric packaging/format code: BD3, UPT1, G51, D2, 3D, 4K.
    let has_alpha = tok.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = tok.chars().any(|c| c.is_ascii_digit());
    if has_alpha && has_digit {
        return true;
    }
    // Short all-caps abbreviation (not roman numeral): UE, SE, EDC, WW. Case
    // is preserved here since variants are peeled BEFORE `clean_title`
    // title-cases, so "UE" (junk) stays distinguishable from "Us" (real title).
    tok.len() <= 4 && tok.chars().all(|c| c.is_ascii_uppercase())
}

// Progressively-trimmed TMDB query variants for a disc `label`, most specific first. Variant 0
// is `clean_title(label)`; each next variant peels one more junk-shaped trailing token
// (`is_trailing_junk`).
fn query_variants(label: &str) -> Vec<String> {
    const MAX_QUERY_VARIANTS: usize = 5;
    // Same separator/year normalization clean_title applies, but WITHOUT the
    // case folding, so junk detection keeps the label's original casing.
    let base = label.replace(['_', '-'], " ");
    let base = strip_paren_year(&base).0;
    let toks: Vec<&str> = base.split_whitespace().collect();
    let mut variants: Vec<String> = Vec::new();
    let mut end = toks.len();
    loop {
        let q = clean_title(&toks[..end].join(" "));
        // Decide from the tokens clean_title kept, not the raw last one it may
        // have already stripped (e.g. "disc 2").
        let kept = q.split_whitespace().count().min(end);
        if !q.is_empty() && !variants.iter().any(|v| v == &q) {
            variants.push(q);
        }
        if variants.len() >= MAX_QUERY_VARIANTS || kept <= 1 || !is_trailing_junk(toks[kept - 1]) {
            break;
        }
        end = kept - 1;
    }
    variants
}

fn urlencoded(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{:02X}", b),
        })
        .collect()
}

// ---- TV episode resolution ------------------------------------------------

/// One episode from a TMDB season listing.
#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    pub number: u16,
    pub name: String,
    /// Runtime in minutes (0 = unknown), used to sanity-check the order-based
    /// title→episode pairing.
    pub runtime_min: u16,
}

/// The episode a ripped title is assigned to, for `Show S{NN}E{MM}` naming.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeAssignment {
    pub episode: u16,
    /// TMDB episode name, or "" when unknown (degraded / low-confidence).
    pub name: String,
}

/// Attach TMDB names to disc-ordered episodes without using TMDB runtime or
/// episode count to select titles or infer where this disc begins.
pub fn numbered_episode_names(
    count: usize,
    episodes: &[Episode],
    start: u16,
) -> Vec<EpisodeAssignment> {
    (0..count)
        .map(|i| {
            let episode = start.saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
            let name = episodes
                .iter()
                .find(|candidate| candidate.number == episode)
                .map(|candidate| candidate.name.clone())
                .unwrap_or_default();
            EpisodeAssignment { episode, name }
        })
        .collect()
}

/// Fetch a TV season's episode list from TMDB (`GET /3/tv/{id}/season/{n}`).
///
/// Uses the same shared timeout-bounded `AGENT`, size-capped JSON read, and
/// no-redirect policy as `fetch_multi`. Returns an empty vec on ANY failure
/// (bad id, no season, network, non-JSON) — the TV auto-naming path degrades to
/// plain sequential numbering rather than blocking.
fn season_episodes_with(
    tv_id: u64,
    season: u16,
    api_key: &str,
    sink: &dyn crate::Sink,
) -> Vec<Episode> {
    if api_key.is_empty() || tv_id == 0 {
        return Vec::new();
    }
    let url = format!(
        "{}/3/tv/{tv_id}/season/{season}?api_key={}",
        api_base(),
        urlencoded(api_key)
    );
    fetch_url(&url, &format!("TV {tv_id} season {season}"), sink)
        .map(|json| parse_episodes(&json))
        .unwrap_or_default()
}

/// Parse the `episodes` array of a `/tv/{id}/season/{n}` response.
fn parse_episodes(json: &serde_json::Value) -> Vec<Episode> {
    let Some(arr) = json["episodes"].as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let number = u16::try_from(e["episode_number"].as_u64()?).ok()?;
            Some(Episode {
                number,
                name: e["name"].as_str().unwrap_or("").to_string(),
                runtime_min: e["runtime"]
                    .as_u64()
                    .and_then(|r| u16::try_from(r).ok())
                    .unwrap_or(0),
            })
        })
        .collect()
}

/// Assign ripped titles to episodes for `Show S{NN}E{MM}` naming.
///
/// The i-th selected title (in disc order) maps to episode `start + i`. When a
/// TMDB episode of that number exists AND its runtime is plausibly the same as
/// the ripped title's, its name is attached; otherwise the assignment is still
/// numbered but nameless (degraded). `start` defaults to 1 at the call site.
///
/// Order-based by design: disc title order is broadcast order in practice, and
/// the runtime check flags gross violations without over-fitting. `title_secs`
/// is each selected title's duration in seconds, in disc order.
pub fn map_episodes(
    title_secs: &[f64],
    episodes: &[Episode],
    start: u16,
) -> Vec<EpisodeAssignment> {
    title_secs
        .iter()
        .enumerate()
        .map(|(i, &secs)| {
            let episode = start.saturating_add(i as u16);
            let name = episodes
                .iter()
                .find(|e| e.number == episode)
                .filter(|e| runtime_plausible(secs, e.runtime_min))
                .map(|e| e.name.clone())
                .unwrap_or_default();
            EpisodeAssignment { episode, name }
        })
        .collect()
}

// Is a ripped title's `secs` runtime plausibly the TMDB episode's `ep_min`
// minutes? Unknown episode runtime (0) never rejects. Tolerance is the larger
// of 5 minutes and 25% (ad breaks, PAL speed-up routinely shift runtimes).
fn runtime_plausible(secs: f64, ep_min: u16) -> bool {
    if ep_min == 0 {
        return true;
    }
    let title_min = secs / 60.0;
    let tol = (ep_min as f64 * 0.25).max(5.0);
    (title_min - ep_min as f64).abs() <= tol
}

// A better offset must beat the fallback's fit by this many minutes of average runtime
// distance. TMDB runtimes are whole minutes and crowd-sourced, so less is noise.
const ALIGN_MIN_GAIN_MIN: f64 = 1.0;

/// Choose the starting episode number for a disc by aligning its title runtimes against the
/// TMDB season's episode runtimes (instead of counting an offset), so a disc with any
/// distinctively-timed episode is pinned to its true position regardless of how earlier discs
/// split. Returns `fallback` on any absence of signal (no episodes/runtimes, a disc that can't
/// fit, a tie, or no offset clearly better than the fallback).
pub fn align_disc_offset(title_secs: &[f64], episodes: &[Episode], fallback: u16) -> u16 {
    let count = title_secs.len();
    if count == 0 || episodes.is_empty() {
        return fallback;
    }
    let Ok(count) = u16::try_from(count) else {
        return fallback;
    };
    // The disc must fit inside the season's episode numbering.
    let min_ep = episodes.iter().map(|e| e.number).min().unwrap_or(1).max(1);
    let max_ep = episodes.iter().map(|e| e.number).max().unwrap_or(0);
    let Some(last_start) = max_ep.checked_sub(count - 1) else {
        return fallback;
    };
    if last_start < min_ep {
        return fallback;
    }
    let mut runtimes = std::collections::HashMap::with_capacity(episodes.len());
    for episode in episodes {
        #[cfg(test)]
        low_metadata_tests::count_runtime_probe();
        runtimes
            .entry(episode.number)
            .or_insert(episode.runtime_min);
    }
    let runtime_of = |number: u16| {
        #[cfg(test)]
        low_metadata_tests::count_runtime_probe();
        runtimes.get(&number).copied()
    };

    // Require a unique minimum over comparable pairs; unknown runtimes add no signal.
    let mut best_start = fallback;
    let mut best_avg = f64::INFINITY;
    let mut best_signal = 0u32;
    let mut best_tied = false;
    let mut fallback_avg: Option<f64> = None;
    for start in min_ep..=last_start {
        let mut dist = 0.0f64;
        let mut signal = 0u32;
        for (i, &secs) in title_secs.iter().enumerate() {
            if secs <= 0.0 {
                continue;
            }
            if let Some(ep_min) = runtime_of(start + i as u16)
                && ep_min > 0
            {
                dist += (secs / 60.0 - ep_min as f64).abs();
                signal += 1;
            }
        }
        if signal == 0 {
            continue;
        }
        let avg = dist / signal as f64;
        if start == fallback {
            fallback_avg = Some(avg);
        }
        if best_signal == 0 || avg < best_avg - 1e-9 {
            best_start = start;
            best_avg = avg;
            best_signal = signal;
            best_tied = false;
        } else if (avg - best_avg).abs() <= 1e-9 {
            best_tied = true;
        }
    }
    if best_signal == 0 || best_tied {
        return fallback;
    }
    if fallback_avg.is_some_and(|fa| fa - best_avg < ALIGN_MIN_GAIN_MIN) {
        return fallback;
    }
    best_start
}

#[cfg(test)]
#[path = "tmdb_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "year_identity_tests.rs"]
mod year_identity_tests;

#[cfg(test)]
#[path = "low_metadata_tests.rs"]
mod low_metadata_tests;
