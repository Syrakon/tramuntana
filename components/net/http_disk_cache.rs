/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The files of the on-disk tier of the HTTP cache.
//!
//! An entry is everything the cache keeps for one cache key. It is a metadata
//! file, `<name>.meta`, and one file per stored response body,
//! `<name>.<id>.body`. This module does not look inside either: the metadata
//! is whatever [`crate::http_cache`] serialises, and it names the bodies it
//! uses by their id.
//!
//! Every file is written under a temporary name and renamed into place, and
//! the metadata file is written last, so it is the commit point of an entry. A
//! crash leaves either the previous entry or the new one, plus files that
//! nothing refers to. Opening the cache removes those, and the reader checks
//! the length of every body against the metadata, so there is no separate
//! index to fall out of step with the directory: the index is rebuilt from the
//! file names each time the cache is opened.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use log::warn;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

const METADATA_EXTENSION: &str = "meta";
const BODY_EXTENSION: &str = "body";
const TEMPORARY_EXTENSION: &str = "tmp";

/// Once the cache is over its capacity, entries are removed until it is back
/// under this share of it, so that not every later write has to evict again.
const EVICTION_TARGET_PERCENT: u64 = 90;

/// What the index knows about one entry.
struct IndexEntry {
    /// The size of the metadata file, or `None` while only bodies were seen.
    metadata_size: Option<u64>,
    /// The id and size of each body file.
    bodies: Vec<(u64, u64)>,
    last_used: SystemTime,
}

impl IndexEntry {
    fn new() -> Self {
        Self {
            metadata_size: None,
            bodies: Vec::new(),
            last_used: UNIX_EPOCH,
        }
    }

    fn size(&self) -> u64 {
        let bodies: u64 = self.bodies.iter().map(|(_id, size)| size).sum();
        self.metadata_size.unwrap_or(0) + bodies
    }
}

#[derive(Default)]
struct Index {
    entries: HashMap<String, IndexEntry>,
    /// The sum of the sizes of all entries.
    size: u64,
}

/// A directory of cache entries, bounded by the bytes they take.
pub struct DiskCache {
    directory: PathBuf,
    /// The most bytes the entries may take together.
    capacity: u64,
    index: Mutex<Index>,
    /// Held while an entry is written or removed, so that two writers never
    /// share a temporary file and no file is removed under a writer.
    write_lock: Mutex<()>,
    next_body_id: AtomicU64,
}

impl DiskCache {
    /// Open the cache in `directory`, creating it if needed, and remove what
    /// an interrupted write left there.
    pub fn open(directory: PathBuf, capacity: u64) -> io::Result<Self> {
        fs::create_dir_all(&directory)?;
        let mut entries: HashMap<String, IndexEntry> = HashMap::new();
        let mut highest_body_id = 0;
        for dir_entry in fs::read_dir(&directory)? {
            let dir_entry = dir_entry?;
            let file_name = dir_entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            let Some(parsed) = ParsedFileName::parse(file_name) else {
                // Not a file of this cache.
                continue;
            };
            let metadata = dir_entry.metadata()?;
            match parsed {
                ParsedFileName::Temporary => remove_file(&dir_entry.path()),
                ParsedFileName::Metadata(name) => {
                    let entry = entries.entry(name.to_owned()).or_insert_with(IndexEntry::new);
                    entry.metadata_size = Some(metadata.len());
                    entry.last_used = metadata.modified().unwrap_or(UNIX_EPOCH);
                },
                ParsedFileName::Body(name, id) => {
                    highest_body_id = highest_body_id.max(id);
                    entries
                        .entry(name.to_owned())
                        .or_insert_with(IndexEntry::new)
                        .bodies
                        .push((id, metadata.len()));
                },
            }
        }

        let cache = Self {
            directory,
            capacity,
            index: Mutex::new(Index::default()),
            write_lock: Mutex::new(()),
            next_body_id: AtomicU64::new(highest_body_id + 1),
        };
        {
            let mut index = cache.index.lock();
            for (name, entry) in entries {
                if entry.metadata_size.is_none() {
                    // Bodies whose metadata was never committed.
                    cache.remove_files(&name, &entry);
                    continue;
                }
                index.size += entry.size();
                index.entries.insert(name, entry);
            }
            cache.evict(&mut index);
        }
        Ok(cache)
    }

