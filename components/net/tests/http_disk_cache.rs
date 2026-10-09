/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fs;
use std::thread::sleep;
use std::time::Duration;

use net::http_disk_cache::DiskCache;

fn file_names(directory: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn test_store_and_read_back() {
    let directory = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(directory.path().to_owned(), 1000).unwrap();
    let name = DiskCache::name_for("https://servo.org/");
    let id = cache.new_body_id();
    cache
        .store(&name, b"metadata", &[(id, b"body".to_vec())], &[])
        .unwrap();

    assert!(cache.contains(&name));
    assert_eq!(cache.size(), 12);
    assert_eq!(cache.read_metadata(&name).unwrap(), b"metadata");
    assert_eq!(cache.read_body(&name, id, 4).unwrap(), b"body");
    assert!(cache.read_body(&name, id, 5).is_none());

    // What a restart finds.
    let cache = DiskCache::open(directory.path().to_owned(), 1000).unwrap();
    assert!(cache.contains(&name));
    assert_eq!(cache.size(), 12);
    assert_eq!(cache.read_body(&name, id, 4).unwrap(), b"body");
    assert!(cache.new_body_id() > id);
}

#[test]
fn test_rewriting_metadata_keeps_or_drops_bodies() {
    let directory = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(directory.path().to_owned(), 1000).unwrap();
    let name = DiskCache::name_for("https://servo.org/");
    let first = cache.new_body_id();
    cache
        .store(&name, b"one", &[(first, b"first".to_vec())], &[])
        .unwrap();

    cache.store(&name, b"two", &[], &[first]).unwrap();
    assert_eq!(cache.read_metadata(&name).unwrap(), b"two");
    assert_eq!(cache.read_body(&name, first, 5).unwrap(), b"first");
    assert_eq!(cache.size(), 8);

    let second = cache.new_body_id();
    cache
        .store(&name, b"three", &[(second, b"second".to_vec())], &[])
        .unwrap();
    assert!(cache.read_body(&name, first, 5).is_none());
    assert_eq!(cache.read_body(&name, second, 6).unwrap(), b"second");
    assert_eq!(cache.size(), 11);
    assert_eq!(file_names(directory.path()).len(), 2);
}

#[test]
fn test_open_removes_what_an_interrupted_write_left() {
    let directory = tempfile::tempdir().unwrap();
    let name = DiskCache::name_for("https://servo.org/");
    let other = DiskCache::name_for("https://servo.org/other");
    {
        let cache = DiskCache::open(directory.path().to_owned(), 1000).unwrap();
        cache
            .store(&name, b"metadata", &[(1, b"body".to_vec())], &[])
            .unwrap();
    }
    // A write that stopped before the rename.
    fs::write(directory.path().join(format!("{name}.meta.tmp")), b"meta").unwrap();
    // A body whose metadata was never written.
    fs::write(directory.path().join(format!("{other}.7.body")), b"orphan").unwrap();
    // Not a file of the cache.
    fs::write(directory.path().join("README"), b"hello").unwrap();

    let cache = DiskCache::open(directory.path().to_owned(), 1000).unwrap();
    assert!(cache.contains(&name));
    assert!(!cache.contains(&other));
    assert_eq!(cache.size(), 12);
    assert_eq!(
        file_names(directory.path()),
        vec![
            "README".to_owned(),
            format!("{name}.1.body"),
            format!("{name}.meta")
        ]
    );
}

#[test]
fn test_entries_used_longest_ago_are_evicted() {
    let directory = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(directory.path().to_owned(), 1000).unwrap();
    let names: Vec<String> = (0..3)
        .map(|index| DiskCache::name_for(&format!("https://servo.org/{index}")))
        .collect();
    for name in &names[..2] {
        let id = cache.new_body_id();
        cache.store(name, b"m", &[(id, vec![0; 399])], &[]).unwrap();
        // The time of last use has the resolution of the clock.
        sleep(Duration::from_millis(20));
    }
    // The first entry is now the one used last.
    cache.mark_used(&names[0]);
    sleep(Duration::from_millis(20));

    let id = cache.new_body_id();
    cache
        .store(&names[2], b"m", &[(id, vec![0; 399])], &[])
        .unwrap();

    assert!(cache.contains(&names[0]));
    assert!(!cache.contains(&names[1]));
    assert!(cache.contains(&names[2]));
    assert_eq!(cache.size(), 800);
    assert_eq!(file_names(directory.path()).len(), 4);
}

#[test]
fn test_time_of_last_use_survives_a_restart() {
    let directory = tempfile::tempdir().unwrap();
    let names: Vec<String> = (0..2)
        .map(|index| DiskCache::name_for(&format!("https://servo.org/{index}")))
        .collect();
    {
        let cache = DiskCache::open(directory.path().to_owned(), 1000).unwrap();
        for name in &names {
            cache.store(name, b"m", &[(1, vec![0; 399])], &[]).unwrap();
            sleep(Duration::from_millis(20));
        }
        cache.mark_used(&names[0]);
    }

    // Too small for both, so opening evicts the one used longest ago.
    let cache = DiskCache::open(directory.path().to_owned(), 500).unwrap();
    assert!(cache.contains(&names[0]));
    assert!(!cache.contains(&names[1]));
}

#[test]
fn test_remove_and_clear() {
    let directory = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(directory.path().to_owned(), 1000).unwrap();
    let names: Vec<String> = (0..3)
        .map(|index| DiskCache::name_for(&format!("https://servo.org/{index}")))
        .collect();
    for name in &names {
        let id = cache.new_body_id();
        cache.store(name, b"m", &[(id, vec![0; 9])], &[]).unwrap();
    }

    cache.remove(&names[0]);
    assert!(!cache.contains(&names[0]));
    assert_eq!(cache.size(), 20);
    assert_eq!(file_names(directory.path()).len(), 4);

    cache.clear();
    assert_eq!(cache.size(), 0);
    assert!(cache.names().is_empty());
    assert!(file_names(directory.path()).is_empty());
}
