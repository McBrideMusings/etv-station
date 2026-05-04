use serde::{Deserialize, Serialize};

use super::item::ItemConfig;

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuleConfig {
    LoopForever { items: Vec<ItemConfig> },
}

impl RuleConfig {
    pub fn name(&self) -> &'static str {
        match self {
            RuleConfig::LoopForever { .. } => "loop_forever",
        }
    }

    pub fn items(&self) -> &[ItemConfig] {
        match self {
            RuleConfig::LoopForever { items } => items,
        }
    }

    pub fn items_mut(&mut self) -> &mut Vec<ItemConfig> {
        match self {
            RuleConfig::LoopForever { items } => items,
        }
    }
}
