//! Offline signal computation: everything the ranker needs that can't be
//! derived from a single document in isolation.
//!
//! The pipeline is deliberately a batch pass, not query-time work. Anchor
//! text, PageRank and duplicate clusters are all *global* properties of the
//! corpus — you cannot compute them while serving a query, which is exactly
//! why real search engines separate indexing-time from query-time scoring.

pub mod anchors;
pub mod pagerank;
pub mod quality;
