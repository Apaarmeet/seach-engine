//! Inbound anchor text aggregation.
//!
//! For every page, gather the text of the links that point *at* it. This is
//! the single highest-leverage relevance signal available without machine
//! learning, and it's the one most hobby search engines omit.
//!
//! Why it works:
//!   - It's written by *other people*, so it resists on-page keyword stuffing.
//!   - It describes the page in the vocabulary searchers actually use.
//!   - It covers pages whose own text is unhelpful (image-only pages, login
//!     walls, apps) — the classic example being a download page that never
//!     contains the word people search for.

use common::{registrable_domain, CrawledPage};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub struct AnchorIndex {
    /// target url -> anchor phrases from *other* domains (strongest signal:
    /// third-party testimony, hard to self-author)
    external: HashMap<String, HashSet<String>>,
    /// target url -> anchor phrases from the *same* domain. Still genuinely
    /// useful — a site's own navigation and in-body links describe targets
    /// well (Wikipedia's inter-article links are a textbook case) — but
    /// self-authored, so it's kept separate and admitted only after
    /// external text has had first claim on the character budget.
    internal: HashMap<String, HashSet<String>>,
    /// target url -> distinct *external* linking domains
    domains: HashMap<String, HashSet<String>>,
}

/// Anchor text that carries no information about the target. Indexing these
/// actively hurts: every page on the web is "click here".
const STOP_ANCHORS: &[&str] = &[
    "click here", "here", "read more", "more", "link", "this", "home",
    "next", "previous", "prev", "back", "top", "continue", "download",
    "1", "2", "3", "»", "«", "->", "...", ">>", "<<",
];

impl AnchorIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one source page; records its outbound anchors against targets.
    pub fn add_page(&mut self, page: &CrawledPage) {
        let src_domain = registrable_domain(&page.url);

        for link in &page.links {
            let text = normalise(&link.text);
            if text.is_empty() {
                // Still counts for inbound-domain trust even with no text.
                self.record_domain(&link.url, &src_domain);
                continue;
            }
            if STOP_ANCHORS.contains(&text.as_str()) || text.len() > 120 {
                self.record_domain(&link.url, &src_domain);
                continue;
            }

            let dst_domain = registrable_domain(&link.url);
            let is_internal = src_domain.is_some() && src_domain == dst_domain;

            if is_internal {
                self.internal.entry(link.url.clone()).or_default().insert(text);
                // Deliberately no domain-trust credit: a site vouching for
                // itself proves nothing.
            } else {
                self.external.entry(link.url.clone()).or_default().insert(text);
                self.record_domain(&link.url, &src_domain);
            }
        }
    }

    fn record_domain(&mut self, target: &str, src_domain: &Option<String>) {
        let Some(src) = src_domain else { return };
        if registrable_domain(target).as_ref() == Some(src) {
            return; // self-link
        }
        self.domains
            .entry(target.to_string())
            .or_default()
            .insert(src.clone());
    }

    /// Anchor text for a page, capped so one heavily-linked page can't
    /// balloon the index.
    ///
    /// External phrases are emitted first so that when the budget is tight
    /// the third-party descriptions survive and the self-authored ones are
    /// what get dropped.
    pub fn anchor_text_for(&self, url: &str, max_chars: usize) -> String {
        self.anchor_text_with(url, max_chars, true)
    }

    /// `include_internal = false` restricts output to third-party anchors.
    ///
    /// Worth having as a switch because the right answer is corpus-dependent:
    /// on a corpus dominated by one large self-referential site, internal
    /// anchors are mostly noise (every article links "India" to the same
    /// page), while on a broad web crawl they carry real navigational signal.
    /// Measure before choosing — see eval/sweep.sh.
    pub fn anchor_text_with(&self, url: &str, max_chars: usize, include_internal: bool) -> String {
        let mut out = String::new();
        let mut seen: HashSet<&str> = HashSet::new();

        let internal = include_internal.then(|| self.internal.get(url)).flatten();
        for source in [self.external.get(url), internal] {
            let Some(set) = source else { continue };
            let mut phrases: Vec<&String> = set.iter().collect();
            // Longer phrases are usually more specific; ties broken
            // alphabetically so output is deterministic across runs.
            phrases.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
            for p in phrases {
                if !seen.insert(p.as_str()) {
                    continue;
                }
                if out.len() + p.len() + 1 > max_chars {
                    break;
                }
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(p);
            }
        }
        out
    }

    /// Number of distinct domains linking to a page.
    pub fn inbound_domains(&self, url: &str) -> u32 {
        self.domains.get(url).map(|s| s.len() as u32).unwrap_or(0)
    }
}

fn normalise(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::OutLink;

    fn page(url: &str, links: &[(&str, &str)]) -> CrawledPage {
        CrawledPage {
            url: url.into(),
            title: String::new(),
            body_text: String::new(),
            links: links
                .iter()
                .map(|(u, t)| OutLink { url: u.to_string(), text: t.to_string() })
                .collect(),
            crawled_at_unix: 0,
            status: 200,
        }
    }

    #[test]
    fn collects_anchor_text_from_other_domains() {
        let mut idx = AnchorIndex::new();
        idx.add_page(&page("https://blog.in/a", &[("https://irctc.in/", "book train tickets")]));
        idx.add_page(&page("https://news.in/b", &[("https://irctc.in/", "indian railways booking")]));

        let anchors = idx.anchor_text_for("https://irctc.in/", 1000);
        assert!(anchors.contains("book train tickets"));
        assert!(anchors.contains("indian railways booking"));
        assert_eq!(idx.inbound_domains("https://irctc.in/"), 2);
    }

    #[test]
    fn internal_anchors_are_indexed_but_earn_no_domain_trust() {
        let mut idx = AnchorIndex::new();
        idx.add_page(&page(
            "https://shop.in/home",
            &[("https://shop.in/deals", "best deals ever")],
        ));
        // The text is useful, so it is indexed...
        assert_eq!(idx.anchor_text_for("https://shop.in/deals", 1000), "best deals ever");
        // ...but a site vouching for itself is not evidence of trust.
        assert_eq!(idx.inbound_domains("https://shop.in/deals"), 0);
    }

    #[test]
    fn external_anchors_win_the_budget_over_internal() {
        let mut idx = AnchorIndex::new();
        idx.add_page(&page("https://t.in/home", &[("https://t.in/p", "internal words here")]));
        idx.add_page(&page("https://other.in/a", &[("https://t.in/p", "external words")]));
        // Budget fits only one phrase; the external one must survive.
        let got = idx.anchor_text_for("https://t.in/p", 16);
        assert_eq!(got, "external words");
    }

    #[test]
    fn drops_uninformative_anchors_but_keeps_domain_count() {
        let mut idx = AnchorIndex::new();
        idx.add_page(&page("https://a.in/x", &[("https://target.in/", "click here")]));
        assert_eq!(idx.anchor_text_for("https://target.in/", 1000), "");
        // The link still proves a.in endorsed target.in.
        assert_eq!(idx.inbound_domains("https://target.in/"), 1);
    }

    #[test]
    fn respects_char_budget() {
        let mut idx = AnchorIndex::new();
        for i in 0..50 {
            idx.add_page(&page(
                &format!("https://src{i}.in/"),
                &[("https://t.in/", &format!("phrase number {i} about things"))],
            ));
        }
        assert!(idx.anchor_text_for("https://t.in/", 100).len() <= 100);
    }
}
