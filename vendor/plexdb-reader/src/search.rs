//! Shot search over the Frame index (ADR-0024, ADR-0026).
//!
//! A **Shot** is the stretch of a title between two adjacent boundaries. The
//! index numbers a title's Shots from 0 in `frame_vec.shot_ix` and stores each
//! Shot's `in_ms` and `out_ms` in `shots`. The harvester owns the rule that draws
//! the boundaries; this reader only reads what it stored.
//!
//! A frame's similarity to a phrase is `dot(phrase, frame) / |frame|`. The
//! phrase is a unit vector; the stored frame is an int8 vector of whatever
//! uniform scale the harvester chose, and dividing by its own length makes the
//! result the cosine whatever that scale was.

use std::collections::BTreeSet;

use rusqlite::params_from_iter;
use serde::Serialize;

use crate::Reader;
use crate::error::ReaderError;
use crate::phrase::IndexModel;

/// Midjourney's `--no` weight: what a negative subtracts when the caller gives none.
const DEFAULT_WEIGHT: f32 = 0.5;

/// One search: a Phrase over a pool of titles, with optional negatives.
#[derive(Debug, Clone, Copy)]
pub struct Search<'a> {
    /// The `item_id`s to search. Titles the Frame index has not indexed
    /// contribute no Shots and are counted in [`ShotSearch::pool`] only.
    pub pool: &'a [&'a str],
    pub phrase: &'a str,
    /// Phrases a Shot is penalised for resembling. Empty means none.
    pub negatives: &'a [&'a str],
    /// How much of the strongest negative a Shot loses; `None` means 0.5.
    pub weight: Option<f32>,
    /// `None` gives no probabilities.
    pub temperature: Option<f32>,
}

/// What a search found, best Shot first.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShotSearch {
    pub shots: Vec<ShotHit>,
    /// How many pool titles have a `frame_index_state` row that recorded a model.
    pub indexed: usize,
    /// How many distinct titles the pool names.
    pub pool: usize,
    /// The model the pool's index was built with; empty when no pool title is indexed.
    pub model: String,
}

/// One Shot and how it scored.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ShotHit {
    pub item_id: String,
    pub in_ms: u64,
    pub out_ms: u64,
    /// Each sampled frame's offset and similarity to the Phrase.
    pub frames: Vec<(u64, f32)>,
    /// The Shot's best frame against the Phrase.
    pub phrase_sim: f32,
    /// The Shot's best frame against each negative, in the order given.
    pub negative_sims: Vec<f32>,
    /// `phrase_sim - weight * max(negative_sims)`; `phrase_sim` with no negatives.
    pub score: f32,
    /// `exp(score / T) / sum` over the returned Shots, when a temperature was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p: Option<f32>,
}

/// What a Reader can do with the files it was opened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    /// True when `frames.db` is attached and a Phrase cache was given.
    pub search: bool,
}

struct IndexRow {
    item_id: String,
    model: String,
    model_version: String,
    dims: usize,
}

