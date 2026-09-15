//! An LLM for the two judgements that rules cannot make.
//!
//! Two places in this pipeline need language understanding rather than string
//! matching, and both were visibly failing without it:
//!
//!   1. **Which control did they mean?** The heuristic matcher scores labels
//!      by word overlap, so "chandigarh university bca" picked "Computing
//!      (BCA/ MCA)" — right page, clumsy target — and "netflix cancel
//!      membership" picked a label the browser never renders. Choosing
//!      between "Cancel Membership", "Manage your membership" and "Finish
//!      Cancellation" is a judgement about meaning.
//!
//!   2. **Answering the question outright.** "How many runs did Virat Kohli
//!      score in Australia" wants a number, not a link. Extracting it from a
//!      retrieved page is exactly what a language model is for.
//!
//! Two rules make this safe to rely on:
//!
//! - **The model chooses, it does not invent.** For highlighting it is given
//!   a list of labels actually present on the page and must return one of
//!   them verbatim; anything else is rejected. The cost of a hallucinated
//!   fragment is a silent failure the user reads as a broken product.
//! - **It answers only from the page.** No answer is produced from the
//!   model's own knowledge — that is how a search engine starts confidently
//!   stating things no source said.
//!
//! Optional throughout. With no `OPENROUTER_API_KEY` the heuristic path runs
//! and nothing breaks.

use serde::Deserialize;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Default model. Override with `OPENROUTER_MODEL`.
///
/// Chosen for a property that matters more here than price: it is **not a
/// reasoning model**. Reasoning models emit a hidden thinking stream that is
/// billed and counted against `max_tokens` *before* any visible output, and
/// on a prompt this size they never reach the output at all. Measured on the
/// same step-planning request, with a 1,500-token budget:
///
/// ```text
///   deepseek/deepseek-v4-flash   finish=length  content=null   (1500 tokens spent)
///   qwen/qwen3.7-flash           finish=length  content=null   (1500 tokens spent)
///   google/gemini-2.5-flash-lite finish=stop    valid JSON     (224 tokens)
/// ```
///
/// The cheaper per-token models are not cheaper in practice when they spend
/// the whole budget thinking and return nothing. This is classification and
/// extraction work, not analysis — there is nothing to reason about.
const DEFAULT_MODEL: &str = "google/gemini-2.5-flash-lite";

#[derive(Debug, Clone)]
pub struct Llm {
    api_key: String,
    model: String,
}

impl Llm {
    /// Configured from the environment, or `None` when no key is present.
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())?;
        let model = std::env::var("OPENROUTER_MODEL")
            .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Some(Llm { api_key, model })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    async fn chat(
        &self,
        client: &reqwest::Client,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Option<String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            // Capped for models that reason anyway. Note this is a request,
            // not a guarantee — some providers ignore it, which is why the
            // default model is one that does not reason at all.
            "reasoning": { "effort": "low" },
            // Deterministic: the same query should resolve the same way twice,
            // and a search box that answers differently each time reads as
            // broken however good the average is.
            "temperature": 0.0,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
        });

        let resp = client
            .post(ENDPOINT)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| tracing::warn!("openrouter request failed: {e}"))
            .ok()?;

        if !resp.status().is_success() {
            tracing::warn!("openrouter returned HTTP {}", resp.status());
            return None;
        }

        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| tracing::warn!("openrouter response unparseable: {e}"))
            .ok()?;
        let text = parsed
            .choices
            .first()?
            .message
            .content
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.is_empty() {
            tracing::warn!("model returned no content (reasoning budget exhausted?)");
            None
        } else {
            Some(text)
        }
    }

    /// Pick the control label that best matches the request.
    ///
    /// Constrained to `candidates` and verified against them on return, so
    /// the model is choosing from a menu rather than writing free text. That
    /// is the difference between "an LLM picks the button" and "an LLM
    /// invents a button that isn't there".
    pub async fn choose_control(
        &self,
        client: &reqwest::Client,
        request: &str,
        page_title: &str,
        candidates: &[String],
    ) -> Option<String> {
        if candidates.is_empty() {
            return None;
        }
        let numbered: String = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {c}\n", i + 1))
            .collect();

        let system = "You pick which link or button on a web page a person \
            meant. Reply with ONLY the number of the best option, or the word \
            NONE if none of them is what they asked for. No explanation.";
        let user = format!(
            "The person searched for: \"{request}\"\n\
             They landed on the page titled: \"{page_title}\"\n\n\
             Which of these controls on that page did they mean?\n\n{numbered}\n\
             Answer with one number, or NONE.",
        );

        // Generous for a one-number answer: the budget is shared with the
        // reasoning stream, and at 8 tokens the model never reached its
        // visible output at all.
        let reply = self.chat(client, system, &user, 600).await?;
        if reply.to_uppercase().contains("NONE") {
            return None;
        }
        // First run of digits only. Concatenating every digit in the reply
        // turns "12" plus a stray "3" into index 123.
        let digits: String = reply
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let idx: usize = digits.parse().ok()?;
        // 1-based, and out-of-range means the model ignored the menu.
        candidates.get(idx.checked_sub(1)?).cloned()
    }

    /// Answer a question from a page's text, or decline.
    ///
    /// Declining is a first-class outcome. A search engine that answers
    /// confidently from the model's own memory rather than the retrieved page
    /// is worse than one that says nothing, because the user has no way to
    /// tell the two apart.
    pub async fn answer_from_page(
        &self,
        client: &reqwest::Client,
        question: &str,
        page_title: &str,
        page_text: &str,
    ) -> Option<String> {
        let excerpt: String = page_text.chars().take(MAX_ANSWER_CONTEXT).collect();
        if excerpt.trim().is_empty() {
            return None;
        }

        let system = "Answer the question using ONLY the page text provided. \
            Be direct and brief — one or two sentences, and lead with the \
            actual answer. If the page does not contain the answer, reply \
            with exactly: NO_ANSWER. Never use knowledge from outside the \
            page text.";
        let user = format!(
            "Question: {question}\n\nPage title: {page_title}\n\nPage text:\n{excerpt}",
        );

        let reply = self.chat(client, system, &user, 1200).await?;
        if reply.contains("NO_ANSWER") {
            return None;
        }
        Some(reply)
    }
}

