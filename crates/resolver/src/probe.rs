//! Checking guesses against reality.
//!
//! Hostname synthesis deliberately over-generates — hundreds of candidates
//! for one query. This module is what makes that affordable, and it works in
//! two stages of increasing cost:
//!
//!   1. **DNS.** "Does this name exist?" costs a UDP round trip, no page
//!      fetch, no robots.txt, no crawl budget. Hundreds of these run in
//!      parallel in well under a second, and they eliminate almost
//!      everything: invented domains overwhelmingly do not resolve.
//!
//!   2. **Fetch.** Only survivors get retrieved, and retrieval is where a
//!      guess is *confirmed* rather than merely permitted. This step is not
//!      optional politeness — it is load-bearing for precision, because of
//!      parked domains.
//!
//! Parking is the failure mode that makes stage 1 insufficient on its own.
//! A large share of plausible-sounding domains are registered and resolve
//! perfectly while serving nothing: this project's own crawl opens with
//! `0068.in - Domain For Sale`. Answering a navigational query with a parked
//! page is worse than answering with nothing, because the user believes it.
//! So `signals::quality::quality_score` — already written for exactly this
//! problem on the indexing side — gates every candidate here too.

use crate::query::{Kind, Token};
use futures::stream::StreamExt;
use std::time::Duration;

/// Identify honestly. This is the same string the crawler uses; a resolver
/// probe is the same kind of traffic and should be as attributable.
const USER_AGENT: &str =
    "rust-search-engine-bot/0.1 (+contact: apaarmeet5000@gmail.com)";

/// Parallel DNS lookups in flight.
///
/// getaddrinfo runs on a blocking pool, so this is bounded by threads rather
/// than sockets, and tokio's default pool holds 512. Sized to clear a full
/// candidate list in roughly two waves instead of five: at 64, a 300-host
/// query spent 6.7 s in DNS alone, almost all of it waiting on NXDOMAIN
/// responses that cost nothing but latency.
const DNS_CONCURRENCY: usize = 192;

/// Parallel page fetches in flight. Small by design: every survivor is a
/// different host, so this is total load, not per-host load.
const FETCH_CONCURRENCY: usize = 12;

/// A guess that failed here cost the user latency for nothing, so these are
/// deliberately tighter than a crawler's.
const DNS_TIMEOUT: Duration = Duration::from_secs(3);

/// Second-chance budget for lookups that timed out rather than answered.
const DNS_RETRY_TIMEOUT: Duration = Duration::from_secs(6);

/// How far down the candidate list a timed-out lookup is still worth
/// retrying. Beyond this the guess is too weak to win even if it resolves.
const RETRY_PRIOR_CUTOFF: usize = 60;
/// Raised from five seconds once results were cached.
///
/// A tight timeout trades correctness for latency on every request, which is
/// the wrong trade when the answer is computed once and reused: dropping a
/// slow-but-correct site meant the *cached* answer was the wrong one, frozen
/// in place. Better to spend a few extra seconds once.
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

/// Separate from the overall timeout so a host that is merely slow to
/// connect fails fast, while one that connects and streams a large sitemap
/// gets the full window.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Most HTML retained per page for the highlighter.
const MAX_RETAINED_HTML: usize = 2_000_000;

/// Below this, a page is parked, thin, or a placeholder. Answering a
/// navigational query with one of those is worse than returning nothing.
const MIN_QUALITY: f32 = 0.35;

pub fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(FETCH_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?)
}

/// Outcome of one lookup. The distinction between the last two is the point:
/// "this name does not exist" is an answer, "I did not hear back in time" is
/// not, and collapsing them into one boolean loses real hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    Live,
    Absent,
    TimedOut,
}

