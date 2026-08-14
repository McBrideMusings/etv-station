//! Typed rows returned by [`crate::Reader`]'s accessors.
//!
//! Deliberately concrete structs rather than a dynamic row type: a schema
//! change that drops a field a consumer reads fails that consumer's
//! **build**, not a runtime query built from a string (ADR-0003).

/// One namespaced, opaque fact about a title — a row of `enrichment`.
///
/// The store never interprets `key`/`value`; neither does this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrichmentFact {
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub fetched_at: String,
}

/// One directed, typed, ranked relationship between two titles — a row of
/// `edges`. A snapshot of what a source said at one moment, not a fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub from_id: String,
    pub to_id: String,
    pub edge_type: String,
    pub rank: i64,
    pub fetched_at: String,
}

/// One attribute in a user's Layer 2 taste vector, and how strongly their
/// watch history carries it.
///
/// `weight` is the sum, over every title the account watched, of
/// `sqrt(r) / attributes_on_that_title`, where `r` is that account's
/// consumption of the title measured in **seasons** — a film, or one full
/// season of a show, is `r = 1`. Titles below `r = 0.5` contribute nothing.
/// Dividing by the title's attribute count stops a heavily-tagged title
/// outvoting a sparsely-tagged one (plex-db-ex ADR-0011).
///
/// **Still no ranking policy here**, and now on evidence rather than by
/// deferral: measured against 25,835 real plays, a recency half-life
/// concentrated the vector onto one recent show rather than mixing it, and an
/// abandoned watch is no signal rather than negative signal. The exploration
/// fraction describes how a *channel* is assembled, not what a person likes,
/// so it stays with the consumer. There is no constant in here to tune.
#[derive(Debug, Clone, PartialEq)]
pub struct TasteAttribute {
    pub namespace: String,
    pub key: String,
    pub value: String,
    pub weight: f64,
}

/// One crowd list a title appears on, and where in it — a join of one
/// `collection_membership` row with the `collection` row it belongs to.
///
/// `collection_id`, `source`, and `observed_at` (the membership's own, not
/// the list's) are always present; every other field is nullable at the
/// source and stays `Option<_>` here rather than collapsing to `0` or `""`,
/// because a missing `rank` must stay distinguishable from rank 1 and a
/// missing `name`/`url`/`size`/`likes` from a source that genuinely recorded
/// none. There is deliberately no `weight` field — `rank`, `size`, `likes`
/// and `mentions` are the facts a source gave; a consumer wanting one number
/// computes it from those (plex-db-ex#33, ADR-0012).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionMembership {
    pub collection_id: String,
    pub source: String,
    pub name: Option<String>,
    pub url: Option<String>,
    pub size: Option<i64>,
    pub likes: Option<i64>,
    pub rank: Option<i64>,
    pub mentions: Option<i64>,
    pub observed_at: String,
}

/// A user's Layer 2 taste vector, plus what the rollup could not weigh
/// properly.
///
/// `shows_without_seasons` names every show whose episodes Plex files with no
/// season number. There is no season length to divide by, so each play counts
/// as one unit — which over-weights a long show. Reported rather than
/// silently folded in, because the alternative is a vector that is quietly
/// wrong in a way nothing downstream can detect.
#[derive(Debug, Clone, PartialEq)]
pub struct TasteVector {
    pub attributes: Vec<TasteAttribute>,
    pub shows_without_seasons: Vec<String>,
}
