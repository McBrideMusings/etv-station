use std::time::Duration;

use ersatztv_playout::playout::{PlayoutItemSource, ProgramMetadata};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
pub struct ItemConfig {
    pub id: String,

    pub source: SourceConfig,

    #[serde(default, with = "humantime_serde")]
    pub in_point: Option<Duration>,

    #[serde(default, with = "humantime_serde")]
    pub out_point: Option<Duration>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<ProgramMetadata>,
}

#[derive(Debug, Deserialize, Serialize)]
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

impl ItemConfig {
    pub fn to_playout_source(&self) -> PlayoutItemSource {
        let in_point_ms = self.in_point.map(|d| d.as_millis() as u64);
        let out_point_ms = self.out_point.map(|d| d.as_millis() as u64);
        match &self.source {
            SourceConfig::Local { path } => PlayoutItemSource::Local {
                path: path.clone(),
                in_point_ms,
                out_point_ms,
                probe_hint: None,
            },
            SourceConfig::Lavfi { params } => PlayoutItemSource::Lavfi {
                params: params.clone(),
                probe_hint: None,
            },
            SourceConfig::Http {
                uri,
                headers,
                user_agent,
            } => PlayoutItemSource::Http {
                uri: uri.clone(),
                in_point_ms,
                out_point_ms,
                headers: headers.clone(),
                user_agent: user_agent.clone(),
                timeout_us: None,
                reconnect: None,
                reconnect_delay_max: None,
                keep_alive: None,
                probe_hint: None,
            },
        }
    }
}