/// Keep only the hostnames that exist.
///
/// Timed-out lookups are retried once, with a longer budget, because a
/// timeout is not evidence of absence. Measured before this existed: the
/// same query reported 33 live hosts on one run and 13 on the next, and the
/// correct answer disappeared entirely on the bad run — not because DNS was
/// unreliable (300 lookups resolve identically at every concurrency level
/// when measured directly) but because a cold cache made some legitimate
/// lookups exceed a three-second budget and be recorded as nonexistent.
/// A retry wave is cheap precisely because real timeouts are rare.
pub async fn live_hosts(hosts: Vec<String>) -> Vec<String> {
    let first = lookup_all(hosts, DNS_TIMEOUT).await;

    let mut live: Vec<String> = first
        .iter()
        .filter(|(_, r)| *r == Lookup::Live)
        .map(|(h, _)| h.clone())
        .collect();

    // Retry only the candidates good enough to change the answer.
    //
    // The input is in prior order, so a timeout at position 250 is a guess
    // that was never going to win even if it resolved — waiting a second
    // six-second round for it is pure latency. Restricting the retry wave to
    // the head of the list keeps the correctness the retry was added for
    // while cutting the worst case roughly in half.
    let slow: Vec<String> = first
        .into_iter()
        .enumerate()
        .filter(|(i, (_, r))| *r == Lookup::TimedOut && *i < RETRY_PRIOR_CUTOFF)
        .map(|(_, (h, _))| h)
        .collect();

    if !slow.is_empty() {
        tracing::debug!("retrying {} timed-out lookups", slow.len());
        live.extend(
            lookup_all(slow, DNS_RETRY_TIMEOUT)
                .await
                .into_iter()
                .filter(|(_, r)| *r == Lookup::Live)
                .map(|(h, _)| h),
        );
    }

    live.sort();
    live.dedup();
    live
}

async fn lookup_all(hosts: Vec<String>, timeout: Duration) -> Vec<(String, Lookup)> {
    futures::stream::iter(hosts.into_iter().map(|host| async move {
        let outcome = resolves(&host, timeout).await;
        (host, outcome)
    }))
    .buffer_unordered(DNS_CONCURRENCY)
    .collect::<Vec<_>>()
    .await
}

/// Label used to test for a wildcard DNS record. Must be something nobody
/// would ever register; it is only ever asked about, never fetched.
const WILDCARD_SENTINEL: &str = "zq7x2v9nonexistent";

/// Which of these domains answer DNS for *any* subdomain.
///
/// Parking services and some hosts publish a wildcard record, so every
/// subdomain of them "exists". That turns the subdomain wave from evidence
/// into noise, and expensively: asked for "govorit moskva radio", the
/// throwaway `govoritmoskva.com` resolved, and `forum.`, `forums.`,
/// `community.`, `blog.` and `news.` of it all resolved too — six phantom
/// hosts that consumed the entire fetch budget while the real answer,
/// `govoritmoskva.ru`, was never retrieved.
///
/// One extra lookup per base domain buys the whole distinction.
pub async fn wildcard_domains(bases: &[String]) -> std::collections::HashSet<String> {
    let checks = bases.iter().map(|base| async move {
        let probe = format!("{WILDCARD_SENTINEL}.{base}");
        (base.clone(), resolves(&probe, DNS_TIMEOUT).await == Lookup::Live)
    });
    futures::future::join_all(checks)
        .await
        .into_iter()
        .filter(|(_, wildcard)| *wildcard)
        .map(|(base, _)| base)
        .collect()
}

async fn resolves(host: &str, timeout: Duration) -> Lookup {
    let target = format!("{host}:443");
    match tokio::time::timeout(timeout, tokio::net::lookup_host(target)).await {
        Ok(Ok(mut addrs)) => {
            if addrs.next().is_some() {
                Lookup::Live
            } else {
                Lookup::Absent
            }
        }
        Ok(Err(_)) => Lookup::Absent,
        Err(_) => Lookup::TimedOut,
    }
}

/// A page that was actually retrieved.
#[derive(Debug, Clone)]
pub struct Fetched {
    /// Where we ended up, after redirects. This is the URL to show — a guess
    /// at `stedmunds.in` that 301s to `stedmundshillong.in` should surface
    /// the destination, not the guess.
    pub url: String,
    pub title: String,
    pub body: String,
    pub quality: f32,
    /// Raw HTML, kept so the highlighter can find links and buttons.
    ///
    /// Worth the memory: the page has already been downloaded to verify it,
    /// so locating the control the user asked about costs nothing further.
    /// Throwing the markup away and keeping only the text would mean
    /// fetching every page twice.
    pub html: String,
}

