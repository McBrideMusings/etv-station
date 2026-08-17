//! The `guide:` config block (#158): what reaches `ResolvedItem` with zero
//! configuration (the series-title convention, genre-tag categories), and
//! that a `guide:` block cascades channel → block → item with the most
//! specific level winning per field. Template *rendering* — which needs the
//! schedule and the watch history, neither of which exists at resolve time —
//! is covered separately in `etv-station`'s own `daemon` test module.

use std::path::Path;
use std::time::Duration;

use etv_station::catalog::{Catalog, Entry, EntrySource, Source, TagNs};
use etv_station::config::{
    BlockInclude, ChannelConfig, Entry as ConfigEntry, GuideConfig, ItemEntry, Mode, Order,
    PatternStep, Pool, QueryEntry, RuleConfig, SourceConfig, Take, TakeFrom,
};
use etv_station::resolve::resolve_channel;

fn path() -> &'static Path {
    Path::new("/tmp/guide-config-test.toml")
}

fn base_channel(blocks: Vec<BlockInclude>, guide: Option<GuideConfig>) -> ChannelConfig {
    ChannelConfig {
        name: None,
        display_name: None,
        guide,
        scoring: None,
        window_days: 1,
        chunk_hours: 24,
        roll_interval: Duration::from_secs(3600),
        retention_days: 1,
        seed: None,
        anchor: None,
        rule: RuleConfig { blocks },
        groups: Vec::new(),
        overlay: None,
    }
}

fn inline_block(entries: Vec<ConfigEntry>, guide: Option<GuideConfig>) -> BlockInclude {
    BlockInclude {
        block: None,
        program: None,
        guide,
        duplicates: None,
        constraints: None,
        entries,
        fallback: None,
        pools: Vec::new(),
        pattern: Vec::new(),
        cycles: None,
        sequencer: None,
        mode: Mode::All,
        order: Some(Order::Manual),
        filter: None,
    }
}

/// An episode entry ("Ozymandias", show "Breaking Bad") plus two genre tags,
/// resolvable by a `query` entry matching everything.
fn episode_catalog() -> Catalog {
    let cat = Catalog::open_in_memory().unwrap();
    let mut e = Entry::new("ep1", "episode", "Ozymandias", Source::Plex);
    e.show = Some("Breaking Bad".into());
    e.show_id = Some("show:breaking-bad".into());
    e.season = Some(5);
    e.episode = Some(14);
    cat.upsert_entry(&e).unwrap();
    cat.add_source(&EntrySource {
        source: Source::LocalFs,
        source_id: "fs-ep1".into(),
        entry_id: "ep1".into(),
        playback_path: "/media/ep1.mkv".into(),
        last_seen: None,
        missing_since: None,
    })
    .unwrap();
    cat.add_tag("ep1", TagNs::Genre, "Drama").unwrap();
    cat.add_tag("ep1", TagNs::Genre, "Crime").unwrap();
    cat.add_tag("ep1", TagNs::Cast, "Bryan Cranston").unwrap();
    cat
}

fn query_all() -> ConfigEntry {
    ConfigEntry::Query(QueryEntry {
        query: "item.title != ''".into(),
        order: None,
    })
}

/// A pattern block naming `pools`, walked once by `pattern` (#289's own
/// fixtures need no pattern beyond drawing every declared pool once).
fn pattern_block(
    pools: Vec<Pool>,
    pattern: Vec<PatternStep>,
    guide: Option<GuideConfig>,
) -> BlockInclude {
    let mut block = inline_block(vec![], guide);
    block.pools = pools;
    block.pattern = pattern;
    block
}

