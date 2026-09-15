//! Navigational search: given words, find the exact URL.
//!
//! A different problem from the web index next door. The index answers
//! "which of the pages I have seen best matches these words?" — which is
//! useless when the target was never crawled, and the long tail of the web
//! is by definition never crawled. Vinay's two examples are both in that
//! tail: `forum.valuepickr.com/t/bajaj-finance-limited/267` and
//! `stedmundshillong.in` are not in a 5,000-page corpus and never will be.
//!
//! So this resolves rather than retrieves. The pipeline mirrors how a person
//! finds a half-remembered site:
//!
//! ```text
//!   "saint edmunds school shillong"
//!      |
//!      1. read it          -> forms (saint|st, edmunds|edmund), site/page splits
//!      2. guess the site   -> stedmundshillong.in, stedmunds.in, ... (hundreds)
//!      3. check cheaply    -> DNS: which of those actually exist?  (a handful)
//!      4. find the page    -> the site's own sitemap, matched against "bajaj finance"
//!      5. confirm          -> fetch it; does the title really say so?
//! ```
//!
//! Steps 2 and 3 are the load-bearing pair. Guessing is only useful because
//! verifying is cheap, and verifying is only cheap because DNS answers
//! "does this exist?" without fetching anything.

pub mod highlight;
pub mod llm;
pub mod hostname;
pub mod probe;
pub mod query;
pub mod site;
pub mod websearch;
pub mod wikidata;
pub mod tld;

use places::gazetteer::Gazetteer;

/// Readings of the query to pursue.
///
/// `splits` emits them best-first, and the tail is where a site name has
/// been carved out of the middle of a phrase — readings that are legal but
/// rarely what anyone meant. Each one costs a full candidate set.
const MAX_SPLITS: usize = 6;

/// Hostnames sent to DNS for one query, across all splits.
const MAX_PROBES: usize = 300;

/// Live hosts actually retrieved.
///
/// Raised from six when recorded sources were added: directory hits occupy
/// the head of the queue by design, and at six they left almost no room for
/// the guesses that cover everything the directories do not know.
const MAX_FETCHES: usize = 16;

/// Control labels offered to the model. Enough to cover a dense settings
/// page, few enough to keep the prompt small and the choice focused.
const MAX_CONTROL_CANDIDATES: usize = 60;

/// Fetch slots held open for guessed hostnames, whatever the directories say.
const GUESS_RESERVE: usize = 6;

/// One URL the resolver is prepared to stand behind, with its reasoning.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Answer {
    pub url: String,
    pub title: String,
    /// Confidence, 0..1: how well the retrieved page matches the query.
    pub score: f32,
    /// Junk filter from the indexing side, carried through so a caller can
    /// see *why* a plausible-looking host was kept or dropped.
    pub quality: f32,
    /// Which reading of the query produced this, in words.
    pub via: String,
    /// Retrieved page text and markup, kept so a question can be answered
    /// and a control located. Not serialised — working state, not response.
    #[serde(skip)]
    pub body_text: String,
    #[serde(skip)]
    pub html: String,
    /// The control on the page matching the request, and a URL that opens
    /// the page with it highlighted. `None` when no control could be
    /// confirmed — the plain `url` is still a correct answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub highlight: Option<highlight::Highlight>,
}

/// What the resolver did, for `?explain=true` and for the eval harness.
///
/// Worth surfacing rather than logging: the funnel is the argument for this
/// design. "312 guessed, 4 exist, 1 confirmed" is the sentence that explains
/// why guessing is affordable.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Trace {
    pub guessed: usize,
    pub resolved: usize,
    pub fetched: usize,
    pub suffixes: Vec<String>,
    pub live_hosts: Vec<String>,
    /// URLs enumerated from site sitemaps, across all sites explored.
    pub sitemap_urls: usize,
    pub dns_ms: u64,
    pub fetch_ms: u64,
    /// Websites supplied by the directory rather than guessed.
    pub directory_hits: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Resolution {
    pub query: String,
    pub answers: Vec<Answer>,
    pub trace: Trace,
    /// A direct answer read off the top result's page, when the query asked
    /// a question and the page actually contained the answer.
    ///
    /// Separate from `answers` because it is a different kind of claim: those
    /// are places to go, this is a statement of fact, and it carries the
    /// source URL so the user can check it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<InlineAnswer>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InlineAnswer {
    pub text: String,
    pub source_url: String,
    pub source_title: String,
}

/// Subdomains to try once a base domain is known to exist.
///
/// A second, tiny DNS wave rather than more candidates in the first. The
/// site a query means is often not at the apex: "valuepickr bajaj finance"
/// wants `forum.valuepickr.com`, and `valuepickr.com` is a WordPress site
/// that does not contain the thread at all. Folding these into wave one
/// would multiply every guess by twelve; doing it after wave one multiplies
/// only the handful that turned out to be real.
const SUBDOMAINS: &[&str] = &[
    "www", "forum", "forums", "community", "blog", "news", "shop", "store",
    "support", "help", "docs", "wiki", "app", "portal",
];

/// Live base domains that earn a subdomain wave.
const MAX_SUBDOMAIN_BASES: usize = 3;

/// Pages retrieved from one site's sitemap to confirm a sub-page answer.
const MAX_SUBPAGE_FETCHES: usize = 3;

/// A recorded entity -> website mapping, consulted before any guessing.
///
/// Hostname synthesis only reaches domains that can be *derived* from the
/// words. A great many organisations use a domain that cannot: Chandigarh
/// University is `cuchd.in`, Panjab University is `puchd.ac.in`. No spelling
/// rule produces those from the institution's name, and no amount of tuning
/// will — the information simply is not in the query.
///
/// It is in OpenStreetMap. POIs carry a `website` tag, and this project
/// already holds the whole Indian extract in the places index. So the
/// resolver asks a directory first and guesses second, which is also the
/// order a person would use.
///
/// Injected as a trait rather than imported, for the same reason `is_place`
/// is a closure: this crate stays testable without a 144 MB tantivy index,
/// and a second directory (Wikidata's `official website`, a company
/// register) slots in beside the first without touching the pipeline.
pub trait Directory: Send + Sync {
    /// Entities whose name matches the query, best first.
    fn lookup(&self, query: &str, limit: usize) -> Vec<DirectoryHit>;
}