/// Fetch one host or URL, trying HTTPS first.
///
/// The HTTP fallback exists because a meaningful slice of exactly the sites
/// this feature targets — small schools, local institutions, regional
/// stations — still have no working TLS. Refusing to fall back would fail
/// them on a technicality after the hard part already succeeded.
pub async fn fetch(client: &reqwest::Client, host_or_url: &str) -> Option<Fetched> {
    let urls: Vec<String> = if let Some(rest) = host_or_url.strip_prefix("http://") {
        // An explicit `http://` URL gets its HTTPS form tried first.
        //
        // Recorded sources are full of stale plain-HTTP URLs — Wikidata
        // stores Chandigarh University as `http://www.cuchd.in` and IISER
        // Mohali as `http://www.iisermohali.ac.in`. The first 301s to HTTPS;
        // the second refuses the connection outright. Both sites are alive
        // and well on HTTPS, and both were being recorded as fetch failures,
        // which silently dropped the one correct answer the directory had
        // supplied.
        vec![format!("https://{rest}"), host_or_url.to_string()]
    } else if host_or_url.starts_with("https://") {
        vec![host_or_url.to_string()]
    } else {
        vec![format!("https://{host_or_url}/"), format!("http://{host_or_url}/")]
    };

    for url in urls {
        let resp = match client.get(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!("fetch error {url}: {e}");
                // Only fall through to the HTTP attempt when HTTPS failed to
                // *connect*. A timeout means the host answered slowly, and
                // retrying it on port 80 just spends the budget twice on the
                // same unresponsive server.
                if e.is_timeout() {
                    break;
                }
                continue;
            }
        };
        if !resp.status().is_success() {
            // Worth logging rather than swallowing: a 403 here is a site
            // refusing our user agent, which is a different problem from a
            // domain that does not exist, and only one of them is fixable.
            tracing::debug!("fetch {url}: HTTP {}", resp.status());
            continue;
        }
        let final_url = resp.url().to_string();
        let Ok(html) = resp.text().await else { continue };
        let (title, body) = extract(&html);
        let mut quality = signals::quality::quality_score(&title, &body);
        if title_is_just_the_domain(&title, &final_url) {
            quality = 0.0;
        }
        // Cap what is retained: a handful of pathological pages are tens of
        // megabytes, and everything the highlighter needs is markup near the
        // top of the document in practice.
        let html = html.chars().take(MAX_RETAINED_HTML).collect();
        return Some(Fetched { url: final_url, title, body, quality, html });
    }
    None
}

/// Fetch many, dropping the ones that are not real pages.
pub async fn fetch_all(client: &reqwest::Client, targets: Vec<String>) -> Vec<Fetched> {
    futures::stream::iter(targets.into_iter().map(|t| {
        let client = client.clone();
        async move { fetch(&client, &t).await }
    }))
    .buffer_unordered(FETCH_CONCURRENCY)
    .filter_map(|f| async move {
        match f {
            Some(p) if p.quality >= MIN_QUALITY => Some(p),
            Some(p) => {
                tracing::debug!("rejected {} (quality {:.2})", p.url, p.quality);
                None
            }
            None => None,
        }
    })
    .collect()
    .await
}

/// Is the page's title nothing but its own domain name?
///
/// A structural companion to the phrase list in `signals::quality`, which
/// will always be one parking-page wording behind. Real pages are titled
/// after what they contain; a page titled only after the asset being sold is
/// selling the asset.
///
/// This matters more to the resolver than to the crawler, because of how the
/// two failures interact: a guessed hostname is *built from the query's
/// words*, so a page echoing its own hostname back scores a perfect content
/// match on those words. `govorit-moskva.ru` scored 1.00 that way.
///
/// Deliberately narrow. The tempting version — "title starts with the
/// hostname" — also catches real sites that brand themselves by domain and
/// add a tagline, and rejecting a genuine answer is the error this whole
/// module is arranged to avoid. So the decoration allowance is small enough
/// to admit "Example.com" and "Example.com - Home" and nothing longer;
/// wordier parking pages are the phrase list's job.
fn title_is_just_the_domain(title: &str, url: &str) -> bool {
    const MAX_DECORATION: usize = 12;

    let host = host_of(url);
    let host = host.trim_start_matches("www.");
    if host.is_empty() || title.trim().is_empty() {
        return false;
    }
    let bare: String = host.chars().filter(|c| c.is_alphanumeric()).collect();
    let title_bare: String = title
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    title_bare.starts_with(&bare) && title_bare.len() <= bare.len() + MAX_DECORATION
}

