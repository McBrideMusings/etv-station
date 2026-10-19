//! The station's lineup as the dashboard's channel rail sees it: one key per
//! channel, in `station.yaml` order (`GET /api/lineup`).

use super::*;

/// One `pattern:` step of a channel — which pool it draws from and whether
/// that pool is scored by a plugin (has a `plugin:`), which is what makes its
/// taste profile editable. A channel with any such step is `scored` in the
/// lineup.
pub(super) struct PlanStep {
    pub(super) block_index: usize,
    pub(super) pool_name: String,
    pub(super) has_plugin: bool,
}

/// Every `pattern:` step across `channel`'s blocks, in block-then-pattern order.
pub(super) fn pattern_plan(channel: &config::ChannelConfig) -> Vec<PlanStep> {
    let mut out = Vec::new();
    for (block_index, block) in channel.rule.all_blocks().enumerate() {
        if !block.is_pattern() {
            continue;
        }
        for step in &block.pattern {
            let has_plugin = block
                .pools
                .iter()
                .find(|p| p.name == step.pool)
                .is_some_and(|p| p.plugin.is_some());
            out.push(PlanStep {
                block_index,
                pool_name: step.pool.clone(),
                has_plugin,
            });
        }
    }
    out
}

/// A channel's folder name (`034-syfy`).
pub(super) fn channel_key(config_path: &Path) -> String {
    config_path
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Each lineup entry's key in the dashboard's URLs: its folder name when
/// every folder name in the lineup is distinct, else every entry's path as
/// written relative to the station file, so two keys never name one channel.
pub(super) fn lineup_keys(lineup: &[PathBuf], base: &Path) -> Vec<String> {
    let folders: Vec<String> = lineup.iter().map(|p| channel_key(p)).collect();
    if folders.iter().collect::<HashSet<_>>().len() == folders.len() {
        return folders;
    }
    lineup
        .iter()
        .map(|p| p.strip_prefix(base).unwrap_or(p).display().to_string())
        .collect()
}

/// `GET /api/lineup`: the channel rail — every lineup entry in `station.yaml`
/// order, keyed as the dashboard's URLs key it. A channel that failed to load
/// carries its `error` and no number.
pub(super) fn api_lineup(state: &AppState) -> Result<(u16, Value), String> {
    let path = &state.options.station;
    let station =
        config::load_for_inspection(path).map_err(|e| format!("load {}: {e}", path.display()))?;
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let keys = lineup_keys(&station.lineup, &base);
    let channels: Vec<Value> = station
        .lineup
        .iter()
        .zip(&keys)
        .map(|(config_path, key)| {
            let loaded = station
                .channels
                .iter()
                .find(|c| c.config_path == *config_path);
            let error = station
                .dropped
                .iter()
                .find(|d| d.config_path == *config_path)
                .map(|d| d.reason.to_string());
            json!({
                "channel": key,
                "number": loaded.map(|c| c.config.number),
                "display_name": loaded.and_then(|c| c.config.display_name.clone()),
                "scored": loaded.is_some_and(|c| pattern_plan(&c.config).iter().any(|s| s.has_plugin)),
                "error": error,
            })
        })
        .collect();
    tracing::info!(
        event = "ui.lineup",
        channels = channels.len(),
        "lineup listed"
    );
    Ok((200, json!({ "channels": channels })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineup_keys_are_folder_names_when_distinct() {
        let base = Path::new("/st");
        let lineup = [
            PathBuf::from("/st/channels/034-syfy/channel.yaml"),
            PathBuf::from("/st/channels/035-prestige/channel.yaml"),
        ];
        assert_eq!(lineup_keys(&lineup, base), ["034-syfy", "035-prestige"]);
    }

    #[test]
    fn lineup_keys_fall_back_to_paths_when_folders_collide() {
        let base = Path::new("/st");
        let lineup = [
            PathBuf::from("/st/channels/a.yaml"),
            PathBuf::from("/st/channels/b.yaml"),
        ];
        assert_eq!(
            lineup_keys(&lineup, base),
            ["channels/a.yaml", "channels/b.yaml"]
        );
    }
}