    /// The name under which the entry for `key` is filed.
    pub fn name_for(key: &str) -> String {
        Self::digest(key.as_bytes())
    }

    /// The SHA-256 of `bytes`, in hexadecimal.
    pub fn digest(bytes: &[u8]) -> String {
        let digest = Sha256::digest(bytes);
        let mut hexadecimal = String::with_capacity(digest.len() * 2);
        for byte in digest {
            hexadecimal.push_str(&format!("{byte:02x}"));
        }
        hexadecimal
    }

    /// An id for a new body file. Ids are not reused while the cache is open.
    pub fn new_body_id(&self) -> u64 {
        self.next_body_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Whether there is an entry named `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.index.lock().entries.contains_key(name)
    }

    /// The names of all entries.
    pub fn names(&self) -> Vec<String> {
        self.index.lock().entries.keys().cloned().collect()
    }

    /// The bytes all entries take together.
    pub fn size(&self) -> u64 {
        self.index.lock().size
    }

    /// Read the metadata of the entry named `name`.
    pub fn read_metadata(&self, name: &str) -> Option<Vec<u8>> {
        if !self.contains(name) {
            return None;
        }
        match fs::read(self.metadata_path(name)) {
            Ok(metadata) => Some(metadata),
            Err(error) => {
                warn!("Could not read the cache entry {name}: {error}");
                None
            },
        }
    }

    /// Read the body `id` of the entry named `name`, which its metadata says
    /// is `length` bytes long. A body of any other length is not returned.
    pub fn read_body(&self, name: &str, id: u64, length: u64) -> Option<Vec<u8>> {
        match fs::read(self.body_path(name, id)) {
            Ok(body) if body.len() as u64 == length => Some(body),
            Ok(body) => {
                warn!(
                    "The cached body {name}.{id} is {} bytes long instead of {length}",
                    body.len()
                );
                None
            },
            Err(error) => {
                warn!("Could not read the cached body {name}.{id}: {error}");
                None
            },
        }
    }

    /// Record that the entry named `name` was used, which keeps it away from
    /// eviction for longer.
    pub fn mark_used(&self, name: &str) {
        let now = SystemTime::now();
        match self.index.lock().entries.get_mut(name) {
            Some(entry) => entry.last_used = now,
            None => return,
        }
        // The next run of the browser reads the time of last use from here.
        let touched = OpenOptions::new()
            .write(true)
            .open(self.metadata_path(name))
            .and_then(|file| file.set_modified(now));
        if let Err(error) = touched {
            warn!("Could not update the time of last use of the cache entry {name}: {error}");
        }
    }

    /// Write the entry named `name`: its `metadata`, and the bodies in
    /// `new_bodies`, each with its id. `kept_bodies` are the ids of the bodies
    /// already on disk that `metadata` still uses. Every other body of the
    /// entry is removed.
    pub fn store(
        &self,
        name: &str,
        metadata: &[u8],
        new_bodies: &[(u64, Vec<u8>)],
        kept_bodies: &[u64],
    ) -> io::Result<()> {
        let _guard = self.write_lock.lock();

        for (id, body) in new_bodies {
            write_atomically(&self.body_path(name, *id), body)?;
        }
        write_atomically(&self.metadata_path(name), metadata)?;

        let mut index = self.index.lock();
        let mut entry = index
            .entries
            .remove(name)
            .unwrap_or_else(IndexEntry::new);
        index.size -= entry.size();
        let (kept, dropped): (Vec<_>, Vec<_>) = entry
            .bodies
            .into_iter()
            .partition(|(id, _size)| kept_bodies.contains(id));
        for (id, _size) in dropped {
            // A body that was just written again under the same id is not stale.
            if !new_bodies.iter().any(|(new_id, _body)| *new_id == id) {
                remove_file(&self.body_path(name, id));
            }
        }
        entry.bodies = kept;
        entry.bodies.extend(
            new_bodies
                .iter()
                .map(|(id, body)| (*id, body.len() as u64)),
        );
        entry.metadata_size = Some(metadata.len() as u64);
        entry.last_used = SystemTime::now();
        index.size += entry.size();
        index.entries.insert(name.to_owned(), entry);
        self.evict(&mut index);
        Ok(())
    }

    /// Remove the entry named `name`.
    pub fn remove(&self, name: &str) {
        let _guard = self.write_lock.lock();
        let mut index = self.index.lock();
        self.remove_indexed(&mut index, name);
    }

    /// Remove every entry.
    pub fn clear(&self) {
        let _guard = self.write_lock.lock();
        let mut index = self.index.lock();
        let names: Vec<String> = index.entries.keys().cloned().collect();
        for name in names {
            self.remove_indexed(&mut index, &name);
        }
    }

    /// Remove the entries used longest ago until the cache fits its capacity.
    fn evict(&self, index: &mut Index) {
        if index.size <= self.capacity {
            return;
        }
        let target = self.capacity / 100 * EVICTION_TARGET_PERCENT;
        let mut by_age: Vec<(SystemTime, String)> = index
            .entries
            .iter()
            .map(|(name, entry)| (entry.last_used, name.clone()))
            .collect();
        by_age.sort();
        for (_last_used, name) in by_age {
            if index.size <= target {
                break;
            }
            self.remove_indexed(index, &name);
        }
    }

    fn remove_indexed(&self, index: &mut Index, name: &str) {
        if let Some(entry) = index.entries.remove(name) {
            index.size -= entry.size();
            self.remove_files(name, &entry);
        }
    }

    fn remove_files(&self, name: &str, entry: &IndexEntry) {
        // The metadata goes first: without it the bodies are not an entry.
        if entry.metadata_size.is_some() {
            remove_file(&self.metadata_path(name));
        }
        for (id, _size) in &entry.bodies {
            remove_file(&self.body_path(name, *id));
        }
    }

    fn metadata_path(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.{METADATA_EXTENSION}"))
    }

    fn body_path(&self, name: &str, id: u64) -> PathBuf {
        self.directory
            .join(format!("{name}.{id:x}.{BODY_EXTENSION}"))
    }
}

/// What the name of a file in the cache directory says the file is.
enum ParsedFileName<'a> {
    Metadata(&'a str),
    Body(&'a str, u64),
    Temporary,
}

impl<'a> ParsedFileName<'a> {
    fn parse(file_name: &'a str) -> Option<Self> {
        let (stem, extension) = file_name.rsplit_once('.')?;
        match extension {
            TEMPORARY_EXTENSION => Some(Self::Temporary),
            METADATA_EXTENSION => Some(Self::Metadata(stem)),
            BODY_EXTENSION => {
                let (name, id) = stem.rsplit_once('.')?;
                Some(Self::Body(name, u64::from_str_radix(id, 16).ok()?))
            },
            _ => None,
        }
    }
}

/// Write through a temporary file and a rename, so that a crash mid-write
/// never leaves a truncated file under the final name.
fn write_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".");
    temporary.push(TEMPORARY_EXTENSION);
    let temporary = PathBuf::from(temporary);
    let written = File::create(&temporary).and_then(|mut file| {
        io::Write::write_all(&mut file, contents)?;
        fs::rename(&temporary, path)
    });
    if written.is_err() {
        remove_file(&temporary);
    }
    written
}

fn remove_file(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {},
        // Already gone, which is the state that was asked for.
        Err(error) if error.kind() == ErrorKind::NotFound => {},
        Err(error) => warn!("Could not remove {}: {error}", path.display()),
    }
}
