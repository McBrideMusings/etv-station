use std::time::Duration;

use ersatztv_playout::playout::PlayoutItemSource;
use serde::{Deserialize, Serialize};

/// A media source for a single item. Mirrors ETV-next's `PlayoutItemSource`
/// variants; `to_playout_source` performs the conversion.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceConfig {
    Local {
        path: String,
    },
    Lavfi {
        params: String,
    },
    Http {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user_agent: Option<String>,
    },
}

impl SourceConfig {
    pub fn to_playout_source(
        &self,
        in_point: Option<Duration>,
        out_point: Option<Duration>,
    ) -> PlayoutItemSource {
        let in_point_ms = in_point.map(|d| d.as_millis() as u64);
        let out_point_ms = out_point.map(|d| d.as_millis() as u64);
        match self {
            SourceConfig::Local { path } => {
                PlayoutItemSource::local(path.clone(), in_point_ms, out_point_ms)
            }
            SourceConfig::Lavfi { params } => PlayoutItemSource::lavfi(params.clone()),
            SourceConfig::Http {
                uri,
                headers,
                user_agent,
            } => PlayoutItemSource::http(
                uri.clone(),
                in_point_ms,
                out_point_ms,
                headers.clone(),
                user_agent.clone(),
            ),
        }
    }
}