#[derive(Debug, Clone)]
pub struct DirectoryHit {
    pub name: String,
    /// Absolute URL, as recorded.
    pub website: String,
    /// Which source said so, for the user-facing explanation. An answer is
    /// only as trustworthy as its provenance, and "OpenStreetMap lists this
    /// as the school's website" is a claim a person can evaluate.
    pub source: String,
}

/// Directory entries consulted per query, per source.
const MAX_DIRECTORY_HITS: usize = 4;

/// Results taken from a web index, which is the recall workhorse.
///
/// Higher than the directory limit on purpose: a knowledge base returns one
/// authoritative row per entity, while an index returns a *ranked list* whose
/// right answer is not always first. Taking more of that list is the cheapest
/// way to raise reach, and every extra candidate still has to survive
/// verification — fetched, checked for parking, scored — before it can be
/// shown. Borrowing recall does not mean borrowing the answer.
const MAX_WEB_HITS: usize = 8;

/// Site phrases sent to a remote directory, beyond the raw query. Each is a
/// network round trip, so this is latency spent on coverage.
const MAX_SITE_PHRASE_LOOKUPS: usize = 2;

/// Where recorded entity -> website facts come from.
///
/// Both are optional and independent. With neither, the resolver is pure
/// guess-and-verify and still answers most of the judgment set; each source
/// added closes a different hole. They are kept behind one struct so adding
/// a third (a commercial search API, a company register) does not change
/// `resolve`'s signature again.
#[derive(Default)]
pub struct Sources<'a> {
    /// Local, synchronous. In this deployment, the OSM places index.
    pub directory: Option<&'a dyn Directory>,
    /// Wikidata's official-website property. One network round trip pair.
    pub wikidata: bool,
    /// Language model for control selection and inline answers. `None`
    /// falls back to the heuristic matcher and no answers.
    pub llm: Option<llm::Llm>,
    /// A borrowed web index, if one is configured. `None` disables it.
    ///
    /// This is the one source that reaches deep pages no entity directory
    /// records — a course page inside a university site — which is the gap
    /// the self-contained sources cannot close.
    pub web: Option<websearch::Provider>,
}

