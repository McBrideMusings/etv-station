//! `GET /api/airings/{airing}/audit`: the stored audit record of one real
//! airing — what the guide shows on a channel, not a re-simulation.
//!
//! # The airing id
//!
//! `{channel number}.{start}`: the channel's dial number and the airing's start
//! as whole Unix seconds, e.g. `34.1789000000`. A guide programme carries both —
//! the number in its channel's XMLTV id (`ersatztv.34`) and the start in its
//! `start` attribute, which XMLTV can only express to the second. The channel
//! chunk file keeps the start to the microsecond, so the record is the item whose
//! start falls within that second.
//!
//! # Where the record is read from
//!
//! The audit trail rides in each playout item's `metadata.audit` in the
//! channel's chunk files under its output folder (`audit_report`), written when
//! the airing was generated. `history.db` records which entry aired and when,
//! and holds no audit trail, so it is not consulted. Nothing is written.
//!
//! # Answers
//!
//! 200 `{airing, channel, number, start, finish, item, stages, titles}`; 400 when
//! the id is malformed; 404 when no lineup channel has that number or its chunk
//! files hold no item starting then (the chunk was swept, or the guide is stale).

use super::*;

/// Parse `{number}.{start}`.
fn parse_airing(airing: &str) -> Option<(i64, i64)> {
    let (number, start) = airing.split_once('.')?;
    Some((number.parse().ok()?, start.parse().ok()?))
}

