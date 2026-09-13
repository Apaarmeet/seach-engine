//! Minimal robots.txt parser. Good enough to be polite for a scaffold; a
//! production crawler should swap this for a battle-tested crate (e.g.
//! `texting_robots`) that handles wildcards, `Allow:` precedence, and
//! `Crawl-delay` properly.

#[derive(Debug, Default, Clone)]
pub struct RobotsRules {
    disallow: Vec<String>,
    pub crawl_delay_ms: u64,
}

impl RobotsRules {
    pub fn parse(body: &str, our_agent: &str) -> Self {
        let mut rules = RobotsRules {
            disallow: Vec::new(),
            crawl_delay_ms: 500,
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