/// Resolve a query to URLs.
///
/// Returns answers best-first, and an empty list rather than a weak guess
/// when nothing could be confirmed — a navigational answer the user cannot
/// trust is worse than none, because they will click it.
pub async fn resolve(
    client: &reqwest::Client,
    raw: &str,
    gaz: &Gazetteer,
    sources: &Sources<'_>,
) -> Resolution {
    let is_place = place_gate(gaz);
    let parsed = query::parse(raw, &is_place);
    let suffixes = tld::for_tokens(&parsed.tokens, gaz);
    let suffix_refs: Vec<&str> = suffixes.iter().map(|s| s.as_str()).collect();
    let mut trace = Trace { suffixes: suffixes.clone(), ..Default::default() };

    // ---- Generate ---------------------------------------------------------
    //
    // Across every reading, keeping the best position each host earned. A
    // host reachable from two readings is *more* likely, not less, so the
    // stronger reading's position survives.
    //
    // The key is (which reading, rank within that reading) — both halves, in
    // that order. Ranking on the within-reading position alone treats the
    // first candidate of every reading as equally good, which is how a probe
    // budget gets spent on `saint.com` and `shillong.com` (rank 0 of the
    // readings that take one stray word as the whole site name) while
    // `stedmundshillong.in` — rank 51 of the *correct* reading — never gets
    // checked at all. That was the observed failure: 300 hosts probed, 30
    // alive, and the right one not among them.
    // If any reading names a page, a directory hit for the site should still
    // look for that page inside it.
    // The most specific reading that names a page: the longest site half.
    //
    // The first such reading is the *shortest* site half, because splits are
    // emitted shortest-first — "chandigarh" rather than "chandigarh
    // university". A recorded hit is an entity match, so it belongs with the
    // reading that names the most of that entity.
    let page_reading: Option<query::Split> = query::splits(&parsed)
        .into_iter()
        .filter(|s| !s.page.is_empty())
        .max_by_key(|s| s.site.len());
    let page_words: Vec<String> =
        page_reading.as_ref().map(|s| s.page.clone()).unwrap_or_default();
    // Recorded hits inherit that reading whole, site half included.
    //
    // Giving them an empty site half looked harmless and silently disabled
    // sub-page search: a recorded hit *replaces* the guessed entry for the
    // same host, so when Wikidata also knew `valuepickr.com`, the entry that
    // carried "valuepickr" as the site name was thrown away. With no site
    // words there is nothing to score the site against, the confidence check
    // fails, the sitemap is never read, and "valuepickr bajaj finance"
    // collapsed from the exact thread back to the forum's front page.
    let recorded_split = page_reading.clone().unwrap_or_else(|| query::Split {
        site: parsed
            .tokens
            .iter()
            .filter(|t| t.kind != query::Kind::Stop)
            .map(|t| t.text.clone())
            .collect(),
        page: Vec::new(),
    });

    let readings: Vec<query::Split> =
        query::splits(&parsed).into_iter().take(MAX_SPLITS).collect();
    let quotas = probe_quotas(readings.len());

    let mut best: std::collections::HashMap<String, ((usize, usize), query::Split)> =
        std::collections::HashMap::new();
    for (split_idx, split) in readings.into_iter().enumerate() {
        for c in hostname::candidates(&split.site, &is_place, &suffix_refs)
            .into_iter()
            .take(quotas[split_idx])
        {
            let key = (split_idx, c.rank);
            let entry = best.entry(c.host).or_insert((key, split.clone()));
            if key < entry.0 {
                *entry = (key, split.clone());
            }
        }
    }

    let mut ordered: Vec<(String, (usize, usize), query::Split)> = best
        .into_iter()
        .map(|(host, (key, split))| (host, key, split))
        .collect();
    ordered.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    ordered.truncate(MAX_PROBES);

    // A recorded website is not a guess, so it goes to the front — but it is
    // still *verified* like everything else. OSM tags are crowd-sourced and
    // go stale; an entry pointing at a domain that has since lapsed into
    // parking must lose to a guess that actually serves the school's site.
    // Domains a directory named as belonging to the organisation in the
    // query, as opposed to merely mentioning it.
    let mut entity_domains: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut local_hits: Vec<DirectoryHit> = Vec::new();
    let mut remote_hits: Vec<DirectoryHit> = Vec::new();
    if let Some(dir) = sources.directory {
        local_hits.extend(dir.lookup(raw, MAX_DIRECTORY_HITS));
    }
    // A web index's ranking is already a relevance judgment over the whole
    // web, so its results join the remote candidates alongside Wikidata's.
    // They are not trusted more for having come from a search engine: each
    // is fetched, quality-checked and scored against the query like a guess.
    if let Some(provider) = &sources.web {
        remote_hits
            .extend(websearch::search(client, provider, raw, MAX_WEB_HITS).await);
    }

    if sources.wikidata {
        // Ask about the whole query, and — when a reading splits the query
        // into a site and a page — about the site half alone. "chandigarh
        // university bca" names no entity as a whole; "chandigarh
        // university" names one exactly.
        let mut whole_query_hits =
            wikidata::official_sites(client, raw, MAX_DIRECTORY_HITS).await;
        // P856 is, by definition, *the entity's official website*. That is
        // exactly the claim `own_site_bonus` needs, and not using it here
        // was the single biggest source of wrong answers once a web index
        // was added: for "jawaharlal nehru university delhi" the index
        // returns the Wikipedia article, whose title matches the query
        // word-for-word and therefore outscored `jnu.ac.in`. Four of five
        // wrong answers were an encyclopedia page beating the institution
        // itself. An article *about* an organisation is not the
        // organisation's website, and Wikidata already knows which is which.
        rank_by_name_match(&mut whole_query_hits, &parsed);
        if let Some(best) = whole_query_hits.first() {
            if let Some(d) = common::registrable_domain(&best.website) {
                entity_domains.insert(d);
            }
        }
        remote_hits.extend(whole_query_hits);
        // Every distinct site phrase, not just the first reading's.
        //
        // `splits` emits prefix readings shortest-first, so the first one
        // with a page part for "chandigarh university bca" is *chandigarh* /
        // "university bca" — and asking a directory about "chandigarh"
        // returns the city government. The reading that names the entity,
        // "chandigarh university" / "bca", is the next one along. Which
        // reading is right is not knowable here, which is the whole premise,
        // so ask about each and let verification sort them out.
        let mut asked: std::collections::HashSet<String> =
            [raw.to_lowercase()].into_iter().collect();
        let phrases: Vec<String> = query::splits(&parsed)
            .into_iter()
            .filter(|s| !s.page.is_empty())
            .map(|s| s.site.join(" "))
            .filter(|p| asked.insert(p.clone()))
            .take(MAX_SITE_PHRASE_LOOKUPS)
            .collect();
        for phrase in phrases {
            let mut hits =
                wikidata::official_sites(client, &phrase, MAX_DIRECTORY_HITS).await;

            // A hit on a *site phrase* states which domain the named
            // organisation owns — stronger information than a hit on the
            // whole query. Only the best-matching entity earns that status,
            // though, not every entity the search returned: asked about
            // "chandigarh university", Wikidata offers both *Chandigarh
            // University* (`cuchd.in`) and *Chandigarh University, Unnao*
            // (`culko.in`). Treating both as "the" entity domain handed the
            // Unnao campus's BCA page the same boost as the main one's, and
            // it won on having "bca" literally in its slug.
            let phrase_parsed = query::parse(&phrase, &is_place);
            rank_by_name_match(&mut hits, &phrase_parsed);
            if let Some(best) = hits.first() {
                if let Some(d) = common::registrable_domain(&best.website) {
                    entity_domains.insert(d);
                }
            }
            remote_hits.extend(hits);
        }
    }
    // Sources are interleaved, not concatenated.
    //
    // Concatenating lets whichever source ran first fill the fetch budget.
    // Asked for "chandigarh university", the OSM index returned four loosely
    // matching campuses and Wikidata returned `cuchd.in` — the correct
    // answer, fifth in line, outside the budget and never fetched. Neither
    // source is reliably better than the other, so neither gets to go first.
    let recorded: (Vec<DirectoryHit>, Vec<f32>) = {
        let mut merged: Vec<DirectoryHit> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut local = local_hits.into_iter();
        let mut remote = remote_hits.into_iter();
        loop {
            let a = remote.next();
            let b = local.next();
            if a.is_none() && b.is_none() {
                break;
            }
            for hit in [a, b].into_iter().flatten() {
                if let Some(host) = host_of_opt(&hit.website) {
                    if seen.insert(host) {
                        merged.push(hit);
                    }
                }
            }
            if merged.len() >= MAX_WEB_HITS {
                break;
            }
        }
        let scores = rank_by_name_match(&mut merged, &parsed);
        merged.truncate(MAX_WEB_HITS);
        (merged, scores)
    };
    let (recorded, recorded_scores) = recorded;

    // A recorded URL is fetched exactly as recorded, not reduced to its bare
    // host. Plenty of sites serve only on `www.` — Chandigarh University's
    // `cuchd.in` is one — so stripping it turns a known-good URL into a dead
    // one. The host is still tracked separately, because that is what
    // subdomain expansion and per-host bookkeeping key on.
    let mut url_for_host: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut name_match: std::collections::HashMap<String, f32> =
        std::collections::HashMap::new();
    let mut source_for: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // Name of the best-matching recorded *entity*, used to tell a site query
    // from a page query.
    //
    // Only entity directories count. A web index returns page titles, not
    // entity names, and a page title contains the page words by definition —
    // so reading the name from a search result made every query look
    // site-seeking. "chandigarh university bca" matched the title "BCA
    // Course … Chandigarh University", concluded nothing was left over,
    // applied the entry-page prior, and buried the very BCA page it had just
    // found under a YouTube video. A knowledge base says what an
    // organisation is *called*; an index says what a page is *about*.
    let best_entity_name: Option<String> = recorded
        .iter()
        .find(|h| matches!(h.source.as_str(), "wikidata" | "openstreetmap"))
        .map(|h| h.name.clone());
    trace.directory_hits = recorded.len();
    for (i, hit) in recorded.into_iter().enumerate().rev() {
        let Some(host) = host_of_opt(&hit.website) else { continue };
        tracing::debug!("recorded: {} -> {}", hit.name, hit.website);
        url_for_host.insert(host.clone(), hit.website.clone());
        name_match
            .insert(host.clone(), recorded_scores.get(i).copied().unwrap_or(1.0));
        source_for.insert(host.clone(), describe_source(&hit));

        // When a recorded hit names a host the guesser already proposed,
        // promote that entry rather than replacing it — and keep *its*
        // reading of the query.
        //
        // Replacing it silently disabled sub-page search. A recorded entry
        // carries the query's most specific site/page reading, which is not
        // always the one that produced the host: for "valuepickr bajaj
        // finance" the guesser had `valuepickr.com` under the reading
        // *valuepickr* / "bajaj finance", and overwriting it with a reading
        // whose page half was "valuepickr" sent the sitemap search looking
        // for the wrong words. The answer fell back from the exact thread to
        // the forum's front page.
        let existing = ordered.iter().position(|(h, _, _)| *h == host);
        let split = match existing {
            Some(ix) => ordered.remove(ix).2,
            None => recorded_split.clone(),
        };
        ordered.insert(0, (host, (0, i), split));
    }
    trace.guessed = ordered.len();

    // ---- Wave 1: does the name exist? -------------------------------------
    let t0 = std::time::Instant::now();
    let live = probe::live_hosts(ordered.iter().map(|(h, _, _)| h.clone()).collect()).await;
    let live: std::collections::HashSet<String> = live.into_iter().collect();

    // Back into prior order: DNS says which exist, not which is likeliest.
    let mut targets: Vec<(String, query::Split)> = ordered
        .into_iter()
        .filter(|(h, _, _)| live.contains(h))
        .map(|(h, _, split)| (h, split))
        .collect();

    // ---- Wave 2: the site may not be at the apex --------------------------
    let bases: Vec<(String, query::Split)> =
        targets.iter().take(MAX_SUBDOMAIN_BASES).cloned().collect();
    // A domain with a wildcard record answers for every subdomain, so
    // expanding one produces confident nonsense. Ask once per base.
    let wildcards =
        probe::wildcard_domains(&bases.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>())
            .await;
    let sub_candidates: Vec<String> = bases
        .iter()
        .filter(|(host, _)| !wildcards.contains(host))
        .flat_map(|(host, _)| SUBDOMAINS.iter().map(move |s| format!("{s}.{host}")))
        .collect();
    let sub_live: std::collections::HashSet<String> =
        probe::live_hosts(sub_candidates).await.into_iter().collect();
    trace.dns_ms = t0.elapsed().as_millis() as u64;

    // Splice each base's subdomains in directly *after* that base, rather
    // than appending them all at the end.
    //
    // Appending looks equivalent and is not. The fetch budget is spent in
    // list order, and wave one leaves behind junk that happens to resolve —
    // "valuepickr bajaj finance" produces `finance.org`, `finance.net`,
    // `finance.in`, `finances.com` from the reading that takes "finance" as
    // the whole site name. All real domains, all irrelevant, and all ahead
    // of `forum.valuepickr.com` if subdomains go last. Observed: the budget
    // filled with `finance.*` and the forum was never fetched.
    let mut spliced: Vec<(String, query::Split)> = Vec::new();
    for (host, split) in targets {
        let subs: Vec<(String, query::Split)> = SUBDOMAINS
            .iter()
            .map(|s| format!("{s}.{host}"))
            .filter(|h| sub_live.contains(h))
            // `www.x` and `x` are the same site; keep the shorter form.
            .filter(|h| h.trim_start_matches("www.") != host)
            .map(|h| (h, split.clone()))
            .collect();
        spliced.push((host, split));
        spliced.extend(subs);
    }
    let mut targets = spliced;
    targets.dedup_by(|a, b| a.0 == b.0);
    trace.resolved = targets.len();

    // Reserve part of the fetch budget for guessed hostnames.
    //
    // Recorded hits sit at the head of the queue, so a web index returning
    // eight results simply consumed everything. That is a real loss, not a
    // reshuffle: asked for "govorit moskva radio station", the index returns
    // eight radio *directories* — liveonlineradio, radioguide, thenonstopradio
    // — and the station's own `govoritmoskva.ru`, which hostname synthesis
    // had already found and DNS had already confirmed, was never retrieved.
    //
    // The sources fail in different directions, which is the entire reason
    // for running four of them; letting the loudest one take the whole
    // budget throws that away.
    let recorded_hosts: std::collections::HashSet<&String> =
        url_for_host.keys().collect();
    let (from_sources, from_guesses): (Vec<_>, Vec<_>) = targets
        .into_iter()
        .partition(|(h, _)| recorded_hosts.contains(h));

    // A hostname that exists *and* contains every distinctive word of the
    // query goes first — ahead of anything a directory or index returned.
    //
    // `stedmundshillong.in` resolving in DNS is close to proof: someone
    // registered a domain spelling out this exact institution and this exact
    // city, and it is live. Nothing an index can return outranks that as
    // evidence of a navigational target. Leaving these behind the search
    // results meant they lost fetch slots to whatever the index happened to
    // rank highly, and the school's own site simply never got retrieved —
    // the answer flipped between the real site and its Wikipedia article
    // depending on network timing.
    let (strong, weak): (Vec<_>, Vec<_>) = from_guesses
        .into_iter()
        .partition(|(h, _)| host_covers_query(h, &parsed));

    let mut targets: Vec<(String, query::Split)> = strong
        .into_iter()
        .chain(from_sources.into_iter().take(MAX_FETCHES - GUESS_RESERVE))
        .chain(weak.into_iter().take(GUESS_RESERVE))
        .collect();
    targets.truncate(MAX_FETCHES);
    trace.live_hosts = targets.iter().map(|(h, _)| h.clone()).collect();

    // ---- Confirm: fetch, and score against what each reading claimed ------
    let t1 = std::time::Instant::now();
    let split_for: std::collections::HashMap<String, query::Split> =
        targets.iter().cloned().collect();
    let fetch_targets: Vec<String> = targets
        .iter()
        .map(|(h, _)| url_for_host.get(h).cloned().unwrap_or_else(|| h.clone()))
        .collect();
    let pages = probe::fetch_all(client, fetch_targets).await;
    trace.fetched = pages.len();

    // Whether to apply the entry-page prior, decided once for the query.
    let entity_name = best_entity_name.clone();
    let site_seeking = is_site_seeking(&parsed, entity_name.as_deref());
    tracing::debug!("site_seeking={site_seeking} (entity {entity_name:?})");

    let mut answers: Vec<Answer> = Vec::new();
    let mut explore: Vec<(String, query::Split)> = Vec::new();

    for p in pages {
        let host = host_of(&p.url);
        let split = split_for
            .get(&host)
            .or_else(|| split_for.get(host.trim_start_matches("www.")))
            .cloned()
            .unwrap_or_else(|| query::Split { site: Vec::new(), page: Vec::new() });

        // A site whose reading also named a page is a lead, not an answer.
        if !split.page.is_empty() {
            let site_tokens = tokens_for(&parsed, &split.site);
            if probe::match_score(&site_tokens, &p) >= SITE_CONFIDENCE {
                explore.push((host.clone(), split.clone()));
            }
        }

        // How well the *directory entry's name* matched modulates the
        // score, for answers that came from a directory at all.
        //
        // "chandigarh university" returns two pages that are both genuinely
        // titled Chandigarh University: `cuchd.in`, the university, and
        // `culko.in`, its Unnao campus. Page text cannot separate them. What
        // can is that one was recorded under the name "Chandigarh
        // University" and the other under "Chandigarh University, Unnao" —
        // the directory already knew they were different things, and that
        // knowledge was being discarded after ordering.
        //
        // Guessed answers default to 1.0 rather than being penalised: a
        // guess that verified is not evidence of the wrong entity, it is
        // just evidence from a different source.
        let provenance =
            0.85 + 0.15 * name_match.get(&host).copied().unwrap_or(1.0);
        let depth_prior = if site_seeking {
            DEPTH_PRIOR[url_depth(&p.url).min(DEPTH_PRIOR.len() - 1)]
        } else {
            1.0
        };
        // Clamped: the modifiers are multiplicative and can stack past 1.0,
        // which surfaced as "confidence 106%" in the UI.
        let score = (probe::match_score(&parsed.tokens, &p)
            * provenance
            * own_site_bonus(&p.url, &entity_domains)
            * depth_prior
            * platform_penalty(&p.url))
            .min(1.0);
        let via = source_for.get(&host).cloned().unwrap_or_else(|| describe(&split));
        let controls = highlight::control_words(
            &parsed.tokens,
            &host,
            best_entity_name.as_deref(),
        );
        let highlight = highlight::find(&p.html, &p.url, &p.body, &controls);
        answers.push(Answer {
            url: p.url,
            title: p.title,
            score,
            quality: p.quality,
            via,
            body_text: p.body,
            html: p.html,
            highlight,
        });
    }

    // ---- Which page on the site? ------------------------------------------
    //
    // Sites are explored concurrently. Each one costs a chain of dependent
    // round trips — robots.txt, then a sitemap index, then a sitemap of up
    // to a megabyte, then the pages themselves — and running two sites in
    // sequence adds those chains rather than overlapping them.
    let explorations = explore.into_iter().take(MAX_SUBDOMAIN_BASES).map(
        |(host, split)| {
            let page_tokens = tokens_for(&parsed, &split.page);
            async move {
                let robots = site::robots_for(client, &host).await;
                let urls = site::sitemap_urls(client, &host, &robots).await;
                (split, page_tokens, robots, urls)
            }
        },
    );

    for (split, page_tokens, robots, urls) in
        futures::future::join_all(explorations).await
    {
        trace.sitemap_urls += urls.len();
        if urls.is_empty() {
            continue;
        }

        let picks = site::best_pages(&urls, &page_tokens, MAX_SUBPAGE_FETCHES);
        let allowed: Vec<site::PageMatch> = picks
            .into_iter()
            .filter(|m| {
                url::Url::parse(&m.url)
                    .map(|u| robots.is_allowed(u.path()))
                    .unwrap_or(false)
            })
            .collect();
        // Keyed canonically because a fetch may redirect, and the URL that
        // comes back is not always the one that went out.
        let page_score: std::collections::HashMap<String, f32> = allowed
            .iter()
            .map(|m| (common::canonical_url(&m.url), m.score))
            .collect();

        for p in probe::fetch_all(client, allowed.iter().map(|m| m.url.clone()).collect()).await
        {
            trace.fetched += 1;
            // How well the *URL* matched modulates how well the *page*
            // matched, rather than being averaged with it. Retrieved titles
            // are often near-identical across a forum's threads — "Bajaj
            // Finance Limited", "Bajaj housing finance" and "SG Finserve -
            // is this birth of another Bajaj finance?" all score 0.77 on
            // content alone. The slug is what tells them apart, so it has to
            // reach the final ordering rather than only the fetch shortlist,
            // where its effect survived merely as sort stability.
            let matched = probe::match_score(&parsed.tokens, &p);
            let slug = page_score
                .get(&common::canonical_url(&p.url))
                .copied()
                .unwrap_or(1.0);
            let own = own_site_bonus(&p.url, &entity_domains);
            let platform = platform_penalty(&p.url);
            // The entry-page prior applies here too.
            //
            // Applying it only to the homepage path left the hole it was
            // written to close. Asked for "govorit moskva radio station",
            // the resolver took `radioguide.fm` — a third-party radio
            // directory — as a site candidate, searched *its* sitemap, and
            // returned `radioguide.fm/internet-radio-russia/govorit-moskva-920-fm`
            // over the station's own `govoritmoskva.ru`. A deep page on
            // somebody else's site is the least likely thing to be the
            // answer to a query that names only an organisation, and where
            // the candidate came from does not change that.
            let depth_prior = if site_seeking {
                DEPTH_PRIOR[url_depth(&p.url).min(DEPTH_PRIOR.len() - 1)]
            } else {
                1.0
            };
            let via = format!(
                "found \"{}\" in {}'s own sitemap",
                split.page.join(" "),
                host_of(&p.url)
            );
            let controls = highlight::control_words(
                &parsed.tokens,
                &host_of(&p.url),
                best_entity_name.as_deref(),
            );
            let highlight = highlight::find(&p.html, &p.url, &p.body, &controls);
            answers.push(Answer {
                url: p.url,
                title: p.title,
                score: (matched * (0.8 + 0.2 * slug) * own * depth_prior * platform)
                    .min(1.0),
                quality: p.quality,
                via,
                body_text: p.body,
                html: p.html,
                highlight,
            });
        }
    }
    trace.fetch_ms = t1.elapsed().as_millis() as u64;

    collapse_to_front_door(&mut answers);
    answers.retain(|a| a.score >= MIN_CONFIDENCE);
    answers.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Ties go to the shorter URL, which means the apex domain over
            // its own subdomains. A site's news feed and support portal
            // mention the institution exactly as often as its front page
            // does, so content scoring cannot separate them — and asked for
            // "panjab university" it returned `news.puchd.ac.in`, while
            // "iiser mohali" returned `support.iisermohali.ac.in`. Both are
            // the right organisation and the wrong page. Shortest wins is a
            // crude rule that is right nearly every time here, because a
            // navigational query with no page part is asking for the front
            // door.
            .then_with(|| a.url.len().cmp(&b.url.len()))
    });
    answers.dedup_by(|a, b| a.url == b.url);

    // ---- Let the model pick the control, for the top answer only ----------
    //
    // The heuristic matcher scores labels by word overlap, which is enough to
    // find the right region of a page and not enough to choose within it: for
    // "chandigarh university bca" it picked "Computing (BCA/ MCA)" over the
    // page's own "Bachelor of Computer Applications", and for "netflix cancel
    // membership" it picked a label the browser never renders. Deciding
    // between "Cancel Membership", "Manage your membership" and "Finish
    // Cancellation" is a judgement about meaning.
    //
    // Top answer only: this is a network round trip on the critical path, and
    // nobody reads the highlight on result four.
    if let (Some(model), Some(top)) = (&sources.llm, answers.first_mut()) {
        let controls = highlight::control_words(
            &parsed.tokens,
            &host_of(&top.url),
            best_entity_name.as_deref(),
        );
        if !controls.is_empty() {
            let request = controls
                .iter()
                .map(|t| t.text.clone())
                .collect::<Vec<_>>()
                .join(" ");
            let menu = highlight::candidate_labels(
                &top.html,
                &top.body_text,
                MAX_CONTROL_CANDIDATES,
            );
            if let Some(label) =
                model.choose_control(client, &request, &top.title, &menu).await
            {
                // Verified against the page again before use: the model was
                // given a menu, but trusting it not to paraphrase is exactly
                // the assumption that produces silently-failing fragments.
                if let Some(h) = highlight::from_label(&top.url, &top.body_text, &label)
                {
                    top.highlight = Some(h);
                }
            }
        }
    }

    // ---- Answer the question outright, when it was one --------------------
    //
    // Only from the page that was just retrieved and verified, never from the
    // model's own knowledge. An answer with no source behind it is the thing
    // that turns a search engine into a confident liar.
    let answer = match (&sources.llm, answers.first()) {
        (Some(model), Some(top)) if llm::looks_like_question(raw) => model
            .answer_from_page(client, raw, &top.title, &top.body_text)
            .await
            .map(|text| InlineAnswer {
                text,
                source_url: top.url.clone(),
                source_title: top.title.clone(),
            }),
        _ => None,
    };

    Resolution { query: raw.to_string(), answers, trace, answer }
}

