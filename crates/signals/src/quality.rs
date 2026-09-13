//! Page quality and near-duplicate detection.
//!
//! Coverage without filtering makes relevance *worse*, not better. A large
//! fraction of any ccTLD is parked domains, doorway pages and thin
//! boilerplate; if those compete on equal footing with real content, they
//! win queries they shouldn't because they're often keyword-dense and short
//! (which BM25 rewards via length normalisation).

/// Phrases that identify a parked / placeholder / holding page. Matched
/// case-insensitively against the title and the first part of the body.
const PARKED_MARKERS: &[&str] = &[
    "domain for sale",
    "buy this domain",
    "this domain is for sale",
    "domain name is for sale",
    "parked domain",
    "please access via the domain name",
    "under construction",
    "coming soon",
    "website is coming soon",
    "default web page",
    "apache2 ubuntu default page",
    "welcome to nginx",
    "index of /",
    "account suspended",
    "bandwidth limit exceeded",
];

/// Score a page 0.0 (junk) .. 1.0 (clean content).
///
/// Deliberately rule-based and legible rather than a learned classifier:
/// at this stage you want to be able to read *why* a page was demoted, and
/// a founder will ask. Replace with a trained model once you have judgments.
pub fn quality_score(title: &str, body: &str) -> f32 {
    let mut score: f32 = 1.0;

    let title_l = title.to_lowercase();
    let head: String = body.chars().take(600).collect::<String>().to_lowercase();

    // Parked / placeholder pages: decisive, not a nudge.
    if PARKED_MARKERS.iter().any(|m| title_l.contains(m) || head.contains(m)) {
        return 0.0;
    }

    // Thin content. Length thresholds are crude but catch most doorways.
    let len = body.chars().count();
    if len < 200 {
        score *= 0.25;
    } else if len < 600 {
        score *= 0.6;
    }

    // Missing or useless title.
    if title.trim().is_empty() {
        score *= 0.5;
    } else if title_l == "untitled" || title_l.starts_with("http") {
        score *= 0.7;
    }

    // Keyword stuffing: very low lexical diversity over a long document is
    // the classic signature of generated spam.
    if len > 400 {
        let words: Vec<&str> = body.split_whitespace().collect();
        if words.len() > 50 {
            let unique: std::collections::HashSet<_> =
                words.iter().map(|w| w.to_lowercase()).collect();
            let diversity = unique.len() as f32 / words.len() as f32;
            if diversity < 0.12 {
                score *= 0.3;
            } else if diversity < 0.25 {
                score *= 0.7;
            }
        }
    }

    score.clamp(0.0, 1.0)
}

/// 64-bit SimHash over word shingles.
///
/// Unlike a cryptographic hash, SimHash is *locality sensitive*: near-
/// identical documents produce hashes differing in only a few bits, so
/// near-duplicates are found with a Hamming-distance comparison. This
/// matters because the same article is syndicated across dozens of domains,
/// and showing ten copies of it is the fastest way to look broken.
pub fn simhash(text: &str) -> u64 {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();

    if words.is_empty() {
        return 0;
    }

    // 3-word shingles keep local word order, which single words discard.
    let mut bits = [0i32; 64];
    let shingles: Vec<String> = if words.len() < 3 {
        vec![words.join(" ")]
    } else {
        words.windows(3).map(|w| w.join(" ")).collect()
    };

    for shingle in &shingles {
        let h = fnv1a(shingle.as_bytes());
        for (i, bit) in bits.iter_mut().enumerate() {
            if (h >> i) & 1 == 1 {
                *bit += 1;
            } else {
                *bit -= 1;
            }
        }
    }

    let mut out = 0u64;
    for (i, &b) in bits.iter().enumerate() {
        if b > 0 {
            out |= 1 << i;
        }
    }
    out
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Two pages are near-duplicates when their SimHashes differ in <= 3 bits.
/// Tune against your corpus: lower is stricter.
pub const DUPLICATE_BIT_THRESHOLD: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_pages_score_zero() {
        assert_eq!(quality_score("example.in - Domain For Sale", "Buy this domain"), 0.0);
        assert_eq!(quality_score("Welcome", "Please access via the domain name."), 0.0);
        assert_eq!(quality_score("Apache2 Ubuntu Default Page", "It works!"), 0.0);
    }

    #[test]
    fn real_content_scores_high() {
        let body = "The monsoon season in Kerala typically begins in early June \
            and brings heavy rainfall across the Western Ghats. Farmers depend on \
            this rainfall for paddy cultivation and reservoir levels recover \
            substantially during these months across the southern districts."
            .repeat(3);
        assert!(quality_score("Kerala Monsoon Report", &body) > 0.9);
    }

    #[test]
    fn thin_content_is_demoted() {
        let thin = quality_score("Page", "short text here");
        let full = quality_score("Page", &"real sentence with varied words ".repeat(60));
        assert!(thin < full, "thin {thin} should score below full {full}");
    }

    #[test]
    fn keyword_stuffing_is_demoted() {
        let stuffed = "cheap loans ".repeat(300);
        assert!(quality_score("Loans", &stuffed) < 0.5);
    }

    #[test]
    fn simhash_detects_near_duplicates() {
        let a = "the quick brown fox jumps over the lazy dog in the meadow today";
        let b = "the quick brown fox jumps over the lazy dog in the meadow today!";
        let c = "completely unrelated text about semiconductor manufacturing yields";
        assert!(hamming(simhash(a), simhash(b)) <= DUPLICATE_BIT_THRESHOLD);
        assert!(hamming(simhash(a), simhash(c)) > DUPLICATE_BIT_THRESHOLD);
    }
}
