//! Turning a typed query into things worth trying.
//!
//! The web index answers "which page best matches these words?". This module
//! serves a different question — "which *site* did they mean, and which page
//! on it?" — and the two need different preparation.
//!
//! Two jobs here, both deliberately *generative* rather than decisive:
//!
//!   1. **Spelling forms.** `saint` and `st` are the same word in a school
//!      name; `edmunds` and `edmund` are the same word in a hostname. We emit
//!      every plausible surface form rather than picking one, because the
//!      evidence that settles it (does the host resolve? does the page say
//!      so?) lives downstream.
//!
//!   2. **Site/page splits.** "valuepickr bajaj finance" names a site *and* a
//!      page on it; "saint edmunds school shillong" names only a site. Rather
//!      than guessing which tokens are the site — every heuristic for that
//!      is wrong on something — we emit all reasonable splits and let DNS
//!      decide. Exactly one of `valuepickr.com`, `valuepickrbajaj.com`,
//!      `valuepickrbajajfinance.com` exists, and that fact is free to check.

/// What a token contributes to identifying the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Carries identity: "edmunds", "valuepickr", "bajaj".
    Distinctive,
    /// Describes a *kind* of thing, not a specific one: "school", "forum".
    /// Frequently dropped from hostnames — `stedmundshillong.in` contains
    /// the saint and the city but not the word "school".
    Generic,
    /// A place name: "shillong", "moscow". Often *kept* in a hostname
    /// precisely because the generic word was dropped, so this is its own
    /// category rather than a flavour of generic.
    Place,
    /// Grammatical filler, never part of a hostname: "the", "of", "in".
    Stop,
}

#[derive(Debug, Clone)]
pub struct Token {
    /// Normalised lowercase form as typed.
    pub text: String,
    /// Every surface form worth trying, `text` first. Deduplicated.
    pub forms: Vec<String>,
    pub kind: Kind,
}

#[derive(Debug, Clone)]
pub struct ParsedQuery {
    pub raw: String,
    pub tokens: Vec<Token>,
}

/// One way of reading the query: some tokens name the site, the rest name a
/// page on it. `page` empty means "they want the site itself".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub site: Vec<String>,
    pub page: Vec<String>,
}

/// Words that describe a category rather than an identity.
///
/// Kept separate from `places::synonyms::is_category_word`, which is tuned
/// for "cafe near me" local intent. This list is about *hostname
/// construction*: the question is "would a site owner leave this word out of
/// their domain name?", which is a different question from "does this word
/// pick out a place category?".
const GENERIC: &[&str] = &[
    "school", "college", "university", "institute", "academy", "campus",
    "hospital", "clinic", "pharmacy", "nursing",
    "forum", "forums", "board", "community", "blog", "wiki", "news",
    "official", "website", "web", "site", "page", "homepage", "home", "online",
    "higher", "secondary", "primary", "public", "central", "national",
    "ltd", "limited", "pvt", "private", "inc", "corp", "company", "co",
    "radio", "station", "channel", "tv", "fm",
    "department", "office", "bureau", "council", "board",
];

/// Is this one of the category words? Exposed for hostname synthesis, which
/// needs to know whether a word run looks like an institution's name.
pub fn is_generic_word(w: &str) -> bool {
    GENERIC.contains(&w)
}

/// Category words that name a *kind of organisation*, as opposed to a kind
/// of page.
///
/// "school", "college" and "radio" say the query is naming an institution;
/// "official", "admission" and "blog" say it is asking for a page within
/// one. The distinction decides whether a front page or a deep page is the
/// right answer, which is why it is worth separating from the broader
/// category list.
const ORGANISATION: &[&str] = &[
    "school", "college", "university", "institute", "academy", "campus",
    "hospital", "clinic", "pharmacy", "nursing",
    "forum", "forums", "board", "community", "wiki",
    "radio", "station", "channel",
    "department", "office", "bureau", "council",
];

pub fn names_an_organisation(w: &str) -> bool {
    ORGANISATION.contains(&w)
}

/// Filler that never survives into a hostname or a useful match.
const STOP: &[&str] = &[
    "the", "of", "in", "at", "on", "for", "a", "an", "to", "is", "com", "www",
];