/// One step of a guided walkthrough.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GuideStep {
    /// Exact visible text of the control to spotlight.
    ///
    /// Text, never a CSS selector. `.yt-btn-3f2a` breaks the week YouTube
    /// ships a redesign; "Cancel membership" survives it. Text is also the
    /// only thing the extension can match against reliably, since it reads
    /// the *rendered* page rather than served markup.
    pub find: String,
    /// What the person should do, in one short line.
    pub instruction: String,
    /// Substring the page URL should contain for this step, when the step
    /// only makes sense on a particular page. Empty means any page.
    #[serde(default)]
    pub url_hint: String,
}

/// A walkthrough: where to start, and what to click once there.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Plan {
    /// The page on the *product* where the task is actually performed.
    ///
    /// Not the help article. Asked "how to cancel a YouTube plan", a search
    /// engine that returns YouTube's support page has answered a different
    /// question — the user wants to cancel, not to read about cancelling. The
    /// place that happens is `youtube.com/paid_memberships`, and the support
    /// page names it in its own instructions.
    ///
    /// Empty when the page names no such URL, in which case the caller falls
    /// back to the article itself.
    #[serde(default)]
    pub start_url: String,
    #[serde(default)]
    pub steps: Vec<GuideStep>,
}

impl Llm {
    /// Turn a how-to page into a sequence of controls to spotlight.
    ///
    /// The steps are *read off a retrieved page*, not recalled from the
    /// model's training data. A walkthrough invented from memory sends people
    /// clicking for buttons that do not exist, which is worse than no
    /// guidance at all — they conclude they are the problem.
    pub async fn plan_steps(
        &self,
        client: &reqwest::Client,
        task: &str,
        page_title: &str,
        page_text: &str,
    ) -> Plan {
        let excerpt: String = page_text.chars().take(MAX_ANSWER_CONTEXT).collect();
        if excerpt.trim().is_empty() {
            return Plan::default();
        }

        let system = "You convert how-to instructions into a click-by-click \
            walkthrough on the product's own website. Reply with ONLY a JSON \
            object, no prose, no code fences: \
            {\"start_url\": \"the URL on the product's own site where the \
            user should begin, taken from the page text, or empty\", \
            \"steps\": [{\"find\": \"exact button or link text to \
            click\", \"instruction\": \"short imperative\", \
            \"url_hint\": \"domain or path fragment, or empty\"}]}. \
            start_url must be a page on the product itself (for example \
            youtube.com/paid_memberships), never a help or support article. \
            Use ONLY button and link text that appears in the page text \
            given. Maximum 6 steps. If the page describes no click-by-click \
            process, use an empty steps array.";
        let user = format!(
            "Task: {task}

Page title: {page_title}

Page text:
{excerpt}",
        );

        let Some(reply) = self.chat(client, system, &user, 1500).await else {
            return Plan::default();
        };
        parse_plan(&reply)
    }
}