/// A minimal `expr`-sourced pool, with an optional pool-level `guide:`
/// (#289) — the rung between the block and the item.
fn pool(name: &str, expr: &str, guide: Option<GuideConfig>) -> Pool {
    Pool {
        name: name.into(),
        expr: Some(expr.into()),
        plugin: None,
        sources: None,
        groups: Vec::new(),
        order: Some(Order::parse("title:asc").unwrap()),
        bucket_order: None,
        group_by: Default::default(),
        select: Default::default(),
        rotate: Default::default(),
        advance: Default::default(),
        on_short: Default::default(),
        constraints: None,
        config: None,
        capabilities: Vec::new(),
        datastores: Vec::new(),
        guide,
    }
}

fn step(pool: &str, take: usize) -> PatternStep {
    PatternStep {
        pool: pool.into(),
        take: Take::Count(take),
        from: TakeFrom::Start,
        chance: 1.0,
    }
}

/// A films pool and a bumpers pool, catalog-resolved by `library`, exactly
/// the shape #289's issue was filed against — one movie, one station id.
fn films_and_bumpers_catalog() -> Catalog {
    fn add_movie(cat: &Catalog, id: &str, title: &str, library: &str) {
        let mut entry = Entry::new(id, "movie", title, Source::Plex);
        entry.library = Some(library.into());
        cat.upsert_entry(&entry).unwrap();
        cat.add_source(&EntrySource {
            source: Source::LocalFs,
            source_id: format!("fs-{id}"),
            entry_id: id.into(),
            playback_path: format!("/media/{id}.mkv"),
            last_seen: None,
            missing_since: None,
        })
        .unwrap();
    }

    let cat = Catalog::open_in_memory().unwrap();
    add_movie(&cat, "film-1", "Die Hard", "Films");
    add_movie(&cat, "bumper-1", "ID Card", "Station IDs");
    cat
}

/// #158 decision #1 + decision #3: with no `guide:` block anywhere, an
/// episode's `<title>` is still the series name, `<sub-title>` its own name,
/// and `<category>` still carries every genre tag — all with zero config.
#[test]
fn series_title_convention_and_auto_categories_need_no_config() {
    let cat = episode_catalog();
    let block = inline_block(vec![query_all()], None);
    let channel = base_channel(vec![block], None);

    let items = resolve_channel(&channel, path(), &[], None, Some(&cat)).unwrap();
    let item = items.iter().find(|i| i.id == "ep1").expect("ep1 resolves");
    let program = item.program.as_ref().expect("carries metadata");

    assert_eq!(program.title.as_deref(), Some("Breaking Bad"));
    assert_eq!(program.sub_title.as_deref(), Some("Ozymandias"));
    assert_eq!(
        program.categories.as_deref(),
        Some(&["Crime".to_string(), "Drama".to_string()][..])
    );
    assert!(
        item.guide.is_none(),
        "no guide: block anywhere resolves to no cascade"
    );

    // Raw catalog attributes the `guide:` template surface can address, also
    // populated with no config.
    assert_eq!(item.guide_fields.genres, vec!["Crime", "Drama"]);
    assert_eq!(item.guide_fields.cast, vec!["Bryan Cranston"]);
    assert_eq!(item.guide_fields.show.as_deref(), Some("Breaking Bad"));
}

/// #186's catalog `summary` column still auto-populates `description` with
/// no `guide:` config, exactly as before #158, AND reaches the `guide:`
/// template surface as `{summary}` — a `guide:` block does not have to
/// reference `summary` for it to already be the default description.
#[test]
fn the_catalog_summary_column_still_auto_populates_description() {
    let cat = Catalog::open_in_memory().unwrap();
    let mut e = Entry::new("m1", "movie", "Die Hard", Source::Plex);
    e.summary = Some("A cop fights terrorists in a tower.".into());
    cat.upsert_entry(&e).unwrap();
    cat.add_source(&EntrySource {
        source: Source::LocalFs,
        source_id: "fs-m1".into(),
        entry_id: "m1".into(),
        playback_path: "/media/m1.mkv".into(),
        last_seen: None,
        missing_since: None,
    })
    .unwrap();

    let block = inline_block(vec![query_all()], None);
    let channel = base_channel(vec![block], None);
    let items = resolve_channel(&channel, path(), &[], None, Some(&cat)).unwrap();
    let item = items.iter().find(|i| i.id == "m1").unwrap();

    assert_eq!(
        item.program.as_ref().unwrap().description.as_deref(),
        Some("A cop fights terrorists in a tower."),
        "the summary column is still the default description with no guide: block"
    );
    assert_eq!(
        item.guide_fields.summary.as_deref(),
        Some("A cop fights terrorists in a tower."),
        "and it is addressable as {{summary}} for a guide: template that wants it"
    );
}