/// Order recorded hits by how well the entity's *name* matches the query,
/// and drop the ones that match none of it.
///
/// Neither source returns its results in a useful order. Asked for
/// "chandigarh university", Wikidata's entity search returned, in order:
/// CGC University Mohali, the city of Chandigarh, Chandigarh University
/// Unnao, and — fourth — Chandigarh University. OSM was no better. Taking
/// them as given meant the correct entity sat at the back of the fetch queue
/// behind three wrong ones and their subdomains, and was never retrieved.
///
/// The same two measures that rank sub-pages work here, for the same reason:
/// a name should contain everything asked for (coverage) and not much else
/// (precision). "Chandigarh University" covers the query completely in two
/// words; "Chandigarh University, Unnao" covers it in three; "Chandigarh"
/// covers half of it.
///
/// Zero-coverage hits are removed outright rather than ranked last — an
/// entity sharing no word with the query is not a weak candidate, it is a
/// different thing, and it would otherwise consume a fetch.
fn rank_by_name_match(
    hits: &mut Vec<DirectoryHit>,
    parsed: &query::ParsedQuery,
) -> Vec<f32> {
    let wanted: Vec<&query::Token> = parsed
        .tokens
        .iter()
        .filter(|t| t.kind != query::Kind::Stop)
        .collect();
    if wanted.is_empty() {
        return vec![1.0; hits.len()];
    }

    let score_of = |hit: &DirectoryHit| -> f32 {
        let words: Vec<String> = hit
            .name
            .to_lowercase()
            .replace('\'', "")
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_string())
            .collect();
        if words.is_empty() {
            return 0.0;
        }
        let hits_n = wanted
            .iter()
            .filter(|t| t.forms.iter().any(|f| words.iter().any(|w| w == f)))
            .count();
        if hits_n == 0 {
            return 0.0;
        }
        let coverage = hits_n as f32 / wanted.len() as f32;
        let precision = hits_n as f32 / words.len() as f32;
        coverage * coverage * (0.6 + 0.4 * precision)
    };

    let mut scored: Vec<(f32, DirectoryHit)> =
        hits.drain(..).map(|h| (score_of(&h), h)).filter(|(s, _)| *s > 0.0).collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
    });
    let scores: Vec<f32> = scored.iter().map(|(s, _)| *s).collect();
    *hits = scored.into_iter().map(|(_, h)| h).collect();
    scores
}