pub(super) fn api_airing_audit(state: &AppState, airing: &str) -> Result<(u16, Value), String> {
    let began = std::time::Instant::now();
    let Some((number, start_secs)) = parse_airing(airing) else {
        return Ok((
            400,
            json!({ "error": format!("airing {airing:?} is not {{channel number}}.{{start unix seconds}}") }),
        ));
    };
    let path = &state.options.station;
    let station =
        config::load_for_inspection(path).map_err(|e| format!("load {}: {e}", path.display()))?;
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let keys = lineup::lineup_keys(&station.lineup, &base);
    let found = station
        .lineup
        .iter()
        .zip(&keys)
        .find_map(|(config_path, key)| {
            station
                .channels
                .iter()
                .find(|c| c.config_path == *config_path && c.config.number == number)
                .map(|c| (key, c))
        });
    let Some((key, lc)) = found else {
        return Ok((
            404,
            json!({ "error": format!("no lineup channel numbered {number} in {}", path.display()) }),
        ));
    };
    let Ok(second) = time::OffsetDateTime::from_unix_timestamp(start_secs) else {
        return Ok((
            400,
            json!({ "error": format!("start {start_secs} is not a representable time") }),
        ));
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("start a runtime to read chunk files: {e}"))?;
    let item = runtime
        .block_on(crate::audit_report::item_at(&lc.output_folder, second))
        .map_err(|e| e.to_string())?;
    let Some(item) = item else {
        tracing::info!(
            event = "ui.airing_audit",
            airing,
            channel = %key,
            found = false,
            "airing audit: no item starts then"
        );
        return Ok((
            404,
            json!({ "error": format!(
                "channel {key} has no item starting at {start_secs} in {}",
                lc.output_folder.display()
            ) }),
        ));
    };

    let (audit, _) = crate::audit_report::split_audit(item.metadata.as_ref());
    let entry = state.catalog.entry(&item.id).ok().flatten();
    let titles = channel::near_miss_titles(&state.catalog, std::slice::from_ref(&audit), |_| true)?;
    let stages = audit.as_array().map(Vec::len).unwrap_or(0);
    tracing::info!(
        event = "ui.airing_audit",
        airing,
        channel = %key,
        found = true,
        item = %item.id,
        stages,
        ms = began.elapsed().as_millis() as u64,
        "airing audit served"
    );
    Ok((
        200,
        json!({
            "airing": airing,
            "channel": key,
            "number": number,
            "start": item.start.unix_timestamp(),
            "finish": item.finish.unix_timestamp(),
            "item": {
                "id": item.id,
                "label": entry
                    .as_ref()
                    .map(channel::airing_label)
                    .or(item.title.clone())
                    .unwrap_or_else(|| item.id.clone()),
                "title": entry.as_ref().map(|e| e.title.clone()).or(item.title.clone()),
                "show": entry.as_ref().and_then(|e| e.show.clone()),
                "kind": entry.as_ref().map(|e| e.kind.clone()),
                "season": entry.as_ref().and_then(|e| e.season),
                "episode": entry.as_ref().and_then(|e| e.episode),
                "year": entry.as_ref().and_then(|e| e.year),
                "content_rating": entry.as_ref().and_then(|e| e.content_rating.clone()),
                "summary": entry.as_ref().and_then(|e| e.summary.clone()),
            },
            "stages": audit,
            "titles": titles,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ersatztv_playout::playout::{Playout, PlayoutItem, ProgramMetadata};

    #[test]
    fn an_airing_id_is_a_number_and_a_start() {
        assert_eq!(parse_airing("34.1789000000"), Some((34, 1_789_000_000)));
        assert_eq!(parse_airing("34"), None);
        assert_eq!(parse_airing("x.1"), None);
        assert_eq!(parse_airing("34.soon"), None);
    }

    /// The example test station with its output folder moved under `dir`, and
    /// one chunk file holding an item that starts at `start` plus microseconds.
    fn state_with_one_airing(dir: &Path, start: time::OffsetDateTime) -> (AppState, i64) {
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let yaml = std::fs::read_to_string(examples.join("station-test.yaml"))
            .expect("read the example station")
            .replace(
                "output_base: examples/output",
                &format!("output_base: {}", dir.join("out").display()),
            )
            .replace(
                "channels/lavfi-test.yaml",
                &examples
                    .join("channels/lavfi-test.yaml")
                    .display()
                    .to_string(),
            );
        let station_path = dir.join("station.yaml");
        std::fs::write(&station_path, yaml).expect("write the station");
        let station = config::load_for_inspection(&station_path).expect("station loads");
        let lc = &station.channels[0];
        std::fs::create_dir_all(&lc.output_folder).unwrap();

        let item_start = start + time::Duration::microseconds(678_777);
        let item = PlayoutItem {
            id: "entry-1".to_string(),
            start: item_start,
            finish: item_start + time::Duration::minutes(30),
            source: None,
            tracks: None,
            watermark: None,
            program: Some(ProgramMetadata {
                title: Some("Stored Title".to_string()),
                ..Default::default()
            }),
            metadata: Some(json!({ "audit": [
                { "stage": "pool", "by": "taste-cosine", "verdict": "picked",
                  "detail": { "rank": 3 } }
            ] })),
            overlay: None,
            normalize_loudness: None,
        };
        let name = crate::emit::chunk_filename(start, start + time::Duration::hours(1)).unwrap();
        std::fs::write(
            lc.output_folder.join(name),
            serde_json::to_vec(&Playout::new(vec![item])).unwrap(),
        )
        .unwrap();

        let state = AppState {
            catalog: Catalog::open_in_memory().expect("in-memory catalog"),
            pools: PoolState::default(),
            review: ReviewState::new(
                station_path.clone(),
                PathBuf::from("/nonexistent/catalog.db"),
            ),
            options: Options {
                station: station_path,
                catalog: PathBuf::from("/nonexistent/catalog.db"),
                plugins_dir: None,
                bind: IpAddr::from([127, 0, 0, 1]),
                port: 0,
                health: HealthSources::default(),
            },
        };
        (state, lc.config.number)
    }

    fn get(state: &mut AppState, path: &str) -> (u16, Value) {
        match dispatch(state, &Method::Get, path, "", "") {
            Reply::Json(status, body) => (status, body),
            _ => panic!("{path} did not answer JSON"),
        }
    }

    #[test]
    fn the_endpoint_returns_the_stored_record_for_a_known_airing_and_404s_an_unknown_one() {
        let dir = tempfile::tempdir().unwrap();
        let start = time::macros::datetime!(2030-01-02 03:00:00 UTC);
        let (mut state, number) = state_with_one_airing(dir.path(), start);

        let id = format!("{number}.{}", start.unix_timestamp());
        let (status, body) = get(&mut state, &format!("/api/airings/{id}/audit"));
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["airing"], id);
        assert_eq!(body["number"], number);
        assert_eq!(body["start"], start.unix_timestamp());
        assert_eq!(body["item"]["id"], "entry-1");
        assert_eq!(body["item"]["label"], "Stored Title");
        assert_eq!(body["stages"][0]["by"], "taste-cosine");
        assert_eq!(body["stages"][0]["detail"]["rank"], 3);

        let unknown = format!("{number}.{}", start.unix_timestamp() + 5);
        let (status, body) = get(&mut state, &format!("/api/airings/{unknown}/audit"));
        assert_eq!(status, 404, "{body}");
        assert!(body["error"].as_str().unwrap().contains("no item starting"));

        let (status, body) = get(
            &mut state,
            &format!("/api/airings/9999.{}/audit", start.unix_timestamp()),
        );
        assert_eq!(status, 404, "{body}");

        let (status, _) = get(&mut state, "/api/airings/nonsense/audit");
        assert_eq!(status, 400);
        let (status, _) = get(
            &mut state,
            &format!("/api/airings/{number}.99999999999999/audit"),
        );
        assert_eq!(status, 400);
    }
}
