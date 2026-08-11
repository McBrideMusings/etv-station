use std::collections::HashSet;
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use ersatztv::error::LineupError;
use ersatztv_playout::playout::{Credits, DATE_FORMAT, PlayoutItem, ProgramMetadata};
use quick_xml::Writer;
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;

use crate::LineupState;
use crate::channel_model::ChannelModel;

const XMLTV_DATETIME: &[FormatItem<'_>] = format_description!(
    "[year][month][day][hour][minute][second] [offset_hour sign:mandatory][offset_minute]"
);

pub fn format_xmltv_datetime(dt: OffsetDateTime) -> String {
    dt.format(XMLTV_DATETIME)
        .unwrap_or_else(|_| String::from("19700101000000 +0000"))
}

pub async fn xmltv_epg(
    State(state): State<Arc<LineupState>>,
) -> Result<impl IntoResponse, LineupError> {
    let mut sections: Vec<(String, Vec<PlayoutItem>)> = Vec::with_capacity(state.channels.len());
    for channel in &state.channels {
        let items = collect_items(channel).await;
        sections.push((channel.tvg_id().to_owned(), items));
    }

    let xml = build_xmltv(&state.channels, &sections)?;

    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "application/xml; charset=utf-8",
        )],
        xml,
    ))
}

fn build_xmltv(
    channels: &[ChannelModel],
    sections: &[(String, Vec<PlayoutItem>)],
) -> Result<String, LineupError> {
    let mut buf = Cursor::new(Vec::<u8>::new());
    {
        let mut w = Writer::new_with_indent(&mut buf, b' ', 2);
        w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
            .map_err(xml_err)?;

        let mut tv = BytesStart::new("tv");
        tv.push_attribute(("generator-info-name", "ersatztv-next"));
        w.write_event(Event::Start(tv)).map_err(xml_err)?;

        for channel in channels {
            write_channel(&mut w, channel)?;
        }

        for (tvg_id, items) in sections {
            for item in items {
                write_programme(&mut w, tvg_id, item)?;
            }
        }

        w.write_event(Event::End(BytesEnd::new("tv")))
            .map_err(xml_err)?;
    }

    String::from_utf8(buf.into_inner()).map_err(|e| LineupError::XmltvFailure(e.to_string()))
}