/// Prefer a page on the organisation's own domain over a page about it
/// somewhere else.
///
/// A web index answers "which pages are most relevant to these words", which
/// is not the same question as "which page did they mean to visit". Asked
/// for "chandigarh university bca", the top results include a YouTube video
/// *about* Chandigarh University's BCA programme, whose title matches the
/// query at least as well as the university's own course page — and on text
/// alone the video won.
///
/// A directory hit on the *site half* of the query settles it: Wikidata says
/// Chandigarh University's website is `cuchd.in`, so a page on `cuchd.in` is
/// the thing being asked for and a page on `youtube.com` is commentary. The
/// bonus is small and multiplicative, so it reorders near-ties without
/// letting a weak page on the right domain beat a strong one elsewhere.
/// Prior probability that a URL is a site's entry page, by how deep it sits.
///
/// Straight out of the homepage-finding literature: Kraaij, Westerveld &
/// Hiemstra (SIGIR 2002) showed that URL *form* alone — root vs subroot vs
/// path vs file — identified over 70% of entry pages at rank 1 and 89% in
/// the top 10, with no content matching at all. It is the strongest single
/// signal available for "find me this organisation's site", and it needs no
/// directory, no index and no network.
///
/// It is what fixes the failure that hand-tuned bonuses kept missing: asked
/// for "jawaharlal nehru university delhi", a web index returns the
/// Wikipedia article, whose title matches the query word for word and
/// therefore outscores `jnu.ac.in` on every content signal there is. One is
/// a root URL and the other is `/wiki/<something>`, and that difference is
/// the whole answer.
const DEPTH_PRIOR: [f32; 4] = [1.0, 0.80, 0.65, 0.50];

