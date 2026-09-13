//! Local ("near me") search: a places vertical alongside the web index.
//!
//! This is deliberately a *separate* index, not more documents in the web
//! index. The two answer different questions and rank on different signals —
//! a web page has no coordinates, and a restaurant has no inbound links.
//! Conflating them means one ranking function serving two intents badly.

pub mod cache;
pub mod fetch;
pub mod gazetteer;
pub mod geo;
pub mod poi;
pub mod schema;
pub mod search;
pub mod synonyms;