/// Pull the plan out of a model reply.
///
/// Models wrap JSON in prose or code fences however firmly you ask them not
/// to, so the object is located by brace rather than by trusting the reply to
/// be well-formed. A bare array is still accepted, because that is what an
/// earlier version of the prompt asked for and a stale reply should degrade
/// rather than crash.
fn parse_plan(reply: &str) -> Plan {
    if let (Some(a), Some(b)) = (reply.find('{'), reply.rfind('}')) {
        if b > a {
            if let Ok(p) = serde_json::from_str::<Plan>(&reply[a..=b]) {
                // Both fields carry `#[serde(default)]`, so *any* JSON object
                // deserialises into an empty Plan — including a bare array of
                // steps, whose first `{` and last `}` bracket one step rather
                // than the plan. Accepting that silently threw the steps away
                // and reported success. Require the result to actually carry
                // something before trusting it.
                if !p.steps.is_empty() || !p.start_url.is_empty() {
                    return p;
                }
            }
        }
    }
    if let (Some(a), Some(b)) = (reply.find('['), reply.rfind(']')) {
        if b > a {
            if let Ok(steps) = serde_json::from_str::<Vec<GuideStep>>(&reply[a..=b]) {
                return Plan { start_url: String::new(), steps };
            }
        }
    }
    tracing::warn!("plan unparseable");
    Plan::default()
}

/// Page text sent when answering a question.
///
/// The model's context window is a million tokens, but the cost that matters
/// is latency, not money: this call sits on the critical path of a query that
/// already takes seconds. The answer to a factual question is almost always
/// near the top of a retrieved page.
const MAX_ANSWER_CONTEXT: usize = 12_000;

/// Does this query want an answer rather than a link?
///
/// Deliberately conservative. Navigational queries are the product; treating
/// "flipkart customer service" as a question and printing a paragraph about
/// Flipkart's support hours would be a regression, not a feature. Only
/// queries that actually look interrogative qualify.
pub fn looks_like_question(query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.ends_with('?') {
        return true;
    }
    const OPENERS: &[&str] = &[
        "how many", "how much", "how long", "how old", "how tall", "how far",
        "how do", "how does", "how to", "how can", "how is", "how are",
        "what is", "what are", "what was", "what does", "what do",
        "who is", "who was", "who are", "when is", "when was", "when did",
        "where is", "where was", "why is", "why does", "why did",
        "which is", "can i", "do i", "does ",
    ];
    OPENERS.iter().any(|o| q.starts_with(o))
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    /// Null when the model spent its whole budget on reasoning tokens.
    ///
    /// Reasoning models emit a hidden `reasoning` stream before any visible
    /// content, and it counts against `max_tokens`. A tight budget therefore
    /// returns `finish_reason: "stop"` with `content: null` — a success-shaped
    /// failure that deserialises into a panic, or silently falls back, and
    /// looks like the feature was never wired up.
    #[serde(default)]
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_questions() {
        assert!(looks_like_question("how many runs did virat kohli score in australia"));
        assert!(looks_like_question("what is a cname record"));
        assert!(looks_like_question("is netflix down?"));
        assert!(looks_like_question("how to cancel netflix"));
    }

    /// The product is navigational search. Misreading a navigational query as
    /// a question and answering it in prose would be a regression.
    #[test]
    fn navigational_queries_are_not_questions() {
        assert!(!looks_like_question("flipkart customer service"));
        assert!(!looks_like_question("valuepickr bajaj finance"));
        assert!(!looks_like_question("saint edmunds school shillong"));
        assert!(!looks_like_question("cloudflare dns records"));
    }

    #[test]
    fn parses_a_plan_wrapped_in_prose_and_fences() {
        let reply = concat!(
            "Sure!\n```json\n{\"start_url\":\"https://youtube.com/paid_memberships\",",
            "\"steps\":[{\"find\":\"Manage membership\",\"instruction\":\"Open\",\"url_hint\":\"youtube.com\"},",
            "{\"find\":\"Cancel membership\",\"instruction\":\"Cancel\"}]}\n```\nHope that helps!"
        );
        let p = parse_plan(reply);
        assert_eq!(p.start_url, "https://youtube.com/paid_memberships");
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.steps[1].url_hint, "");
    }

    /// An earlier prompt asked for a bare array; a stale reply must degrade.
    #[test]
    fn a_bare_array_is_still_accepted() {
        let p = parse_plan("[{\"find\":\"Cancel\",\"instruction\":\"Click cancel\"}]");
        assert_eq!(p.steps.len(), 1);
        assert!(p.start_url.is_empty());
    }

    #[test]
    fn a_reply_with_no_json_yields_no_steps() {
        assert!(parse_plan("I cannot help with that.").steps.is_empty());
    }

    #[test]
    fn absent_credentials_disable_the_feature() {
        if std::env::var("OPENROUTER_API_KEY").is_ok() {
            return; // developer machine has a key set
        }
        assert!(Llm::from_env().is_none());
    }
}