/// Path segments in a URL: 0 for a root page, 3+ for a deep one.
fn url_depth(url: &str) -> usize {
    url::Url::parse(url)
        .ok()
        .map(|u| u.path().split('/').filter(|s| !s.is_empty()).count())
        .unwrap_or(0)
}

/// Is the query asking for a *site*, or for a page within one?
///
/// The depth prior must only apply to the first kind. "chandigarh university
/// bca" wants a deep page and penalising depth would bury it; "jawaharlal
/// nehru university delhi" wants a front door.
///
/// The test is whether the query says anything the entity's *name* does not
/// already say. Place names do not count — "delhi" qualifies which Jawaharlal
/// Nehru University, it does not ask for a page — and neither do category
/// words. What is left is a genuine page request: "bca".
fn is_site_seeking(parsed: &query::ParsedQuery, entity_name: Option<&str>) -> bool {
    let Some(name) = entity_name else {
        // No knowledge base knows this entity — which is the *normal* case
        // for the long tail this whole system exists to reach, so the
        // fallback has to be good rather than safe.
        //
        // Defaulting to "page query" was the bug: for "saint edmunds school
        // shillong" Wikidata has the school but records no website, so the
        // entity name came back empty, the prior was skipped, and the
        // school's *Instagram profile* — whose title contains every query
        // word — outranked the school's own site.
        //
        // Absent an entity name, the query's own shape answers the question.
        // A word naming a kind of organisation ("school", "radio") means the
        // query names an institution and wants its front door. No such word
        // — "valuepickr bajaj finance" — means the query is asking for
        // something within a site.
        return parsed
            .tokens
            .iter()
            .any(|t| query::names_an_organisation(&t.text));
    };

    let name_words: std::collections::HashSet<String> = name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect();

    !parsed
        .tokens
        .iter()
        .filter(|t| t.kind == query::Kind::Distinctive)
        .any(|t| !t.forms.iter().any(|f| name_words.contains(f)))
}

