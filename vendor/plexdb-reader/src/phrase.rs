//! The Phrase cache (ADR-0026): a SQLite file the Reader's caller names, holding
//! one unit-length vector per `(phrase, model, model_version)`.
//!
//! It is the only file this crate writes. A phrase is looked up after the
//! keyword spelling rule; on a miss the Reader POSTs it to the caller's encoder
//! and stores the answer. A hit stamps `last_used_at`; a row unused for
//! [`MAX_UNUSED_DAYS`] days is deleted before every lookup.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use url::Url;

use crate::error::ReaderError;

/// A cached vector unused this long is deleted (ADR-0026).
pub const MAX_UNUSED_DAYS: u32 = 90;

/// Long enough to wait out a broker loading the model onto a GPU; the writer's
/// own client waits the same (`plexdb/frame_encoder.py::TIMEOUT_S`).
const ENCODER_TIMEOUT: Duration = Duration::from_secs(300);

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%SZ', 'now')";

/// Where a Reader keeps the vectors of phrases it has embedded, and the encoder
/// it asks for a phrase it has not.
#[derive(Debug, Clone)]
pub struct PhraseCache {
    pub path: PathBuf,
    /// An OpenAI-compatible base URL; the Reader POSTs to `<encoder_url>/v1/embeddings`.
    pub encoder_url: Url,
    /// The model name the encoder accepts (`PLEXDB_FRAME_MODEL`). It is sent as the
    /// request's `model` (empty means unset: a phrase that needs the encoder fails); the answer must name the model the Frame index recorded.
    pub request_model: String,
}

/// The model a vector must come from, as `frame_index_state` records it.
pub(crate) struct IndexModel<'a> {
    pub model: &'a str,
    pub model_version: &'a str,
    pub dims: usize,
}

impl PhraseCache {
    /// One unit-length vector per phrase, in the order given. Each phrase is
    /// already in the keyword spelling.
    pub(crate) fn vectors(
        &self,
        phrases: &[String],
        index: &IndexModel<'_>,
    ) -> Result<Vec<Vec<f32>>, ReaderError> {
        let conn = self.open()?;
        let at = |source| ReaderError::PhraseCache {
            path: self.path.clone(),
            source,
        };
        conn.execute(
            &format!(
                "DELETE FROM phrase_cache \
                 WHERE last_used_at < strftime('%Y-%m-%dT%H:%M:%SZ', 'now', '-{MAX_UNUSED_DAYS} days')"
            ),
            [],
        )
        .map_err(at)?;

        let mut found: BTreeMap<&str, Vec<f32>> = BTreeMap::new();
        let mut missing: Vec<&str> = Vec::new();
        for phrase in phrases {
            let phrase = phrase.as_str();
            if found.contains_key(phrase) || missing.contains(&phrase) {
                continue;
            }
            match self.lookup(&conn, phrase, index).map_err(at)? {
                Some(vec) => {
                    found.insert(phrase, vec);
                }
                None => missing.push(phrase),
            }
        }

        if !missing.is_empty() {
            let embedded = self.embed(&missing, index)?;
            for (phrase, vec) in missing.iter().zip(embedded) {
                conn.execute(
                    &format!(
                        "INSERT OR REPLACE INTO phrase_cache \
                         (phrase, model, model_version, vec, created_at, last_used_at) \
                         VALUES (?1, ?2, ?3, ?4, {NOW}, {NOW})"
                    ),
                    params![phrase, index.model, index.model_version, to_blob(&vec)],
                )
                .map_err(at)?;
                found.insert(phrase, vec);
            }
        }

        Ok(phrases
            .iter()
            .map(|phrase| found[phrase.as_str()].clone())
            .collect())
    }