/// Interchangeable spellings of the same name component.
///
/// These are *bidirectional* name conventions, not typos. Every entry here
/// is a case where both spellings are in real, common use for the same
/// entity — "St. Edmund's School" is signed that way and written "Saint
/// Edmunds" by people searching for it. A general spellchecker cannot help
/// with these because both forms are correct.
const EQUIVALENTS: &[(&str, &str)] = &[
    ("saint", "st"),
    ("mount", "mt"),
    ("fort", "ft"),
    ("doctor", "dr"),
    ("mister", "mr"),
    ("sister", "sr"),
    ("father", "fr"),
    ("and", "n"),
    ("first", "1st"),
    ("second", "2nd"),
    ("international", "intl"),
    ("technology", "tech"),
    ("government", "govt"),
    ("university", "univ"),
    ("association", "assn"),
];

/// Split a raw query into normalised, classified tokens.
///
/// `is_place` is injected rather than imported so this stays testable
/// without loading a gazetteer, matching how `places::search` handles the
/// same dependency.
pub fn parse(raw: &str, is_place: &dyn Fn(&str) -> bool) -> ParsedQuery {
    let tokens = raw
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(normalise_token)
        .filter(|t| !t.is_empty())
        .map(|text| {
            let kind = classify(&text, is_place);
            let forms = forms_for(&text);
            Token { text, forms, kind }
        })
        .collect();

    ParsedQuery { raw: raw.to_string(), tokens }
}

/// Lowercase and drop possessive apostrophes.
///
/// `edmund's` and `edmunds` must collapse to one token before anything else
/// runs — otherwise the apostrophe form tokenises as two tokens (`edmund`,
/// `s`) and the trailing `s` pollutes every hostname candidate.
fn normalise_token(word: &str) -> String {
    word.to_lowercase().replace('\'', "")
}

/// Category words are checked *before* place names, and the order is not
/// arbitrary.
///
/// A gazetteer built from every settlement on earth contains a place called
/// "University" (several, in the United States), one called "Saint", one
/// called "Of". Asking "is this a place?" first means the category word
/// "university" is classified as a location, and everything downstream
/// breaks: for "chandigarh university bca", both *chandigarh* and
/// *university* became places, leaving "bca" as the only identifying word,
/// and the resolver spent its entire budget on `bca.org`, `bca.net`,
/// `bca.edu`.
///
/// The category list is small, hand-written and unambiguous in intent. When
/// it and the gazetteer disagree, the list is right: someone typing
/// "university" means the kind of institution, not the hamlet in Mississippi.
pub fn classify(text: &str, is_place: &dyn Fn(&str) -> bool) -> Kind {
    if STOP.contains(&text) {
        Kind::Stop
    } else if GENERIC.contains(&text) {
        Kind::Generic
    } else if is_place(text) {
        Kind::Place
    } else {
        Kind::Distinctive
    }
}

/// Every spelling of one token worth trying, most-likely first.
pub fn forms_for(text: &str) -> Vec<String> {
    let mut forms = vec![text.to_string()];

    for (a, b) in EQUIVALENTS {
        if text == *a {
            forms.push(b.to_string());
        } else if text == *b {
            forms.push(a.to_string());
        }
    }

    // Plural/possessive stem. "edmunds" -> "edmund" matters because a site
    // owner picks one and the searcher types the other, in both directions.
    // Guarded on length so "its"/"his" don't spawn junk, and on a non-"s"
    // preceding character so "class" doesn't become "clas".
    if let Some(stem) = text.strip_suffix('s') {
        if stem.len() >= 3 && !stem.ends_with('s') {
            forms.push(stem.to_string());
        }
    } else if text.len() >= 3 {
        forms.push(format!("{text}s"));
    }

    forms.dedup();
    forms
}

/// Longest run of tokens we will consider to be the site name.
///
/// Three covers "value pickr", "make my trip" and "bajaj finance" written as
/// separate words. Beyond that the candidate hostnames get long enough that
/// they stop being plausible domain names.
const MAX_SITE_TOKENS: usize = 3;