/// Does this hostname spell out every identifying word of the query?
///
/// Substring matching, because hostnames are concatenated: "shillong" has to
/// be found inside "stedmundshillong". Only distinctive words and place
/// names count — requiring "school" would reject the very domains that drop
/// it.
fn host_covers_query(host: &str, parsed: &query::ParsedQuery) -> bool {
    let identifying: Vec<&query::Token> = parsed
        .tokens
        .iter()
        .filter(|t| matches!(t.kind, query::Kind::Distinctive | query::Kind::Place))
        .collect();
    if identifying.len() < 2 {
        return false;
    }
    identifying
        .iter()
        .all(|t| t.forms.iter().any(|f| host.contains(f.as_str())))
}

/// Platforms that host pages *about* organisations rather than being them.
///
/// A school's Instagram profile, its Facebook page and its Wikipedia article
/// all carry the school's exact name in the title, so they match a
/// navigational query as well as the school's own site does — and sometimes
/// better, because the platform writes a cleaner title than the school does.
/// Measured across the judgment set this was the single largest error class:
/// `bits pilani` returned `instagram.com/bitspilaniofficial`, `bishop cotton
/// school shimla` returned its Instagram, `chandigarh university bca`
/// returned a YouTube video, and a third of all wrong answers were Wikipedia
/// articles.
///
/// The depth prior alone does not catch them: a profile URL is one segment
/// deep, and the site-seeking test that gates the prior needs an entity name
/// that the long tail does not have.
///
/// Demoted rather than excluded, because sometimes the platform page is the
/// only thing that exists — plenty of small businesses have a Facebook page
/// and no website, and that answer is better than nothing.
const PLATFORM_HOSTS: &[&str] = &[
    "instagram.com", "facebook.com", "twitter.com", "x.com", "linkedin.com",
    "youtube.com", "youtu.be", "tiktok.com", "pinterest.com", "reddit.com",
    "wikipedia.org", "wikiwand.com", "fandom.com",
    "justdial.com", "indiamart.com", "sulekha.com", "yelp.com",
    // Complaint and review aggregators rank well for "<brand> customer
    // service" precisely because that phrase is their entire business model.
    // `flipkart.pissedconsumer.com` outranked Flipkart's own help centre.
    "pissedconsumer.com", "complaintsboard.com", "trustpilot.com",
    "consumercomplaints.in", "mouthshut.com", "sitejabber.com",
    "collegedunia.com", "shiksha.com", "careers360.com", "getmyuni.com",
    "liveonlineradio.net", "radioguide.fm", "onlineradiobox.com",
];

/// How much a platform page is demoted relative to a first-party site.
const PLATFORM_PENALTY: f32 = 0.6;

fn platform_penalty(url: &str) -> f32 {
    match common::registrable_domain(url) {
        Some(d) if PLATFORM_HOSTS.contains(&d.as_str()) => PLATFORM_PENALTY,
        _ => 1.0,
    }
}

/// Calibrated so a page on the named organisation's own domain outranks an
/// equally well-worded page about it elsewhere.
///
/// "chandigarh university bca" surfaces a YouTube explainer, an
/// online-degree arm on a separate domain, and the university's own course
/// page. All three are about the right subject; one is the page the user
/// meant to open. Fitted against the judgment set — a tuned constant, not a
/// law.
const OWN_SITE_BONUS: f32 = 1.25;

fn own_site_bonus(
    url: &str,
    entity_domains: &std::collections::HashSet<String>,
) -> f32 {
    if entity_domains.is_empty() {
        return 1.0;
    }
    match common::registrable_domain(url) {
        Some(d) if entity_domains.contains(&d) => OWN_SITE_BONUS,
        _ => 1.0,
    }
}

/// How many probes each reading of the query may spend, best reading first.
///
/// Without a per-reading quota the first reading takes everything. Readings
/// are generated best-first and truncation happens globally, so reading zero
/// emitted its full 240 candidates and readings one through five were cut to
/// nothing. For "chandigarh university bca" that was fatal: reading zero
/// treats the whole query as one site name and — with "chandigarh" a place
/// and "university" a category — leaves "bca" as the only identifying word,
/// producing `bca.org`, `bca.net`, `bca.edu`. The reading that actually
/// works, *site* "chandigarh university" and *page* "bca", is reading one
/// and never got probed.
///
/// Linearly decreasing weights: the best reading still gets the largest
/// share, but no reading gets zero. Being confident about which reading is
/// right is precisely what this system cannot do up front — that is DNS's
/// job — so starving the alternatives defeats the design.
fn probe_quotas(n: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let weights: Vec<usize> = (0..n).map(|i| n - i).collect();
    let total: usize = weights.iter().sum();
    weights
        .iter()
        .map(|w| (MAX_PROBES * w / total).max(8))
        .collect()
}

