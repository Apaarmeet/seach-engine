//! Minimal robots.txt parser. Good enough to be polite for a scaffold; a
//! production crawler should swap this for a battle-tested crate (e.g.
//! `texting_robots`) that handles wildcards, `Allow:` precedence, and
//! `Crawl-delay` properly.
//!
//! Lives in `common` rather than in the crawler because the crawler is no
//! longer the only thing that fetches pages: the URL resolver checks a
//! handful of live pages per query and must be just as polite. Two parsers
//! would be two sets of politeness bugs.

#[derive(Debug, Default, Clone)]
pub struct RobotsRules {
    disallow: Vec<String>,
    pub crawl_delay_ms: u64,
    /// `Sitemap:` URLs declared in the file.
    ///
    /// Captured here because robots.txt is the *only* standard place a site
    /// says where its sitemap lives, and the resolver needs that to answer
    /// "which page on this site?" without crawling the whole site. Note
    /// these lines are global, not per-user-agent, so they are collected
    /// regardless of which agent block is in scope.
    pub sitemaps: Vec<String>,
}

impl RobotsRules {
    pub fn parse(body: &str, our_agent: &str) -> Self {
        let mut rules = RobotsRules {
            disallow: Vec::new(),
            crawl_delay_ms: 500,
            sitemaps: Vec::new(),
        };
        let mut applies_to_us = false;
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "user-agent" => {
                    applies_to_us = value == "*" || value.eq_ignore_ascii_case(our_agent);
                }
                "disallow" if applies_to_us && !value.is_empty() => {
                    rules.disallow.push(value.to_string());
                }
                // Not gated on `applies_to_us`: sitemap declarations are
                // file-scoped, not part of any user-agent group.
                "sitemap" if !value.is_empty() => {
                    rules.sitemaps.push(value.to_string());
                }
                "crawl-delay" if applies_to_us => {
                    if let Ok(secs) = value.parse::<f64>() {
                        rules.crawl_delay_ms = (secs * 1000.0) as u64;
                    }
                }
                _ => {}
            }
        }
        rules
    }

    pub fn is_allowed(&self, path: &str) -> bool {
        !self.disallow.iter().any(|rule| path.starts_with(rule))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
User-agent: *
Disallow: /admin
Crawl-delay: 1

Sitemap: https://example.com/sitemap.xml
Sitemap: https://example.com/sitemap_2.xml
";

    #[test]
    fn collects_sitemaps() {
        let r = RobotsRules::parse(SAMPLE, "rust-search-engine-bot");
        assert_eq!(
            r.sitemaps,
            vec![
                "https://example.com/sitemap.xml",
                "https://example.com/sitemap_2.xml"
            ]
        );
    }

    /// Sitemap lines sit outside any user-agent group and must still be read
    /// when the group in scope is not ours.
    #[test]
    fn sitemaps_are_read_even_under_another_agents_block() {
        let body = "User-agent: Googlebot\nDisallow: /\nSitemap: https://example.com/s.xml\n";
        let r = RobotsRules::parse(body, "rust-search-engine-bot");
        assert_eq!(r.sitemaps, vec!["https://example.com/s.xml"]);
        assert!(r.is_allowed("/anything"), "another agent's Disallow must not apply to us");
    }

    #[test]
    fn disallow_applies_to_the_wildcard_group() {
        let r = RobotsRules::parse(SAMPLE, "rust-search-engine-bot");
        assert!(!r.is_allowed("/admin/panel"));
        assert!(r.is_allowed("/public"));
        assert_eq!(r.crawl_delay_ms, 1000);
    }
}
