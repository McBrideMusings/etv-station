//! End-to-end acceptance for #123: `item.fs_dir` names the folder a file is in
//! **now**, not every folder it has ever been in.
//!
//! The unit tests around `ingest_files` and `translate` run against an
//! in-memory catalog. This drives the whole path a running station uses: a real
//! media tree on disk, a real `catalog.db` opened for writing and migrated, a
//! real directory walk, and then the **read-only** per-channel handle that
//! `daemon` hands each channel. That last hop is the one worth spending a test
//! on — `open_readonly` registers its own SQL functions, and a query that works
//! through the writable handle would still fail at air time if it didn't.

use std::fs;
use std::path::Path;

use etv_station::catalog::ingest::fs::ingest_roots;
use etv_station::catalog::{Catalog, Entry, EntrySource, Source};

fn touch(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"not really a video").unwrap();
}

/// Drag a bumper from `bumpers/` into `commercials/`, re-scan, and ask both
/// questions.
///
/// The folder used to be stored as a tag, and `add_tag` only ever inserted, so
/// any entry that kept its id across a move kept the old folder as well as the
/// new one — a bumpers-only channel went on airing a file that had been
/// reclassified as an ad. Reading the folder off the provenance row removes the
/// second copy that could disagree, so there is no id for which it can go stale.
#[tokio::test]
async fn a_moved_file_stops_answering_to_the_folder_it_left() {
    let tmp = tempfile::tempdir().unwrap();
    let media = tmp.path().join("media");
    let db = tmp.path().join("catalog.db");
    let roots = vec![media.clone()];
    let identity_roots = vec![media.to_string_lossy().into_owned()];

    touch(&media.join("bumpers/station-bumper-01.mp4"));

    let cat = Catalog::open(&db).unwrap();
    ingest_roots(&cat, &roots, &identity_roots).await.unwrap();
    assert_eq!(
        cat.resolve_query(r#"item.fs_dir == "bumpers""#)
            .unwrap()
            .len(),
        1,
        "the file starts life in bumpers/"
    );

    // The move, exactly as a user would do it in Finder.
    fs::create_dir_all(media.join("commercials")).unwrap();
    fs::rename(
        media.join("bumpers/station-bumper-01.mp4"),
        media.join("commercials/station-bumper-01.mp4"),
    )
    .unwrap();
    ingest_roots(&cat, &roots, &identity_roots).await.unwrap();
    drop(cat);

    // Read it back the way a channel does: a separate, read-only handle.
    let reader = Catalog::open_readonly(&db).unwrap();
    assert_eq!(
        reader
            .resolve_query(r#"item.fs_dir == "commercials""#)
            .unwrap()
            .len(),
        1,
        "the folder it moved into must match"
    );
    assert!(
        reader
            .resolve_query(r#"item.fs_dir == "bumpers""#)
            .unwrap()
            .is_empty(),
        "the folder it left must stop matching"
    );
}

/// Two folders can both be true. A movie Plex holds as two files under one IMDb
/// id — a 4K copy and a 1080p copy, in different folders — is a single catalog
/// entry with two provenance rows, and a channel asking for either folder must
/// find it. This is why #123 could not be fixed the way #101 fixed genres:
/// clearing the entry's folder tags and rewriting them would have left whichever
/// file was written last and thrown the other away.
#[test]
fn an_entry_with_files_in_two_folders_answers_to_both() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog.db");

    let cat = Catalog::open(&db).unwrap();
    cat.upsert_entry(&Entry::new(
        "imdb:tt0095016",
        "movie",
        "Die Hard",
        Source::Plex,
    ))
    .unwrap();
    for (key, folder) in [("plex-4k", "movies-4k"), ("plex-hd", "movies-1080")] {
        cat.add_source(&EntrySource {
            source: Source::Plex,
            source_id: key.to_string(),
            entry_id: "imdb:tt0095016".to_string(),
            playback_path: format!("/data/media/{folder}/die-hard.mkv"),
            last_seen: None,
            missing_since: None,
        })
        .unwrap();
    }
    drop(cat);

    let reader = Catalog::open_readonly(&db).unwrap();
    for folder in ["movies-4k", "movies-1080"] {
        assert_eq!(
            reader
                .resolve_query(&format!(r#"item.fs_dir == "{folder}""#))
                .unwrap(),
            vec!["imdb:tt0095016".to_string()],
            "expected the entry to answer to {folder}"
        );
    }
}