/// One answer per site, and for a site-level query that answer is the front
/// door.
///
/// A university's blog, news feed and support portal all say the
/// university's name in their titles as emphatically as its homepage does —
/// often more so, because the homepage leads with a marketing line. So
/// content scoring ranks them interchangeably, and "chandigarh university"
/// returned `blog.cuchd.in` and `news.cuchd.in` above `cuchd.in`, while
/// "panjab university" returned `news.puchd.ac.in`. Every one of those is
/// the right institution and the wrong page.
///
/// A tie-break on URL length was not enough, because the scores are not tied
/// — they differ by whichever accident of wording favoured a subdomain. The
/// rule has to be structural: these are one site, the query named the site,
/// and the answer is its root. The group keeps the best score it earned, so
/// collapsing never demotes a site relative to other sites.
///
/// Skipped entirely when the query named a page, where a sub-page *is* the
/// answer and `forum.valuepickr.com/t/...` must not collapse to
/// `valuepickr.com`.
fn collapse_to_front_door(answers: &mut Vec<Answer>) {
    let mut best: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut drop = vec![false; answers.len()];

    for i in 0..answers.len() {
        let Some(domain) = common::registrable_domain(&answers[i].url) else {
            continue;
        };
        match best.get(&domain).copied() {
            None => {
                best.insert(domain, i);
            }
            Some(j) => {
                // Whether this group is a set of front doors or a set of
                // pages is read off the URLs, not off the query.
                //
                // A query-level flag does not work: almost every multi-word
                // query produces *some* reading with a page part, so a flag
                // derived from the query was false for "chandigarh
                // university" and the collapse never ran. But
                // `forum.valuepickr.com/t/bajaj-finance-limited/267` has a
                // path and `cuchd.in/` does not, and that difference is
                // exactly the distinction being drawn.
                let deep = |a: &Answer| {
                    url::Url::parse(&a.url).map(|u| u.path().len() > 1).unwrap_or(false)
                };
                let keep_i = if deep(&answers[i]) || deep(&answers[j]) {
                    answers[i].score > answers[j].score
                } else {
                    host_of(&answers[i].url).len() < host_of(&answers[j].url).len()
                };
                let (winner, loser) = if keep_i { (i, j) } else { (j, i) };
                answers[winner].score = answers[winner].score.max(answers[loser].score);
                drop[loser] = true;
                best.insert(domain, winner);
            }
        }
    }

    let mut i = 0;
    answers.retain(|_| {
        let keep = !drop[i];
        i += 1;
        keep
    });
}

/// Confidence that a fetched page really is the *site* the query named,
/// which is what licenses spending a sitemap fetch on it.
///
/// Lower than `MIN_CONFIDENCE` on purpose: being wrong here costs one
/// wasted request, not a wrong answer, and the site half of a query is
/// typically one word ("valuepickr") carrying less evidence than a full
/// query does.
const SITE_CONFIDENCE: f32 = 0.3;

/// Recover the parsed tokens — with their spelling forms and classification
/// — for one half of a split, which carries only the bare words.
fn tokens_for(parsed: &query::ParsedQuery, words: &[String]) -> Vec<query::Token> {
    words
        .iter()
        .filter_map(|w| parsed.tokens.iter().find(|t| t.text == *w).cloned())
        .collect()
}

fn host_of_opt(url: &str) -> Option<String> {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase()))
        .filter(|h| !h.is_empty())
}

/// Host of a URL, with `www.` removed.
///
/// The `www.` prefix is stripped here rather than at each call site because
/// every map in this module is keyed on the bare host, and forgetting it
/// once is a silent miss rather than an error. It was: directory provenance
/// was recorded under `cuchd.in` and looked up under `www.cuchd.in`, so the
/// modifier defaulted to "no information" for every answer that had come
/// from a directory — the exact answers it existed to rank.
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        .map(|h| h.trim_start_matches("www.").to_string())
        .unwrap_or_default()
}

/// Settlement kinds substantial enough that a bare word naming one is more
/// likely to be that place than an ordinary word.
///
/// The gazetteer is built from every `place=` node in OSM, which is 300,000
/// names and includes a great many hamlets named after common words. Asking
/// it "is this a place?" without qualification is therefore far too eager,
/// and it fails in a way that is invisible until it bites: OSM contains a
/// **village called "Saint"**, so `saint` classified as a place name, so
/// "saint edmunds school shillong" counted the place constraint as satisfied
/// by any page with "Saint" in the title — which is every St. Edmund's
/// church on earth. Requiring a town or larger drops the collisions while
/// keeping the names that actually disambiguate an institution.
///
/// A word that misses this bar is not discarded, only reclassified as
/// distinctive, so it still has to appear in the hostname.
const PLACE_KINDS: &[&str] =
    &["country", "state", "city", "town", "suburb", "borough", "quarter"];

/// Whether a token should be treated as naming a place.
pub fn place_gate(gaz: &Gazetteer) -> impl Fn(&str) -> bool + '_ {
    move |w: &str| {
        gaz.lookup(w, None)
            .is_some_and(|p| PLACE_KINDS.contains(&p.kind.as_str()))
    }
}

/// Below this the page does not convincingly claim to be what was asked for.
///
/// Set where it is because a *wrong* navigational answer is the expensive
/// error. The user asked for one specific thing; handing them a different
/// site that happens to share a word reads as the product being broken, in a
/// way that an empty result does not.
pub const MIN_CONFIDENCE: f32 = 0.4;

/// Plain-language provenance for an answer that came from a directory.
///
/// The explanation shown under a result was previously always derived from
/// the query reading, which produced sentences that were both wrong and
/// confusing for directory answers — "read \"edmunds schol shillong\" as
/// the site and \"saint\" as the page" under a result that in fact came
/// from a recorded website. An explanation nobody can check is worse than
/// none.
fn describe_source(hit: &DirectoryHit) -> String {
    match hit.source.as_str() {
        "openstreetmap" => {
            format!("OpenStreetMap lists this as {}'s website", hit.name)
        }
        "wikidata" => format!("Wikidata lists this as {}'s official website", hit.name),
        other => format!("found in the {other} web index"),
    }
}

fn describe(split: &query::Split) -> String {
    if split.page.is_empty() {
        format!("guessed the site from \"{}\"", split.site.join(" "))
    } else {
        format!(
            "read \"{}\" as the site and \"{}\" as the page",
            split.site.join(" "),
            split.page.join(" ")
        )
    }
}