/// Every reading of the query as (site, page), best-first.
///
/// Ordering is a prior, not a decision — downstream verification reorders on
/// evidence. The prior: a site name is usually at one end of the query
/// (people write "valuepickr bajaj finance" or "bajaj finance valuepickr",
/// rarely with the site buried in the middle), and the whole query naming a
/// site with no page is the single most common shape of all.
pub fn splits(q: &ParsedQuery) -> Vec<Split> {
    let usable: Vec<&Token> =
        q.tokens.iter().filter(|t| t.kind != Kind::Stop).collect();
    if usable.is_empty() {
        return Vec::new();
    }

    let text_of = |ts: &[&Token]| ts.iter().map(|t| t.text.clone()).collect::<Vec<_>>();
    let mut out: Vec<Split> = Vec::new();
    let mut push = |site: Vec<String>, page: Vec<String>| {
        let s = Split { site, page };
        if !s.site.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };

    // 1. The whole query is the site. "saint edmunds school shillong".
    push(text_of(&usable), Vec::new());

    // 2. A prefix names the site, the rest names a page on it.
    //    "valuepickr | bajaj finance".
    for n in 1..=MAX_SITE_TOKENS.min(usable.len().saturating_sub(1)) {
        push(text_of(&usable[..n]), text_of(&usable[n..]));
    }

    // 3. A suffix names the site. "bajaj finance | valuepickr" — the same
    //    query with the words the other way round, which people do type.
    for n in 1..=MAX_SITE_TOKENS.min(usable.len().saturating_sub(1)) {
        let at = usable.len() - n;
        push(text_of(&usable[at..]), text_of(&usable[..at]));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> impl Fn(&str) -> bool {
        |w: &str| matches!(w, "shillong" | "moscow" | "bangalore" | "delhi")
    }

    fn parsed(q: &str) -> ParsedQuery {
        parse(q, &places())
    }

    fn kinds(q: &str) -> Vec<Kind> {
        parsed(q).tokens.iter().map(|t| t.kind).collect()
    }

    #[test]
    fn possessives_collapse_to_one_token() {
        // "st edmund's school" must not tokenise to [st, edmund, s, school];
        // the stray "s" ends up concatenated into every hostname guess.
        let p = parsed("st edmund's school");
        let texts: Vec<&str> = p.tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, vec!["st", "edmunds", "school"]);
    }

    #[test]
    fn saint_and_st_are_the_same_word() {
        assert!(forms_for("saint").contains(&"st".to_string()));
        assert!(forms_for("st").contains(&"saint".to_string()));
    }

    #[test]
    fn plural_stems_go_both_ways() {
        assert!(forms_for("edmunds").contains(&"edmund".to_string()));
        assert!(forms_for("edmund").contains(&"edmunds".to_string()));
    }

    #[test]
    fn stemming_does_not_mangle_words_ending_in_double_s() {
        assert!(!forms_for("class").contains(&"clas".to_string()));
    }

    #[test]
    fn classification_separates_identity_from_category() {
        assert_eq!(
            kinds("saint edmunds school shillong"),
            vec![Kind::Distinctive, Kind::Distinctive, Kind::Generic, Kind::Place]
        );
    }

    /// A gazetteer of every settlement on earth contains a place called
    /// "University". The category list has to win.
    #[test]
    fn a_category_word_that_is_also_a_place_name_stays_a_category() {
        let everything_is_a_place = |_: &str| true;
        let p = parse("chandigarh university bca", &everything_is_a_place);
        let kinds: Vec<Kind> = p.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(kinds[1], Kind::Generic, "university must not be a place");
    }

    #[test]
    fn stopwords_never_reach_a_hostname() {
        let p = parsed("the university of delhi");
        let split = &splits(&p)[0];
        assert!(!split.site.contains(&"the".to_string()));
        assert!(!split.site.contains(&"of".to_string()));
    }

    /// The shape of Vinay's second example: no page part, the whole query
    /// identifies one site.
    #[test]
    fn whole_query_as_site_is_tried_first() {
        let p = parsed("saint edmunds school shillong");
        assert_eq!(
            splits(&p)[0],
            Split {
                site: vec!["saint".into(), "edmunds".into(), "school".into(), "shillong".into()],
                page: Vec::new(),
            }
        );
    }

    /// The shape of his first example: site + page on that site.
    #[test]
    fn site_plus_page_split_is_offered() {
        let p = parsed("valuepickr bajaj finance");
        let want = Split {
            site: vec!["valuepickr".into()],
            page: vec!["bajaj".into(), "finance".into()],
        };
        assert!(splits(&p).contains(&want), "got {:?}", splits(&p));
    }

    /// Same query, words reversed — still has to produce the same reading.
    #[test]
    fn site_named_last_is_also_offered() {
        let p = parsed("bajaj finance valuepickr");
        let want = Split {
            site: vec!["valuepickr".into()],
            page: vec!["bajaj".into(), "finance".into()],
        };
        assert!(splits(&p).contains(&want), "got {:?}", splits(&p));
    }

    #[test]
    fn splits_are_deduplicated() {
        let p = parsed("valuepickr");
        assert_eq!(splits(&p).len(), 1);
    }

    #[test]
    fn empty_query_yields_nothing_rather_than_panicking() {
        assert!(splits(&parsed("   ")).is_empty());
        assert!(splits(&parsed("the of in")).is_empty());
    }
}