/// A non-episode entry (no `show`) keeps its own title and gets no
/// `sub-title` — the convention only fires for the catalog's `"episode"`
/// kind.
#[test]
fn a_movie_keeps_its_own_title_and_no_sub_title() {
    let cat = Catalog::open_in_memory().unwrap();
    let e = Entry::new("m1", "movie", "Die Hard", Source::Plex);
    cat.upsert_entry(&e).unwrap();
    cat.add_source(&EntrySource {
        source: Source::LocalFs,
        source_id: "fs-m1".into(),
        entry_id: "m1".into(),
        playback_path: "/media/m1.mkv".into(),
        last_seen: None,
        missing_since: None,
    })
    .unwrap();

    let block = inline_block(vec![query_all()], None);
    let channel = base_channel(vec![block], None);
    let items = resolve_channel(&channel, path(), &[], None, Some(&cat)).unwrap();
    let item = items.iter().find(|i| i.id == "m1").unwrap();
    let program = item.program.as_ref().unwrap();

    assert_eq!(program.title.as_deref(), Some("Die Hard"));
    assert_eq!(program.sub_title, None);
    assert_eq!(program.categories, None, "no genre tags, no categories");
}

/// The `guide:` cascade (#158): item wins per field, then block, then
/// channel — never a whole-level override clobbering a field the more
/// specific level said nothing about.
#[test]
fn guide_cascades_channel_block_item_field_by_field() {
    let cat = episode_catalog();

    let channel_guide = GuideConfig {
        title: Some("{show} (channel)".into()),
        description: Some("channel description".into()),
        sub_title: None,
        categories: None,
    };
    let block_guide = GuideConfig {
        description: Some("block description".into()),
        categories: Some(vec!["Marathon".into()]),
        title: None,
        sub_title: None,
    };
    let item_guide = GuideConfig {
        sub_title: Some("item sub".into()),
        title: None,
        description: None,
        categories: None,
    };

    let mut entry = ItemEntry {
        source: SourceConfig::Local {
            path: "/media/inline.mkv".into(),
        },
        in_point: None,
        out_point: Some(Duration::from_secs(30)),
        program: None,
        guide: Some(item_guide),
    };
    // Force a stable id for the assertion below.
    entry.source = SourceConfig::Local {
        path: "/media/inline.mkv".into(),
    };

    let block = inline_block(vec![ConfigEntry::Item(Box::new(entry))], Some(block_guide));
    let channel = base_channel(vec![block], Some(channel_guide));

    let items = resolve_channel(&channel, path(), &[], None, Some(&cat)).unwrap();
    assert_eq!(items.len(), 1);
    let guide = items[0].guide.as_ref().expect("cascade is Some");

    // Item's own field wins.
    assert_eq!(guide.sub_title.as_deref(), Some("item sub"));
    // Block's field wins over the channel's, since the item said nothing.
    assert_eq!(guide.description.as_deref(), Some("block description"));
    assert_eq!(
        guide.categories.as_deref(),
        Some(&["Marathon".to_string()][..])
    );
    // Channel's field survives when neither item nor block said anything.
    assert_eq!(guide.title.as_deref(), Some("{show} (channel)"));
}