impl Reader {
    /// Rank every Shot of the pool's indexed titles against the Phrase.
    ///
    /// Shots come back ordered by score, ties by `(item_id, in_ms)`, so the same
    /// inputs give the same output. The Phrase and each negative are embedded
    /// once and kept in the Phrase cache.
    pub fn search_shots(&self, q: &Search<'_>) -> Result<ShotSearch, ReaderError> {
        if !self.frames {
            return Err(ReaderError::FramesNotAttached);
        }
        let cache = self.phrases.as_ref().ok_or(ReaderError::NoPhraseCache)?;
        let weight = q.weight.unwrap_or(DEFAULT_WEIGHT);
        if !weight.is_finite() {
            return Err(ReaderError::InvalidSearch(
                "weight must be a finite number".into(),
            ));
        }
        if let Some(t) = q.temperature
            && !(t.is_finite() && t > 0.0)
        {
            return Err(ReaderError::InvalidSearch(
                "temperature must be a number above 0".into(),
            ));
        }
        let phrase = crate::normalize_surface(q.phrase);
        if phrase.is_empty() {
            return Err(ReaderError::InvalidSearch("the phrase is empty".into()));
        }
        let mut wanted = vec![phrase];
        for negative in q.negatives {
            let negative = crate::normalize_surface(negative);
            if negative.is_empty() {
                return Err(ReaderError::InvalidSearch("a negative is empty".into()));
            }
            wanted.push(negative);
        }

        let pool: BTreeSet<&str> = q.pool.iter().copied().collect();
        let rows = self.index_rows(&pool)?;
        let models: BTreeSet<(&str, &str, usize)> = rows
            .iter()
            .map(|r| (r.model.as_str(), r.model_version.as_str(), r.dims))
            .collect();
        let mut hits = Vec::new();
        let model = match models.len() {
            0 => String::new(),
            1 => {
                let (model, model_version, dims) = *models.iter().next().expect("one model");
                let vectors = cache.vectors(
                    &wanted,
                    &IndexModel {
                        model,
                        model_version,
                        dims,
                    },
                )?;
                for row in &rows {
                    hits.extend(self.shots_of(row, &vectors)?);
                }
                model.to_string()
            }
            _ => {
                let names: Vec<String> = models
                    .iter()
                    .map(|(m, v, _)| format!("{m} {v}").trim().to_string())
                    .collect();
                return Err(ReaderError::MixedModels(names.join(", ")));
            }
        };

        for hit in &mut hits {
            let worst = hit.negative_sims.iter().copied().reduce(f32::max);
            hit.score = match worst {
                Some(worst) => hit.phrase_sim - weight * worst,
                None => hit.phrase_sim,
            };
        }
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.item_id.cmp(&b.item_id))
                .then_with(|| a.in_ms.cmp(&b.in_ms))
        });
        if let Some(t) = q.temperature {
            let top = hits.first().map_or(0.0, |h| h.score);
            let weights: Vec<f64> = hits
                .iter()
                .map(|h| f64::from((h.score - top) / t).exp())
                .collect();
            let total: f64 = weights.iter().sum();
            for (hit, w) in hits.iter_mut().zip(weights) {
                hit.p = Some((w / total) as f32);
            }
        }
        Ok(ShotSearch {
            shots: hits,
            indexed: rows.len(),
            pool: pool.len(),
            model,
        })
    }

    /// Pick `n` Shots from a search's results.
    ///
    /// With a temperature the search carried probabilities: `n` Shots are drawn
    /// without replacement, each weighted by `p`, from a generator seeded by
    /// `seed`, so the same results, `n` and seed give the same picks in the same
    /// order. Without one, the top `n` come back.
    #[must_use]
    pub fn draw(&self, results: &ShotSearch, n: usize, seed: u64) -> Vec<ShotHit> {
        draw(&results.shots, n, seed)
    }

    /// Every Cut of a title, ascending: scene changes and chapter marks together.
    pub fn cuts(&self, item_id: &str) -> Result<Vec<u64>, ReaderError> {
        self.cut_times(item_id, false)
    }

    /// The title's chapter marks, ascending.
    pub fn chapters(&self, item_id: &str) -> Result<Vec<u64>, ReaderError> {
        self.cut_times(item_id, true)
    }

    /// What this Reader can do with the files it was opened on.
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            search: self.frames && self.phrases.is_some(),
        }
    }

    fn cut_times(&self, item_id: &str, chapters_only: bool) -> Result<Vec<u64>, ReaderError> {
        if !self.frames {
            return Err(ReaderError::FramesNotAttached);
        }
        let filter = if chapters_only {
            "AND source = 'chapter'"
        } else {
            ""
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT t_ms FROM frames.cuts \
             WHERE item_id = ?1 AND t_ms > 0 {filter} ORDER BY t_ms"
        ))?;
        let rows = stmt
            .query_map((item_id,), |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|t| u64::try_from(t).ok())
            .collect())
    }

    /// The pool's `frame_index_state` rows that recorded a model, in `item_id` order: a
    /// title whose every attempt failed holds no model and no vectors, so it is not indexed.
    /// A title that failed a re-index keeps its earlier rows and is still searched.
    fn index_rows(&self, pool: &BTreeSet<&str>) -> Result<Vec<IndexRow>, ReaderError> {
        let ids: Vec<&str> = pool.iter().copied().collect();
        let max_vars = self
            .conn
            .limit(rusqlite::limits::Limit::SQLITE_LIMIT_VARIABLE_NUMBER);
        let chunk_size = usize::try_from(max_vars).unwrap_or(1).max(1);
        let mut rows = Vec::new();
        for chunk in ids.chunks(chunk_size) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let mut stmt = self.conn.prepare(&format!(
                "SELECT item_id, model, model_version, dims \
                 FROM frames.frame_index_state \
                 WHERE item_id IN ({placeholders}) AND model IS NOT NULL ORDER BY item_id"
            ))?;
            let found = stmt
                .query_map(params_from_iter(chunk.iter()), |row| {
                    Ok(IndexRow {
                        item_id: row.get(0)?,
                        model: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        model_version: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        dims: usize::try_from(row.get::<_, Option<i64>>(3)?.unwrap_or(0))
                            .unwrap_or(0),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.extend(found);
        }
        Ok(rows)
    }

    /// The `(in_ms, out_ms)` of each Shot of a title, indexed by `shot_ix`.
    fn shot_bounds(&self, item_id: &str) -> Result<Vec<(u64, u64)>, ReaderError> {
        let mut stmt = self.conn.prepare(
            "SELECT in_ms, out_ms FROM frames.shots WHERE item_id = ?1 ORDER BY shot_ix",
        )?;
        let rows = stmt
            .query_map((item_id,), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(a, b)| match (u64::try_from(a), u64::try_from(b)) {
                (Ok(a), Ok(b)) if a <= b => Ok((a, b)),
                _ => Err(ReaderError::InconsistentIndex {
                    item_id: item_id.to_string(),
                    detail: format!("a Shot spanning {a}..{b} ms"),
                }),
            })
            .collect()
    }

    /// Every Shot of one title, scored against the Phrase and each negative.
    /// `vectors[0]` is the Phrase, the rest are the negatives in order.
    fn shots_of(&self, row: &IndexRow, vectors: &[Vec<f32>]) -> Result<Vec<ShotHit>, ReaderError> {
        let mut stmt = self
            .conn
            .prepare("SELECT shot_ix, t_ms, vec FROM frames.frame_vec WHERE item_id = ?1")?;
        let mut frames: Vec<(i64, u64, Vec<i8>)> = stmt
            .query_map((row.item_id.as_str(),), |r| {
                let blob: Vec<u8> = r.get(2)?;
                Ok((
                    r.get::<_, i64>(0)?,
                    u64::try_from(r.get::<_, i64>(1)?).unwrap_or(0),
                    blob.into_iter().map(|b| b as i8).collect(),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        frames.sort_by_key(|(shot_ix, t_ms, _)| (*shot_ix, *t_ms));

        let bounds = self.shot_bounds(&row.item_id)?;

        let mut hits: Vec<ShotHit> = Vec::new();
        let mut current: Option<i64> = None;
        for (shot_ix, t_ms, vec) in &frames {
            if current != Some(*shot_ix) {
                let ix = usize::try_from(*shot_ix)
                    .ok()
                    .filter(|ix| *ix < bounds.len())
                    .ok_or_else(|| ReaderError::InconsistentIndex {
                        item_id: row.item_id.clone(),
                        detail: format!("shot_ix {shot_ix} but {} shot(s)", bounds.len()),
                    })?;
                hits.push(ShotHit {
                    item_id: row.item_id.clone(),
                    in_ms: bounds[ix].0,
                    out_ms: bounds[ix].1,
                    frames: Vec::new(),
                    phrase_sim: f32::NEG_INFINITY,
                    negative_sims: vec![f32::NEG_INFINITY; vectors.len() - 1],
                    score: 0.0,
                    p: None,
                });
                current = Some(*shot_ix);
            }
            if vec.len() != row.dims {
                return Err(ReaderError::InconsistentIndex {
                    item_id: row.item_id.clone(),
                    detail: format!("a {}-wide vector, index records {}", vec.len(), row.dims),
                });
            }
            let hit = hits.last_mut().expect("a shot was just pushed");
            let norm = vec
                .iter()
                .map(|&x| f32::from(x) * f32::from(x))
                .sum::<f32>()
                .sqrt();
            let sim = |q: &[f32]| -> f32 {
                if norm == 0.0 || q.len() != vec.len() {
                    return 0.0;
                }
                q.iter()
                    .zip(vec)
                    .map(|(a, &b)| a * f32::from(b))
                    .sum::<f32>()
                    / norm
            };
            let phrase_sim = sim(&vectors[0]);
            hit.frames.push((*t_ms, phrase_sim));
            hit.phrase_sim = hit.phrase_sim.max(phrase_sim);
            for (slot, q) in hit.negative_sims.iter_mut().zip(&vectors[1..]) {
                *slot = slot.max(sim(q));
            }
        }
        Ok(hits)
    }
}

fn draw(shots: &[ShotHit], n: usize, seed: u64) -> Vec<ShotHit> {
    let n = n.min(shots.len());
    if shots.iter().any(|s| s.p.is_none()) {
        return shots[..n].to_vec();
    }
    let mut weights: Vec<f64> = shots
        .iter()
        .map(|s| f64::from(s.p.unwrap_or(0.0)))
        .collect();
    let mut taken = vec![false; shots.len()];
    let mut rng = SplitMix64(seed);
    let mut picked = Vec::with_capacity(n);
    for _ in 0..n {
        let total: f64 = weights.iter().sum();
        let ix = if total > 0.0 {
            let target = rng.unit() * total;
            let mut acc = 0.0;
            let mut chosen = 0;
            for (i, w) in weights.iter().enumerate() {
                if *w > 0.0 {
                    acc += w;
                    chosen = i;
                    if acc > target {
                        break;
                    }
                }
            }
            chosen
        } else {
            // Every remaining probability underflowed to 0: take them in rank order.
            taken
                .iter()
                .position(|t| !t)
                .expect("fewer picks than shots")
        };
        weights[ix] = 0.0;
        taken[ix] = true;
        picked.push(shots[ix].clone());
    }
    picked
}

/// Steele, Lea & Flood's SplitMix64: a tiny generator whose stream is fixed by
/// its seed, so a draw is reproducible without a dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}