fn write_channel<W: std::io::Write>(
    w: &mut Writer<W>,
    channel: &ChannelModel,
) -> Result<(), LineupError> {
    let mut elem = BytesStart::new("channel");
    // Must be the same string the M3U publishes as tvg-id — clients join the
    // lineup to the guide on it, and a mismatch silently empties every row.
    elem.push_attribute(("id", channel.tvg_id()));
    w.write_event(Event::Start(elem)).map_err(xml_err)?;

    // Three forms, most specific first. Clients match channels on display-name
    // and disagree about which form they expect: some want "1 Name", some the
    // bare number, some the bare name. Emitting all three lets each pick the one
    // it understands instead of failing to match at all. Ported from upstream
    // aef9f30.
    write_text_element(
        w,
        "display-name",
        &format!("{} {}", channel.number(), channel.name()),
    )?;
    write_text_element(w, "display-name", channel.number())?;
    write_text_element(w, "display-name", channel.name())?;

    if let Some(logo) = channel.logo() {
        let mut icon = BytesStart::new("icon");
        icon.push_attribute(("src", logo));
        w.write_event(Event::Empty(icon)).map_err(xml_err)?;
    }

    w.write_event(Event::End(BytesEnd::new("channel")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_programme<W: std::io::Write>(
    w: &mut Writer<W>,
    tvg_id: &str,
    item: &PlayoutItem,
) -> Result<(), LineupError> {
    let start = format_xmltv_datetime(item.start);
    let stop = format_xmltv_datetime(item.finish);

    let mut elem = BytesStart::new("programme");
    elem.push_attribute(("start", start.as_str()));
    elem.push_attribute(("stop", stop.as_str()));
    elem.push_attribute(("channel", tvg_id));
    w.write_event(Event::Start(elem)).map_err(xml_err)?;

    if let Some(meta) = &item.program {
        write_metadata(w, meta)?;
    }

    w.write_event(Event::End(BytesEnd::new("programme")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_metadata<W: std::io::Write>(
    w: &mut Writer<W>,
    meta: &ProgramMetadata,
) -> Result<(), LineupError> {
    if let Some(title) = &meta.title {
        write_text_element(w, "title", title)?;
    }
    if let Some(sub_title) = &meta.sub_title {
        write_text_element(w, "sub-title", sub_title)?;
    }
    if let Some(desc) = &meta.description {
        write_text_element(w, "desc", desc)?;
    }
    if let Some(credits) = &meta.credits {
        write_credits(w, credits)?;
    }
    if let Some(categories) = &meta.categories {
        for category in categories {
            write_text_element(w, "category", category)?;
        }
    }
    if let Some(year) = meta.year {
        write_text_element(w, "date", &year.to_string())?;
    }
    if let Some(url) = &meta.artwork_url {
        let mut icon = BytesStart::new("icon");
        icon.push_attribute(("src", url.as_str()));
        w.write_event(Event::Empty(icon)).map_err(xml_err)?;
    }
    if let Some(countries) = &meta.country {
        for country in countries {
            write_text_element(w, "country", country)?;
        }
    }
    if let (Some(season), Some(episode)) = (meta.season, meta.episode) {
        write_text_element(w, "episode-num", &format!("S{season:02}E{episode:02}"))?;
        let mut elem = BytesStart::new("episode-num");
        elem.push_attribute(("system", "xmltv_ns"));
        w.write_event(Event::Start(elem)).map_err(xml_err)?;
        let body = format!(
            "{}.{}.",
            season.saturating_sub(1),
            episode.saturating_sub(1)
        );
        w.write_event(Event::Text(BytesText::new(&body)))
            .map_err(xml_err)?;
        w.write_event(Event::End(BytesEnd::new("episode-num")))
            .map_err(xml_err)?;
    }
    if let Some(rating) = &meta.content_rating {
        let elem = BytesStart::new("rating");
        w.write_event(Event::Start(elem)).map_err(xml_err)?;
        write_text_element(w, "value", rating)?;
        w.write_event(Event::End(BytesEnd::new("rating")))
            .map_err(xml_err)?;
    }
    if let Some(star_rating) = &meta.star_rating {
        let elem = BytesStart::new("star-rating");
        w.write_event(Event::Start(elem)).map_err(xml_err)?;
        write_text_element(w, "value", star_rating)?;
        w.write_event(Event::End(BytesEnd::new("star-rating")))
            .map_err(xml_err)?;
    }
    Ok(())
}

/// Writes `<credits>` with children in XMLTV's required order — director,
/// actor, writer — regardless of the order the caller populated `Credits`
/// in. Omits the whole element (not just an empty one) when every role list
/// is empty, matching how the rest of `ProgramMetadata` treats "nothing to
/// say" as "say nothing" rather than an empty tag.
fn write_credits<W: std::io::Write>(
    w: &mut Writer<W>,
    credits: &Credits,
) -> Result<(), LineupError> {
    if credits.director.is_empty() && credits.actor.is_empty() && credits.writer.is_empty() {
        return Ok(());
    }

    w.write_event(Event::Start(BytesStart::new("credits")))
        .map_err(xml_err)?;
    for director in &credits.director {
        write_text_element(w, "director", director)?;
    }
    for actor in &credits.actor {
        let mut elem = BytesStart::new("actor");
        if let Some(role) = &actor.role {
            elem.push_attribute(("role", role.as_str()));
        }
        w.write_event(Event::Start(elem)).map_err(xml_err)?;
        w.write_event(Event::Text(BytesText::new(&actor.name)))
            .map_err(xml_err)?;
        w.write_event(Event::End(BytesEnd::new("actor")))
            .map_err(xml_err)?;
    }
    for writer in &credits.writer {
        write_text_element(w, "writer", writer)?;
    }
    w.write_event(Event::End(BytesEnd::new("credits")))
        .map_err(xml_err)?;
    Ok(())
}

fn write_text_element<W: std::io::Write>(
    w: &mut Writer<W>,
    tag: &'static str,
    text: &str,
) -> Result<(), LineupError> {
    w.write_event(Event::Start(BytesStart::new(tag)))
        .map_err(xml_err)?;
    w.write_event(Event::Text(BytesText::new(text)))
        .map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new(tag)))
        .map_err(xml_err)?;
    Ok(())
}

fn xml_err(err: std::io::Error) -> LineupError {
    LineupError::XmltvFailure(err.to_string())
}

/// Walk the channel's output folder for any `{start}_{finish}.json` playout files,
/// load each, and return every airing, deduped by airing. The principle is "the
/// JSON on disk is the truth" — we don't apply a time window cap.
///
/// The key is the pair of id and start, not the id alone. An item straddling a
/// chunk boundary is deliberately written into both neighbouring files so either
/// side can play it, so the same airing really is read twice and does need
/// collapsing — but `id` names the film, not the showing of it. Keyed on `id`
/// alone, a channel that plays anything twice loses every repeat: Lord of the
/// Rings has six films, so its guide held six programmes out of twenty-two
/// airings and then simply stopped, and a 190-airing comedy channel showed 96
/// programmes with 94 holes where the repeats had been. The two showings differ
/// in `start`, so both survive; the same showing read from two files does not.
async fn collect_items(channel: &ChannelModel) -> Vec<PlayoutItem> {
    let folder = channel.playout_folder();
    let paths = playout_file_paths(folder).await;

    let mut seen: HashSet<(String, OffsetDateTime)> = HashSet::new();
    let mut items: Vec<PlayoutItem> = Vec::new();

    for path in paths {
        match ersatztv_playout::playout::from_file(&path).await {
            Ok(loaded) => {
                for item in loaded.playout.items {
                    if seen.insert((item.id.clone(), item.start)) {
                        items.push(item);
                    }
                }
            }
            Err(err) => {
                log::warn!(
                    "skipping playout file {} for channel {}: {}",
                    path,
                    channel.number(),
                    err
                );
            }
        }
    }

    items.sort_by_key(|i| i.start);
    items
}

async fn playout_file_paths(folder: &Path) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();

    let mut entries = match tokio::fs::read_dir(folder).await {
        Ok(entries) => entries,
        Err(_) => return paths,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let path_str = match path.clone().into_os_string().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !path_str.ends_with(".json") {
            continue;
        }

        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };

        let split: Vec<&str> = stem.split('_').collect();
        if split.len() != 2 {
            continue;
        }

        let start = OffsetDateTime::parse(split[0], &DATE_FORMAT)
            .ok()
            .or_else(|| parse_unix_timestamp(split[0]));
        let finish = OffsetDateTime::parse(split[1], &DATE_FORMAT)
            .ok()
            .or_else(|| parse_unix_timestamp(split[1]));

        if start.is_some() && finish.is_some() {
            paths.push(path_str);
        }
    }

    paths.sort();
    paths
}

fn parse_unix_timestamp(timestamp: &str) -> Option<OffsetDateTime> {
    let epoch = timestamp
        .parse::<i64>()
        .map(|i| if timestamp.len() > 10 { i / 1000 } else { i })
        .ok()?;
    OffsetDateTime::from_unix_timestamp(epoch).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header::CONTENT_TYPE};
    use axum::routing::get;
    use ersatztv_playout::playout::{Actor, Credits, Playout, PlayoutItem, ProgramMetadata};
    use time::macros::datetime;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn datetime_format_matches_xmltv_spec() {
        let dt = datetime!(2026-04-30 12:00:00 +00:00:00);
        assert_eq!(format_xmltv_datetime(dt), "20260430120000 +0000");

        let dt = datetime!(2026-04-30 12:00:00 -05:00:00);
        assert_eq!(format_xmltv_datetime(dt), "20260430120000 -0500");
    }

    fn fully_populated_item() -> PlayoutItem {
        PlayoutItem {
            id: "a".into(),
            start: datetime!(2026-04-30 12:00:00 +00:00:00),
            finish: datetime!(2026-04-30 12:30:00 +00:00:00),
            source: None,
            tracks: None,
            watermark: None,
            program: Some(ProgramMetadata {
                title: Some("The Title".into()),
                sub_title: Some("The Episode".into()),
                description: Some("A & B < C".into()),
                season: Some(2),
                episode: Some(5),
                categories: Some(vec!["Drama".into(), "Sci-Fi".into()]),
                content_rating: Some("TV-14".into()),
                artwork_url: Some("https://example.test/poster.jpg".into()),
                year: Some(2026),
                credits: Some(Box::new(Credits {
                    director: vec!["Ridley Scott".into()],
                    actor: vec![
                        Actor {
                            name: "Sigourney Weaver".into(),
                            role: Some("Ripley".into()),
                        },
                        Actor {
                            name: "Tom Skerritt".into(),
                            role: None,
                        },
                    ],
                    writer: vec!["Ronald Shusett".into()],
                })),
                country: Some(vec!["United States".into(), "United Kingdom".into()]),
                star_rating: Some("4 / 5".into()),
            }),
            overlay: None,
            // Present on purpose: nothing in this repo reads `metadata`, so if it
            // ever leaked into the guide these assertions would say so.
            metadata: Some(serde_json::json!({ "picked_because": "oscar" })),
        }
    }

    fn bare_item() -> PlayoutItem {
        PlayoutItem {
            id: "b".into(),
            start: datetime!(2026-04-30 12:30:00 +00:00:00),
            finish: datetime!(2026-04-30 13:00:00 +00:00:00),
            source: None,
            tracks: None,
            watermark: None,
            program: None,
            overlay: None,
            metadata: None,
        }
    }

    // Registers `/xmltv.xml` the same way `run()` does in main.rs and drives it
    // with a real request, so a route-registration or Content-Type regression
    // fails here — the other tests only call `build_xmltv()` directly.
    #[tokio::test]
    async fn xmltv_epg_route_serves_the_guide() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("playout")).unwrap();
        let cfg_path = dir.path().join("channel.json");
        std::fs::write(&cfg_path, r#"{"playout":{"folder":"playout"}}"#).unwrap();
        let cfg = ersatztv::config::ChannelConfig {
            number: "1".into(),
            name: "ETV 1".into(),
            config: cfg_path.to_string_lossy().into_owned(),
            overlays: Vec::new(),
            tvg_id: None,
            logo: None,
            group: None,
        };
        let channel = ChannelModel::new(&cfg_path, dir.path(), cfg).unwrap();

        let playout = Playout::new(vec![bare_item()]);
        std::fs::write(
            dir.path()
                .join("playout")
                .join("20260430T123000.000000000+0000_20260430T130000.000000000+0000.json"),
            serde_json::to_string(&playout).unwrap(),
        )
        .unwrap();

        let state = Arc::new(LineupState {
            channels: vec![channel],
            active: Arc::new(Mutex::new(HashMap::new())),
            health: Arc::new(Mutex::new(crate::channel_health::HealthMap::default())),
            device_id: "test-device".into(),
        });

        let app = Router::new()
            .route("/xmltv.xml", get(xmltv_epg))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/xmltv.xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/xml; charset=utf-8"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert!(
            body.contains(r#"<tv generator-info-name="ersatztv-next">"#),
            "{body}"
        );
        assert!(body.contains(r#"<channel id="ersatztv.1">"#), "{body}");
        assert!(
            body.contains(
                r#"<programme start="20260430123000 +0000" stop="20260430130000 +0000" channel="ersatztv.1">"#
            ),
            "{body}"
        );
    }

    #[tokio::test]
    async fn playout_file_paths_only_matches_well_named_files() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path();

        let p1 = Playout::new(vec![fully_populated_item()]);
        let p2 = Playout::new(vec![fully_populated_item(), bare_item()]);

        tokio::fs::write(
            folder.join("20260430T120000.000000000+0000_20260430T123000.000000000+0000.json"),
            serde_json::to_string(&p1).unwrap(),
        )
        .await
        .unwrap();
        tokio::fs::write(
            folder.join("20260430T123000.000000000+0000_20260430T130000.000000000+0000.json"),
            serde_json::to_string(&p2).unwrap(),
        )
        .await
        .unwrap();
        tokio::fs::write(folder.join("notes.json"), "{}")
            .await
            .unwrap();

        let paths = playout_file_paths(folder).await;
        assert_eq!(paths.len(), 2, "only the two well-named files match");
    }

    #[test]
    fn build_xmltv_emits_expected_structure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("playout")).unwrap();
        let cfg_path = dir.path().join("channel.json");
        std::fs::write(&cfg_path, r#"{"playout":{"folder":"playout"}}"#).unwrap();
        let cfg = ersatztv::config::ChannelConfig {
            number: "1".into(),
            name: "ETV 1".into(),
            config: cfg_path.to_string_lossy().into_owned(),
            overlays: Vec::new(),
            tvg_id: None,
            logo: Some("https://example.test/logo.png".into()),
            group: Some("Movies".into()),
        };
        let channel = ChannelModel::new(&cfg_path, dir.path(), cfg).unwrap();

        let sections = vec![(
            channel.tvg_id().to_owned(),
            vec![fully_populated_item(), bare_item()],
        )];

        let xml = build_xmltv(std::slice::from_ref(&channel), &sections).unwrap();

        assert!(
            xml.contains(r#"<tv generator-info-name="ersatztv-next">"#),
            "{xml}"
        );
        assert!(xml.contains(r#"<channel id="ersatztv.1">"#), "{xml}");
        assert!(
            xml.contains("<display-name>1 ETV 1</display-name>"),
            "{xml}"
        );
        assert!(xml.contains("<display-name>1</display-name>"), "{xml}");
        assert!(xml.contains("<display-name>ETV 1</display-name>"), "{xml}");
        assert!(
            xml.contains(r#"<icon src="https://example.test/logo.png"/>"#),
            "{xml}"
        );

        assert!(
            xml.contains(
                r#"<programme start="20260430120000 +0000" stop="20260430123000 +0000" channel="ersatztv.1">"#
            ),
            "{xml}"
        );
        assert!(xml.contains("<title>The Title</title>"), "{xml}");
        assert!(xml.contains("<sub-title>The Episode</sub-title>"), "{xml}");
        assert!(xml.contains("<desc>A &amp; B &lt; C</desc>"), "{xml}");
        assert!(xml.contains("<category>Drama</category>"), "{xml}");
        assert!(xml.contains("<category>Sci-Fi</category>"), "{xml}");
        assert!(xml.contains("<episode-num>S02E05</episode-num>"), "{xml}");
        assert!(
            xml.contains(r#"<episode-num system="xmltv_ns">1.4.</episode-num>"#),
            "{xml}"
        );
        assert!(xml.contains("<rating>"), "{xml}");
        assert!(xml.contains("<value>TV-14</value>"), "{xml}");
        assert!(xml.contains("<director>Ridley Scott</director>"), "{xml}");
        assert!(
            xml.contains(r#"<actor role="Ripley">Sigourney Weaver</actor>"#),
            "{xml}"
        );
        assert!(xml.contains("<actor>Tom Skerritt</actor>"), "{xml}");
        assert!(xml.contains("<writer>Ronald Shusett</writer>"), "{xml}");
        assert!(xml.contains("<country>United States</country>"), "{xml}");
        assert!(xml.contains("<country>United Kingdom</country>"), "{xml}");
        assert!(xml.contains("<star-rating>"), "{xml}");
        assert!(xml.contains("<value>4 / 5</value>"), "{xml}");

        // XMLTV is order-sensitive: verify both the top-level placement of
        // the new elements against the DTD (`credits?` before `category*`;
        // `country*` after `icon*`, before `episode-num*`; `star-rating*`
        // after `rating*`) and the required child order inside `<credits>`
        // (director, actor, writer).
        let full_open = r#"<programme start="20260430120000 +0000" stop="20260430123000 +0000" channel="ersatztv.1">"#;
        let full_idx = xml.find(full_open).expect("populated programme present");
        let full_close_idx = xml[full_idx..].find("</programme>").unwrap() + full_idx;
        let full_body = &xml[full_idx..full_close_idx];

        let desc_pos = full_body.find("<desc>").unwrap();
        let credits_pos = full_body.find("<credits>").unwrap();
        let category_pos = full_body.find("<category>").unwrap();
        let icon_pos = full_body.find("<icon ").unwrap();
        let country_pos = full_body.find("<country>").unwrap();
        let episode_pos = full_body.find("<episode-num>").unwrap();
        let rating_pos = full_body.find("<rating>").unwrap();
        let star_rating_pos = full_body.find("<star-rating>").unwrap();
        assert!(desc_pos < credits_pos, "credits should follow desc");
        assert!(
            credits_pos < category_pos,
            "credits should precede category per the XMLTV DTD"
        );
        assert!(icon_pos < country_pos, "country should follow icon");
        assert!(
            country_pos < episode_pos,
            "country should precede episode-num per the XMLTV DTD"
        );
        assert!(
            rating_pos < star_rating_pos,
            "star-rating should follow rating"
        );

        let director_pos = full_body.find("<director>").unwrap();
        let actor_pos = full_body.find("<actor").unwrap();
        let writer_pos = full_body.find("<writer>").unwrap();
        assert!(
            director_pos < actor_pos && actor_pos < writer_pos,
            "credits children must appear director, actor, writer per the XMLTV DTD"
        );

        let bare_open = r#"<programme start="20260430123000 +0000" stop="20260430130000 +0000" channel="ersatztv.1">"#;
        let bare_idx = xml.find(bare_open).expect("bare programme present");
        let bare_close_idx = xml[bare_idx..].find("</programme>").unwrap() + bare_idx;
        let bare_body = &xml[bare_idx + bare_open.len()..bare_close_idx];
        assert!(
            bare_body.trim().is_empty(),
            "bare programme should have no children, got: {bare_body:?}"
        );
    }

    // `Some(Credits::default())` — every role list present but empty — is
    // reachable from a producer that always sets the field (e.g. serializing
    // a struct that starts empty rather than omitting it). It must still
    // read as "nothing to say" and emit no `<credits>` tag at all, not an
    // empty one; a bare `<credits></credits>` is invalid in some XMLTV
    // readers even though the DTD's `*` cardinalities technically allow it.
    #[test]
    fn empty_credits_emits_no_element() {
        let mut buf = Cursor::new(Vec::<u8>::new());
        {
            let mut w = Writer::new(&mut buf);
            write_credits(&mut w, &Credits::default()).unwrap();
        }
        assert_eq!(buf.into_inner(), Vec::<u8>::new());
    }

    #[tokio::test]
    async fn collect_items_reads_from_playout_folder_not_hls_output() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        let playout_folder = root.join("station-playout");
        std::fs::create_dir(&playout_folder).unwrap();

        let hls_output_folder = root.join("hls");
        std::fs::create_dir(&hls_output_folder).unwrap();
        let hls_channel_folder = hls_output_folder.join("1");
        std::fs::create_dir(&hls_channel_folder).unwrap();

        let cfg_path = root.join("channel.json");
        std::fs::write(&cfg_path, r#"{"playout":{"folder":"station-playout"}}"#).unwrap();

        let real = Playout::new(vec![fully_populated_item()]);
        tokio::fs::write(
            playout_folder
                .join("20260430T120000.000000000+0000_20260430T123000.000000000+0000.json"),
            serde_json::to_string(&real).unwrap(),
        )
        .await
        .unwrap();

        let decoy = Playout::new(vec![bare_item()]);
        tokio::fs::write(
            hls_channel_folder
                .join("20260430T123000.000000000+0000_20260430T130000.000000000+0000.json"),
            serde_json::to_string(&decoy).unwrap(),
        )
        .await
        .unwrap();

        let cfg = ersatztv::config::ChannelConfig {
            number: "1".into(),
            name: "ETV 1".into(),
            config: cfg_path.to_string_lossy().into_owned(),
            overlays: Vec::new(),
            tvg_id: None,
            logo: None,
            group: None,
        };
        let channel = ChannelModel::new(&cfg_path, &hls_output_folder, cfg).unwrap();

        let items = collect_items(&channel).await;
        assert_eq!(items.len(), 1, "should read only from playout folder");
        assert_eq!(
            items[0].id, "a",
            "got the populated item from playout, not the decoy from HLS"
        );
    }

    // A themed channel plays the same films again over the days — that is the
    // point of one. Keyed on `id` alone, every showing after the first was
    // dropped as a duplicate: a six-film channel published six programmes and
    // then ended, and a big-library channel published one hole per repeat.
    #[tokio::test]
    async fn a_film_shown_twice_appears_twice_in_the_guide() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let playout_folder = root.join("playout");
        std::fs::create_dir(&playout_folder).unwrap();
        let cfg_path = root.join("channel.json");
        std::fs::write(&cfg_path, r#"{"playout":{"folder":"playout"}}"#).unwrap();

        // The same film shown on Monday and again on Tuesday. `PlayoutItem` is
        // not `Clone`, so each airing is built fresh.
        let monday_start = datetime!(2026-04-30 12:00:00 +00:00:00);
        let tuesday_start = datetime!(2026-05-01 12:00:00 +00:00:00);
        let tuesday_airing = || {
            let mut item = fully_populated_item();
            item.start = tuesday_start;
            item.finish = datetime!(2026-05-01 12:30:00 +00:00:00);
            item
        };

        // The Monday airing also straddles a chunk boundary, so it is written
        // into both neighbouring files — the duplication the dedup exists for.
        for (name, items) in [
            (
                "20260430T120000.000000000+0000_20260430T180000.000000000+0000.json",
                vec![fully_populated_item()],
            ),
            (
                "20260430T180000.000000000+0000_20260501T000000.000000000+0000.json",
                vec![fully_populated_item()],
            ),
            (
                "20260501T000000.000000000+0000_20260501T180000.000000000+0000.json",
                vec![tuesday_airing()],
            ),
        ] {
            tokio::fs::write(
                playout_folder.join(name),
                serde_json::to_string(&Playout::new(items)).unwrap(),
            )
            .await
            .unwrap();
        }

        let cfg = ersatztv::config::ChannelConfig {
            number: "1".into(),
            name: "ETV 1".into(),
            config: cfg_path.to_string_lossy().into_owned(),
            overlays: Vec::new(),
            tvg_id: None,
            logo: None,
            group: None,
        };
        let channel = ChannelModel::new(&cfg_path, root, cfg).unwrap();

        let items = collect_items(&channel).await;

        assert_eq!(
            items.len(),
            2,
            "both showings survive; only the straddle duplicate collapses",
        );
        assert_eq!(items[0].start, monday_start);
        assert_eq!(items[1].start, tuesday_start);
        assert_eq!(items[0].id, items[1].id, "same film, two airings");
    }
}