/// A catalog-resolved item (`query`/`collection`) has no item-level `guide:`
/// surface of its own — only the block ∘ channel cascade reaches it, exactly
/// like the existing `program:` defaults.
#[test]
fn a_catalog_resolved_item_only_sees_block_and_channel_guide() {
    let cat = episode_catalog();
    let block_guide = GuideConfig {
        description: Some("block description".into()),
        title: None,
        sub_title: None,
        categories: None,
    };
    let block = inline_block(vec![query_all()], Some(block_guide));
    let channel = base_channel(vec![block], None);

    let items = resolve_channel(&channel, path(), &[], None, Some(&cat)).unwrap();
    let item = items.iter().find(|i| i.id == "ep1").unwrap();
    let guide = item
        .guide
        .as_ref()
        .expect("block guide reaches a catalog item");
    assert_eq!(guide.description.as_deref(), Some("block description"));
}

/// #289: a pattern block interleaving a films pool and a bumpers pool — the
/// bumpers pool names its own `guide.title`, which reaches only the items it
/// draws. The films pool declares no `guide:` of its own, so its items fall
/// through to the block's, exactly as they would with no pool rung at all.
#[test]
fn a_pools_own_guide_title_reaches_only_that_pools_items() {
    let cat = films_and_bumpers_catalog();

    let bumper_guide = GuideConfig {
        title: Some("Station Break".into()),
        sub_title: None,
        description: None,
        categories: None,
    };
    let block_guide = GuideConfig {
        title: Some("{title} (block)".into()),
        sub_title: None,
        description: None,
        categories: None,
    };

    let films = pool("films", "item.library == 'Films'", None);
    let bumpers = pool(
        "bumpers",
        "item.library == 'Station IDs'",
        Some(bumper_guide),
    );
    let block = pattern_block(
        vec![films, bumpers],
        vec![step("films", 1), step("bumpers", 1)],
        Some(block_guide),
    );
    let channel = base_channel(vec![block], None);

    let items = resolve_channel(&channel, path(), &[], None, Some(&cat)).unwrap();

    let film_item = items.iter().find(|i| i.id == "film-1").unwrap();
    let bumper_item = items.iter().find(|i| i.id == "bumper-1").unwrap();

    assert_eq!(
        bumper_item.guide.as_ref().unwrap().title.as_deref(),
        Some("Station Break"),
        "the bumpers pool's own guide.title wins over the block's"
    );
    assert_eq!(
        film_item.guide.as_ref().unwrap().title.as_deref(),
        Some("{title} (block)"),
        "the films pool declares no guide of its own, so its items still see the block's"
    );
}

/// A pool's own `guide:` only overrides the fields it sets — a field it
/// says nothing about still falls through to the block, same as every other
/// rung in the cascade (#289 mirrors #158's field-by-field rule, not a
/// whole-level override).
#[test]
fn a_pools_own_guide_only_overrides_the_fields_it_sets() {
    let cat = films_and_bumpers_catalog();

    let bumper_guide = GuideConfig {
        title: Some("Station Break".into()),
        sub_title: None,
        description: None,
        categories: None,
    };
    let block_guide = GuideConfig {
        title: Some("block title".into()),
        description: Some("block description".into()),
        sub_title: None,
        categories: None,
    };

    let bumpers = pool(
        "bumpers",
        "item.library == 'Station IDs'",
        Some(bumper_guide),
    );
    let block = pattern_block(vec![bumpers], vec![step("bumpers", 1)], Some(block_guide));
    let channel = base_channel(vec![block], None);

    let items = resolve_channel(&channel, path(), &[], None, Some(&cat)).unwrap();
    let bumper_item = items.iter().find(|i| i.id == "bumper-1").unwrap();
    let guide = bumper_item.guide.as_ref().unwrap();

    assert_eq!(guide.title.as_deref(), Some("Station Break"));
    assert_eq!(
        guide.description.as_deref(),
        Some("block description"),
        "the pool said nothing about description, so the block's still reaches it"
    );
}