/// Title and visible text from an HTML document.
fn extract(html: &str) -> (String, String) {
    let doc = scraper::Html::parse_document(html);
    let title = scraper::Selector::parse("title")
        .ok()
        .and_then(|sel| doc.select(&sel).next().map(|e| e.text().collect::<String>()))
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    // Script and style subtrees are skipped explicitly.
    //
    // `ElementRef::text()` walks all descendant text nodes, and per the HTML
    // spec the contents of <script> and <style> *are* text nodes — so the
    // naive version returns minified JavaScript as page content. That
    // pollutes every text comparison downstream, and it makes the
    // highlighter match variable names.
    let body = scraper::Selector::parse("body")
        .ok()
        .and_then(|sel| doc.select(&sel).next())
        .map(|el| {
            el.descendants()
                .filter_map(|node| {
                    // Skip a text node if any ancestor is script/style.
                    let text = node.value().as_text()?;
                    let mut parent = node.parent();
                    while let Some(p) = parent {
                        if let Some(e) = p.value().as_element() {
                            if matches!(e.name(), "script" | "style" | "noscript") {
                                return None;
                            }
                        }
                        parent = p.parent();
                    }
                    Some(text.to_string())
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    (title, body)
}

/// How well a retrieved page answers the query, 0.0 .. 1.0.
///
/// The question this answers is narrow: *is this the thing they asked for?*
/// — not "is this a good page", which quality already covers. So it is
/// dominated by whether the identifying words appear in the title and the
/// hostname, the two places a site states what it is.
///
/// Body text is worth much less here and is capped accordingly. A school's
/// homepage mentions "Shillong" fifty times; so does every other page in
/// Shillong. Weighting body matches highly is how a navigational search
/// starts returning the local newspaper instead of the school.
pub fn match_score(tokens: &[Token], f: &Fetched) -> f32 {
    let identifying: Vec<&Token> = tokens
        .iter()
        .filter(|t| matches!(t.kind, Kind::Distinctive | Kind::Place))
        .collect();
    if identifying.is_empty() {
        return 0.0;
    }

    let title_words = word_set(&f.title);
    let host = host_of(&f.url);
    // A generous window, not a snippet.
    //
    // At 2,000 characters this term was noise rather than signal: whether a
    // university's own homepage happened to print its city name inside the
    // first two thousand characters decided a 0.1 swing, and that swing was
    // enough to rank `culko.in` (Chandigarh University's Unnao campus) above
    // `cuchd.in` (the university itself) for the query "chandigarh
    // university". Body text is weighted low precisely so it can be read
    // broadly without dominating.
    let body_head: String = f.body.chars().take(8000).collect();
    let body_words = word_set(&body_head);

    let mut title_hits = 0usize;
    let mut host_hits = 0usize;
    let mut body_hits = 0usize;
    for t in &identifying {
        if t.forms.iter().any(|form| title_words.contains(form.as_str())) {
            title_hits += 1;
        }
        // Substring, not word match: hostnames are concatenated, so
        // "edmund" has to be found inside "stedmundshillong".
        if t.forms.iter().any(|form| host.contains(form.as_str())) {
            host_hits += 1;
        }
        if t.forms.iter().any(|form| body_words.contains(form.as_str())) {
            body_hits += 1;
        }
    }

    let n = identifying.len() as f32;
    let title = title_hits as f32 / n;
    let host_frac = host_hits as f32 / n;
    let body = body_hits as f32 / n;

    // The hostname is a *bonus*, not a required share of the score.
    //
    // Averaging it in penalises every site whose domain is not derivable
    // from its name — which is the entire class the OSM directory exists to
    // cover. Asked for "panjab university": the university's own site,
    // `puchd.ac.in`, scored 0.65 because its hostname contains no query word,
    // while `panjab.com` — a charity called Panjab Relief — scored 1.00 for
    // having "panjab" in the domain. The guessing path is what benefits from
    // a matching hostname, and it should not be able to outvote the page's
    // own statement of what it is.
    // Body stays deliberately cheap. Rebalancing toward it broke the case
    // this weighting exists for: the Shillong local newspaper mentions
    // "St. Edmund's School" and "Shillong" in its text as often as the
    // school does, and at a 0.2 body weight it scored 0.55 — above the
    // confidence floor, as an answer to "saint edmunds school shillong".
    // What a page *is* lives in its title; what it merely mentions lives in
    // its body, and navigational search cares only about the former.
    let base = (0.85 * title + 0.1 * body + 0.25 * host_frac).min(1.0);
    let place = place_agreement(tokens, &title_words, &host, &body_words);
    let kind = kind_agreement(tokens, &title_words, &host, &body_words);
    (base * place * kind).clamp(0.0, 1.0)
}

/// Does the page look like the *kind* of thing that was asked for?
///
/// Category words ("university", "school", "forum") are excluded from the
/// identity score on purpose — they pick out no particular entity, and
/// letting them count would make every school in the country match "school".
/// But excluding them entirely is its own failure: asked for "panjab
/// university", the only identifying word is "panjab", so `panjab.com` —
/// titled "Panjab Relief", a charity — scored a *perfect* 1.00, exactly as
/// well as the university's own site.
///
/// So they return as a modifier rather than a component. Not enough to
/// promote a page on their own, enough to separate two pages that are
/// otherwise tied on identity. The floor is deliberately high: plenty of
/// legitimate sites never print their own category word.
fn kind_agreement(
    tokens: &[Token],
    title_words: &std::collections::HashSet<String>,
    host: &str,
    body_words: &std::collections::HashSet<String>,
) -> f32 {
    const FLOOR: f32 = 0.7;

    let generics: Vec<&Token> =
        tokens.iter().filter(|t| t.kind == Kind::Generic).collect();
    if generics.is_empty() {
        return 1.0;
    }
    let hits = generics
        .iter()
        .filter(|t| {
            t.forms.iter().any(|form| {
                title_words.contains(form.as_str())
                    || host.contains(form.as_str())
                    || body_words.contains(form.as_str())
            })
        })
        .count();
    FLOOR + (1.0 - FLOOR) * (hits as f32 / generics.len() as f32)
}

/// Penalty for a page that matches the name but not the *place*.
///
/// A named place in a navigational query is close to a hard constraint, and
/// the failure it prevents is specific. "saint edmunds school shillong"
/// returned `saintedmunds.org` — an Episcopal church in San Marino,
/// California — scoring 0.67, because "saint" and "edmunds" matched the
/// title and the hostname perfectly. They genuinely do. It is the wrong
/// continent, and the only token that says so is "shillong".
///
/// Institution names repeat endlessly across the world; the place is what
/// disambiguates them, which is exactly why the site owner put it in the
/// domain. Absence of the place is therefore strong evidence against, not
/// merely weak evidence for — so this multiplies rather than adds.
fn place_agreement(
    tokens: &[Token],
    title_words: &std::collections::HashSet<String>,
    host: &str,
    body_words: &std::collections::HashSet<String>,
) -> f32 {
    let places: Vec<&Token> = tokens.iter().filter(|t| t.kind == Kind::Place).collect();
    if places.is_empty() {
        return 1.0;
    }
    // Every named place must be corroborated, not merely one of them.
    //
    // `any` looks equivalent when a query names a single place, and is wrong
    // the moment one of the words is both a brand and a place. "sherwood
    // college nainital": OSM carries a settlement called Sherwood, so
    // *sherwood* is a place token — and `sherwoodcompanies.com`, a civil
    // construction firm, satisfied the place check by containing the word
    // "Sherwood" in its own name. The query's actual location, Nainital, was
    // never required to appear anywhere, and a construction company in the
    // wrong country was returned as a school.
    //
    // The strict reading is also the user's: naming Nainital is a constraint,
    // not a hint. The cost is queries naming two places where a page mentions
    // only one ("st edmunds shillong meghalaya"), which are penalised despite
    // being right — rare enough, and a silent result, to be the better trade.
    let all_mentioned = places.iter().all(|t| {
        t.forms.iter().any(|form| {
            title_words.contains(form.as_str())
                || host.contains(form.as_str())
                || body_words.contains(form.as_str())
        })
    });
    if all_mentioned {
        1.0
    } else {
        0.25
    }
}

/// Normalise text to comparable tokens, the same way queries are normalised
/// — apostrophes dropped, so the title "St. Edmund's School" yields
/// `edmunds` and matches a query for "edmunds".
fn word_set(text: &str) -> std::collections::HashSet<String> {
    text.to_lowercase()
        .replace('\'', "")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect()
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query;

    fn places() -> impl Fn(&str) -> bool {
        |w: &str| matches!(w, "shillong" | "moscow")
    }

    fn page(url: &str, title: &str, body: &str) -> Fetched {
        Fetched {
            url: url.into(),
            title: title.into(),
            body: body.into(),
            quality: 1.0,
            html: String::new(),
        }
    }

    fn score(q: &str, p: &Fetched) -> f32 {
        match_score(&query::parse(q, &places()).tokens, p)
    }

    /// The real page, against the real query. The apostrophe in the title
    /// and "saint" vs "st" in the query both have to survive normalisation.
    #[test]
    fn the_real_page_scores_highly() {
        let p = page(
            "https://stedmundshillong.in/",
            "St. Edmund's School, Shillong",
            "Attendance, schedule, academic records, syllabus.",
        );
        assert!(score("saint edmunds school shillong", &p) > 0.85, "{}", score("saint edmunds school shillong", &p));
    }

    /// A different school in the same city must not win on the city alone.
    /// This is the failure that makes body text dangerous to weight.
    #[test]
    fn another_site_in_the_same_city_scores_poorly() {
        let p = page(
            "https://shillongtimes.com/",
            "The Shillong Times",
            "News from Shillong. St. Edmund's School wins the match. Shillong Shillong.",
        );
        let s = score("saint edmunds school shillong", &p);
        assert!(s < 0.5, "local newspaper scored {s}, should not look like the school");
    }

    /// The exact wrong answer this scorer was built to reject: a real
    /// St. Edmund's, perfect name match, wrong continent.
    #[test]
    fn a_same_named_institution_elsewhere_is_rejected_on_the_place() {
        let p = page(
            "https://saintedmunds.org/",
            "St. Edmund's Episcopal Church - San Marino, California",
            "A parish church in the Pasadena area.",
        );
        let s = score("saint edmunds school shillong", &p);
        assert!(s < crate::MIN_CONFIDENCE, "California church scored {s}");
    }

    /// The "sherwood college nainital" failure: one query word is both the
    /// institution's name and, in OSM, a settlement. A company sharing that
    /// name must not satisfy the location constraint on its own.
    #[test]
    fn a_brand_word_that_is_also_a_place_does_not_satisfy_the_location() {
        let tokens = vec![
            Token {
                text: "sherwood".into(),
                forms: vec!["sherwood".into()],
                kind: Kind::Place,
            },
            Token {
                text: "nainital".into(),
                forms: vec!["nainital".into()],
                kind: Kind::Place,
            },
        ];
        let p = page(
            "https://sherwoodcompanies.com/",
            "Sherwood Companies - An Industry Leader In Civil Construction",
            "Civil construction across the midwest.",
        );
        let s = match_score(&tokens, &p);
        assert!(s < crate::MIN_CONFIDENCE, "construction firm scored {s}");
    }

    /// The "panjab university" failure, which took two fixes: the category
    /// word had to start counting, *and* the hostname had to stop being
    /// worth a third of the score. `puchd.ac.in` contains no query word at
    /// all and is still the right answer.
    #[test]
    fn the_category_word_separates_two_pages_tied_on_identity() {
        let charity = page(
            "https://www.panjab.com/",
            "Panjab Relief",
            "Charitable relief work.",
        );
        let uni = page(
            "https://puchd.ac.in/",
            "Official Website of Panjab University Chandigarh",
            "Panjab University, Chandigarh.",
        );
        let q = "panjab university";
        assert!(
            score(q, &uni) > score(q, &charity),
            "university {} vs charity {}",
            score(q, &uni),
            score(q, &charity)
        );
    }

    /// ...but the penalty must not fire when no place was asked for.
    #[test]
    fn queries_without_a_place_are_not_penalised() {
        let p = page(
            "https://forum.valuepickr.com/",
            "ValuePickr Forum",
            "Separating the wheat from the chaff.",
        );
        assert!(score("valuepickr", &p) > 0.8);
    }

    /// The hostname is real evidence — but not enough on its own.
    ///
    /// A page with no title and no text, at a perfectly-matching domain,
    /// scores well clear of zero and still lands below `MIN_CONFIDENCE`.
    /// That is the intended shape: the domain narrows the field, the page
    /// has to confirm it, and an empty page confirms nothing.
    #[test]
    fn hostname_evidence_counts_but_does_not_carry_a_blank_page() {
        let p = page("https://stedmundshillong.in/", "", "");
        let s = score("saint edmunds school shillong", &p);
        assert!(s > 0.1, "hostname evidence should count, got {s}");
        assert!(s < crate::MIN_CONFIDENCE, "a blank page should not be an answer, got {s}");
    }

    #[test]
    fn an_unrelated_page_scores_near_zero() {
        let p = page("https://example.com/", "Example Domain", "Nothing here.");
        assert!(score("saint edmunds school shillong", &p) < 0.1);
    }

    /// Parking is caught by quality, not by match — a parked page at a
    /// perfectly-matching domain still matches the words.
    #[test]
    fn parked_pages_are_rejected_on_quality_not_relevance() {
        let title = "stedmundshillong.in - Domain For Sale";
        let q = signals::quality::quality_score(title, "This premium domain is available");
        assert!(q < MIN_QUALITY, "parked page scored {q}");
    }

    #[test]
    fn a_page_titled_only_after_its_own_domain_is_parked() {
        assert!(title_is_just_the_domain(
            "GOVORIT-MOSKVA.RU",
            "http://govorit-moskva.ru/en/"
        ));
        assert!(title_is_just_the_domain("Example.com - Home", "https://example.com/"));
    }

    /// The Moscow failure end to end: a squatted domain built from the
    /// query's own words, which therefore scores a *perfect* content match.
    /// Relevance cannot reject it; quality has to.
    #[test]
    fn the_squatted_moscow_domain_is_rejected_on_quality() {
        let title = "GOVORIT-MOSKVA.RU. Domain name is available";
        let p = page("http://govorit-moskva.ru/en/", title, "Buy this domain.");
        let s = score("govorit moskva radio", &p);
        assert!(
            s > 0.6,
            "relevance alone cannot tell a squatted domain from the real \
             thing; it scored {s}"
        );
        let q = signals::quality::quality_score(title, "Buy this domain.");
        assert!(q < MIN_QUALITY, "parked page scored {q}");
    }

    /// A site branded by its own domain, with a real tagline, must survive.
    #[test]
    fn a_branded_domain_title_with_a_tagline_is_not_parked() {
        assert!(!title_is_just_the_domain(
            "ValuePickr.com - Separating the Wheat from the Chaff",
            "https://valuepickr.com/"
        ));
    }

    #[test]
    fn a_real_page_is_not_flagged_by_the_domain_title_rule() {
        assert!(!title_is_just_the_domain(
            "St. Edmund's School, Shillong",
            "https://stedmundshillong.in/"
        ));
        // Even when the site is named after its domain, real titles say more.
        assert!(!title_is_just_the_domain(
            "ValuePickr Forum - Separating the Wheat from the Chaff",
            "https://forum.valuepickr.com/"
        ));
    }

    #[test]
    fn queries_with_no_identifying_words_score_zero() {
        let p = page("https://x.com/", "Official Website", "Official website");
        assert_eq!(score("official website", &p), 0.0);
    }
}