    fn open(&self) -> Result<Connection, ReaderError> {
        let at = |source| ReaderError::PhraseCache {
            path: self.path.clone(),
            source,
        };
        let conn = Connection::open(&self.path).map_err(at)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(at)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS phrase_cache (
                 phrase        TEXT NOT NULL,
                 model         TEXT NOT NULL,
                 model_version TEXT NOT NULL,
                 vec           BLOB NOT NULL,
                 created_at    TEXT NOT NULL,
                 last_used_at  TEXT NOT NULL,
                 PRIMARY KEY (phrase, model, model_version)
             ) WITHOUT ROWID;",
        )
        .map_err(at)?;
        Ok(conn)
    }

    /// The cached vector, stamping `last_used_at`. A row of the wrong width is
    /// no hit: it is replaced by the next embed.
    fn lookup(
        &self,
        conn: &Connection,
        phrase: &str,
        index: &IndexModel<'_>,
    ) -> Result<Option<Vec<f32>>, rusqlite::Error> {
        let key = params![phrase, index.model, index.model_version];
        let blob: Option<Vec<u8>> = conn
            .query_row(
                "SELECT vec FROM phrase_cache \
                 WHERE phrase = ?1 AND model = ?2 AND model_version = ?3",
                key,
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some(blob) = blob else { return Ok(None) };
        if blob.len() != index.dims * 4 {
            return Ok(None);
        }
        conn.execute(
            &format!(
                "UPDATE phrase_cache SET last_used_at = {NOW} \
                 WHERE phrase = ?1 AND model = ?2 AND model_version = ?3"
            ),
            key,
        )?;
        Ok(Some(from_blob(&blob)))
    }

    /// One request for every phrase the cache lacks.
    fn embed(
        &self,
        phrases: &[&str],
        index: &IndexModel<'_>,
    ) -> Result<Vec<Vec<f32>>, ReaderError> {
        if self.request_model.is_empty() {
            return Err(ReaderError::InvalidSearch(
                "no encoder model: pass --encoder-model or set PLEXDB_FRAME_MODEL".into(),
            ));
        }
        let url = format!(
            "{}/v1/embeddings",
            self.encoder_url.as_str().trim_end_matches('/')
        );
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(ENCODER_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        let body = json!({
            "model": self.request_model,
            "input": phrases,
            "modality": "text",
            "encoding_format": "float",
        });
        let mut response = agent.post(&url).send_json(&body).map_err(|err| {
            ReaderError::EncoderUnavailable(format!("encoder unavailable at {url}: {err}"))
        })?;
        let status = response.status().as_u16();
        let raw = response.body_mut().read_to_string().map_err(|err| {
            ReaderError::EncoderUnavailable(format!("encoder unavailable at {url}: {err}"))
        })?;
        if status >= 500 {
            return Err(ReaderError::EncoderUnavailable(format!(
                "encoder unavailable at {url}: {status}: {}",
                excerpt(&raw)
            )));
        }
        if status != 200 {
            return Err(ReaderError::EncoderFailed(format!(
                "the encoder at {url} refused the request: {status}: {}",
                excerpt(&raw)
            )));
        }
        parse(&raw, &url, phrases.len(), index)
    }
}

fn excerpt(raw: &str) -> String {
    raw.chars().take(200).collect()
}

/// OpenAI's `data[]` of `{index, embedding}`, scaled to unit length.
fn parse(
    raw: &str,
    url: &str,
    expected: usize,
    index: &IndexModel<'_>,
) -> Result<Vec<Vec<f32>>, ReaderError> {
    let dims = index.dims;
    let unusable =
        || ReaderError::EncoderFailed(format!("the encoder at {url} returned an unusable body"));
    let body: Value = serde_json::from_str(raw).map_err(|_| unusable())?;
    // An answer naming no model is not a mismatch; one naming another model is.
    if let Some(answered) = body.get("model").and_then(Value::as_str)
        && !answered.is_empty()
        && answered != index.model
    {
        return Err(ReaderError::EncoderFailed(format!(
            "the encoder at {url} answered as {answered}; the Frame index holds {}",
            index.model
        )));
    }
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(unusable)?;
    let mut rows: Vec<(u64, Vec<f64>)> = Vec::with_capacity(data.len());
    for row in data {
        let index = row
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(unusable)?;
        let embedding = row
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(unusable)?
            .iter()
            .map(|n| n.as_f64().ok_or_else(unusable))
            .collect::<Result<Vec<f64>, _>>()?;
        rows.push((index, embedding));
    }
    rows.sort_by_key(|(index, _)| *index);
    if rows.len() != expected {
        return Err(ReaderError::EncoderFailed(format!(
            "asked {url} for {expected} vector(s), it returned {}",
            rows.len()
        )));
    }
    rows.into_iter()
        .map(|(_, vec)| {
            if vec.len() != dims {
                return Err(ReaderError::EncoderFailed(format!(
                    "the encoder at {url} returned a {}-wide vector; the Frame index holds {dims}",
                    vec.len()
                )));
            }
            let norm = vec.iter().map(|x| x * x).sum::<f64>().sqrt();
            if !norm.is_finite() || norm <= 0.0 {
                return Err(ReaderError::EncoderFailed(format!(
                    "the encoder at {url} returned a zero or non-finite vector"
                )));
            }
            Ok(vec.iter().map(|x| (x / norm) as f32).collect())
        })
        .collect()
}

fn to_blob(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}
