//! The property the daemon leans on when it gives every channel its own catalog
//! handle: many read-only handles on one catalog file, all reading at once, none
//! waiting on any other.
//!
//! This is the shape of the failure it replaces. The station used to share one
//! `Catalog` behind a `Mutex`, so a channel whose scorer plugin spent minutes
//! ranking a library held that lock for every one of those minutes and every
//! other channel's resolve queued behind it — three channels dark for 6m34s.
//! Each test below fixes one of the assumptions that fix rests on: a reader can
//! be opened per channel, a busy reader does not stall another one, and a reader
//! cannot write (so "nothing writes the file after ingest" is enforced by SQLite
//! rather than remembered by whoever edits the daemon next).

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use etv_station::catalog::{Catalog, Entry, Source};

const MOVIES: &str = "item.year >= 1900";

/// Six movies in a real file on disk — not `open_in_memory`, which each handle
/// would get a private empty copy of.
fn catalog_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("catalog.db");
    let cat = Catalog::open(&path).unwrap();
    for (id, title, year) in [
        ("mov-a", "Arrival", 2016),
        ("mov-b", "Blade Runner", 1982),
        ("mov-c", "Contact", 1997),
        ("mov-d", "Dune", 2021),
        ("mov-e", "Enemy Mine", 1985),
        ("mov-f", "Fifth Element", 1997),
    ] {
        let mut e = Entry::new(id, "movie", title, Source::Plex);
        e.year = Some(year);
        cat.upsert_entry(&e).unwrap();
    }
    // The writable handle dies with ingest, exactly as it does in the daemon.
    drop(cat);
    path
}

/// Eight readers on one file, opened and queried from eight threads at once,
/// every one of them seeing the whole catalog. One handle per channel is the
/// daemon's model, and this is that model at a station's worth of channels.
#[test]
fn every_reader_opens_its_own_handle_and_sees_the_whole_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let path = catalog_file(&dir);

    let threads: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            thread::spawn(move || {
                let reader = Catalog::open_readonly(&path).unwrap();
                reader.resolve_query(MOVIES).unwrap()
            })
        })
        .collect();

    for t in threads {
        assert_eq!(t.join().unwrap().len(), 6);
    }
}

/// The acceptance criterion: one channel busy inside its own reader does not
/// hold up another channel's resolve.
///
/// The first thread opens a reader, reads through it, and then sits on it for a
/// second — a scorer plugin ranking a library, scaled down. While it is sitting
/// there, a second thread opens its own reader and resolves. That resolve has to
/// come back long before the first thread lets go; under the old shared lock it
/// could not have started at all.
///
/// `recv_timeout` rather than a wall-clock assertion, so a resolve that does
/// queue fails the test with a named expectation instead of hanging the suite.
#[test]
fn a_busy_reader_does_not_stall_another_channels_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let path = catalog_file(&dir);

    let (opened_tx, opened_rx) = mpsc::channel();
    let busy = {
        let path = path.clone();
        thread::spawn(move || {
            let reader = Catalog::open_readonly(&path).unwrap();
            let ids = reader.resolve_query(MOVIES).unwrap();
            opened_tx.send(()).unwrap();
            thread::sleep(Duration::from_secs(1));
            drop(reader);
            ids.len()
        })
    };
    opened_rx.recv().unwrap();

    let (done_tx, done_rx) = mpsc::channel();
    let other = thread::spawn(move || {
        let reader = Catalog::open_readonly(&path).unwrap();
        done_tx.send(reader.resolve_query(MOVIES).unwrap()).unwrap();
    });

    let ids = done_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("the second channel's resolve queued behind the busy reader");
    assert_eq!(ids.len(), 6);

    other.join().unwrap();
    assert_eq!(busy.join().unwrap(), 6);
}

/// A reader cannot write, and finds that out at the database rather than by
/// convention. Every write this process makes happens in
/// `open_and_ingest_catalog`; if a later edit ever calls a mutating method from
/// a channel, this is what turns that into an error instead of a silent race
/// against the other channels' handles.
#[test]
fn a_reader_is_refused_at_the_database_when_it_tries_to_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = catalog_file(&dir);

    let reader = Catalog::open_readonly(&path).unwrap();
    let mut e = Entry::new("mov-z", "movie", "Zardoz", Source::Plex);
    e.year = Some(1974);
    let err = reader
        .upsert_entry(&e)
        .expect_err("a read-only handle must not be able to insert an entry");
    assert!(
        err.to_string().contains("readonly"),
        "expected SQLite's readonly-database error, got: {err}"
    );

    // And the file is untouched: the six that were ingested are still all of it.
    assert_eq!(reader.resolve_query(MOVIES).unwrap().len(), 6);
}
