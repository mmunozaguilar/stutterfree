//! A game's recording (SCSKiller.Core App/Recordings.cs): Shaderless's copy (games/<id>/recording.db, compact) is the
//! durable one, and the recorder's shaderless.db in the game folder is an inbox, emptied once imported. The recorder
//! skips what the copy already has through [`KEYS_FILE`], next to its shaderless.ini.

use crate::psodb::{self, hex, Rec, ZERO};
use sha1::{Digest, Sha1};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub const KEYS_FILE: &str = "shaderless.keys";
pub const WARMED_FILE: &str = "shaderless.warmed";
pub const FAILED_FILE: &str = "shaderless_failed.keys";

const WARMED_MAGIC: &[u8; 8] = b"SCSKWRM1";
const KEYS_MAGIC: &[u8; 8] = b"SCSKKEY1";

/// proxy.cpp tuple_hash: SHA-1 of the root signature hash, then per stage its type byte and shader hash.
pub fn tuple_hash(rs: &str, stages: &BTreeMap<u32, String>) -> String {
    let mut h = Sha1::new();
    h.update(hex::decode(rs).unwrap_or_default());
    for (s, sh) in stages {
        h.update([*s as u8]);
        h.update(hex::decode(sh).unwrap_or_default());
    }
    hex(&h.finalize())
}

/// The [`WARMED_FILE`] of a compile's work folder dbs: the sorted keys of the pipelines and state objects it created,
/// then the sorted tuple hashes of every pipeline it created (a plan item's from its template with the item's shaders
/// and root signature). An item one of `failed` names wasn't created and counts for nothing.
pub fn warmed(dbs: &[PathBuf], failed: &[PathBuf]) -> io::Result<Vec<u8>> {
    let mut not_created = HashSet::new();
    for f in failed.iter().filter(|f| f.is_file()) {
        for c in std::fs::read(f)?.chunks(20).filter(|c| c.len() == 20) {
            not_created.insert(hex(c));
        }
    }
    let mut keys = BTreeSet::new();
    let mut tuples = BTreeSet::new();
    let mut templates: HashMap<String, Rec> = HashMap::new();
    let mut items: Vec<Vec<u8>> = Vec::new();
    for db in dbs.iter().filter(|d| d.is_file()) {
        for r in psodb::read(db)? {
            let r = r?;
            let is_pso = matches!(r.tag, b'G' | b'C' | b'S');
            let wanted = is_pso || psodb::is_state_object(r.tag) || r.tag == b'P';
            if !wanted {
                continue;
            }
            let key = r.key();
            if is_pso {
                templates.insert(key.clone(), r.clone()); // a plan item's template even when it failed itself
            }
            if not_created.contains(&key) {
                continue;
            }
            if is_pso {
                if let Ok(p) = psodb::parse(&r) {
                    tuples.insert(tuple_hash(&p.rs, &p.stages));
                }
                keys.insert(key);
            } else if psodb::is_state_object(r.tag) {
                keys.insert(key);
            } else {
                items.push(r.payload);
            }
        }
    }
    for item in items {
        let Ok((template, rs, stages)) = psodb::parse_item(&item) else { continue };
        let Some(t) = templates.get(&template) else { continue };
        let Ok(p) = psodb::parse(t) else { continue };
        let mut merged = p.stages.clone();
        for (s, h) in stages {
            if h != ZERO && (merged.contains_key(&s) || (t.tag == b'G' && (1..=5).contains(&s))) {
                merged.insert(s, h);
            }
        }
        tuples.insert(tuple_hash(if rs != ZERO { &rs } else { &p.rs }, &merged));
    }
    let mut out = Vec::with_capacity(12 + 20 * (keys.len() + tuples.len()));
    out.extend_from_slice(WARMED_MAGIC);
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for k in keys.iter().chain(tuples.iter()) {
        out.extend_from_slice(&hex::decode(k).unwrap_or_default());
    }
    Ok(out)
}

// ---- the store lock -------------------------------------------------------------------------------------------------

thread_local! {
    static HELD: RefCell<HashMap<PathBuf, (File, usize)>> = RefCell::new(HashMap::new());
}

/// Held around a read-modify-write of a store (import, compaction, clearing) by every process: `<store>.lock`,
/// flock'ed exclusively (re-entrant on one thread). A temp file found under it belongs to a writer that is gone:
/// removed once not written for an hour, or written before the PC started.
pub struct StoreLock {
    file: PathBuf,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        HELD.with(|h| {
            let mut h = h.borrow_mut();
            let remove = match h.get_mut(&self.file) {
                Some((_, n)) => {
                    *n -= 1;
                    *n == 0
                }
                None => false,
            };
            if remove {
                if let Some((f, _)) = h.remove(&self.file) {
                    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
                }
            }
        });
    }
}

pub fn lock(store: &Path) -> io::Result<StoreLock> {
    lock_wait(store, Duration::from_secs(600))
}

pub fn lock_wait(store: &Path, wait: Duration) -> io::Result<StoreLock> {
    let full = std::path::absolute(store).unwrap_or_else(|_| store.to_path_buf());
    let mut lock_name = full.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_file = PathBuf::from(lock_name);
    let reentered = HELD.with(|h| {
        if let Some((_, n)) = h.borrow_mut().get_mut(&lock_file) {
            *n += 1;
            true
        } else {
            false
        }
    });
    if !reentered {
        if let Some(d) = lock_file.parent() {
            std::fs::create_dir_all(d)?;
        }
        let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock_file)?;
        let start = Instant::now();
        loop {
            let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                break;
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EWOULDBLOCK) || start.elapsed() > wait {
                return Err(err);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        HELD.with(|h| h.borrow_mut().insert(lock_file.clone(), (f, 1)));
    }
    // stale temp files of writers that are gone
    if let (Some(dir), Some(name)) = (full.parent(), full.file_name()) {
        let prefix = format!("{}.", name.to_string_lossy());
        let booted = boot_time();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with(&prefix) && n.ends_with("tmp") {
                    if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                        let old = SystemTime::now().duration_since(m).map(|d| d > Duration::from_secs(3600)).unwrap_or(false);
                        if old || booted.map(|b| m < b).unwrap_or(false) {
                            let _ = std::fs::remove_file(e.path());
                        }
                    }
                }
            }
        }
    }
    Ok(StoreLock { file: lock_file })
}

fn boot_time() -> Option<SystemTime> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().find(|l| l.starts_with("btime "))?;
    let secs: u64 = line[6..].trim().parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

// ---- merge / keys / rotate ------------------------------------------------------------------------------------------

/// A blob by its hash (cheaper than hashing its bytes again), any other record by its key.
fn id(r: &Rec) -> String {
    if r.tag == b'B' {
        format!("B{}", r.blob_hash().unwrap_or_default())
    } else {
        r.key()
    }
}

/// Writes `store` (none: empty) plus the records of `inbox` it lacks, in that order, as a compact recording at
/// `store`. Returns the keys of the pipeline and state object records it added.
pub fn merge(store: &Path, inbox: Option<&Path>) -> io::Result<HashSet<String>> {
    let has_store = store.is_file();
    let inbox = inbox.filter(|p| p.is_file());
    let seen = RefCell::new(HashSet::<String>::new());
    let added = RefCell::new(HashSet::<String>::new());
    let store_iter: Box<dyn Iterator<Item = io::Result<Rec>>> = if has_store { Box::new(psodb::read(store)?) } else { Box::new(std::iter::empty()) };
    let inbox_iter: Box<dyn Iterator<Item = io::Result<Rec>>> = match inbox {
        Some(p) => Box::new(psodb::read(p)?),
        None => Box::new(std::iter::empty()),
    };
    let from_store = store_iter.filter_map(|r| match r {
        Ok(r) => seen.borrow_mut().insert(id(&r)).then_some(Ok(r)),
        Err(e) => Some(Err(e)),
    });
    let from_inbox = inbox_iter.filter_map(|r| match r {
        Ok(r) => {
            let i = id(&r);
            if seen.borrow_mut().insert(i.clone()) {
                if r.tag != b'B' && r.tag != b'W' && r.tag != b'N' {
                    added.borrow_mut().insert(i);
                }
                Some(Ok(r))
            } else {
                None
            }
        }
        Err(e) => Some(Err(e)),
    });
    psodb::write_compact(store, from_store.chain(from_inbox))?;
    Ok(added.into_inner())
}

/// Writes `path` (the recorder's keys file): every blob of `store` and the key of every record it holds that replays
/// from them. A record naming a blob the store lacks is left out, so the recorder records it again, with its bytes, if
/// the game still creates it. Returns how many were left out. With nothing to name, no file.
pub fn write_keys(store: &Path, path: &Path) -> io::Result<usize> {
    let mut blobs = HashSet::new();
    let stored = store.is_file();
    if stored {
        for r in psodb::read(store)? {
            let r = r?;
            if r.tag == b'B' {
                if let Some(h) = r.blob_hash() {
                    blobs.insert(h);
                }
            }
        }
    }
    let kept_fn = |r: &Rec| r.tag == b'N' || psodb::references([r]).map(|refs| refs.iter().all(|h| blobs.contains(h))).unwrap_or(false);
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    let (mut kept, mut left) = (0usize, 0usize);
    let result = (|| -> io::Result<()> {
        let mut f = BufWriter::with_capacity(1 << 16, File::create(&tmp)?);
        f.write_all(KEYS_MAGIC)?;
        for h in &blobs {
            f.write_all(&hex::decode(h).unwrap_or_default())?;
        }
        if stored {
            for r in psodb::read(store)? {
                let r = r?;
                if r.tag == b'B' {
                    continue;
                }
                if kept_fn(&r) {
                    f.write_all(&hex::decode(r.key()).unwrap_or_default())?;
                    kept += 1;
                } else {
                    left += 1;
                }
            }
        }
        f.flush()?;
        drop(f);
        if blobs.len() + kept == 0 {
            let _ = std::fs::remove_file(path);
        } else {
            std::fs::rename(&tmp, path)?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    result.map(|_| left)
}

/// Empties the recorder's shaderless.db once imported, only at the `imported` length it was read at: what came after
/// isn't imported.
pub fn rotate(inbox: &Path, imported: u64) -> bool {
    let Ok(f) = OpenOptions::new().read(true).write(true).open(inbox) else { return false };
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return false;
    }
    let ok = f.metadata().map(|m| m.len() == imported).unwrap_or(false) && f.set_len(0).is_ok();
    unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compute(rs: u8, cs: u8) -> Rec {
        let mut p = vec![rs; 20];
        p.extend(vec![cs; 20]);
        p.extend([0u8; 8]);
        Rec::new(b'C', p)
    }

    fn blob(h: u8) -> Rec {
        let mut p = vec![h; 20];
        p.extend(b"DXBC");
        Rec::new(b'B', p)
    }

    #[test]
    fn merge_dedups_and_reports_added() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("recording.db");
        let inbox = dir.path().join("shaderless.db");
        psodb::write_compact(&store, vec![Ok(blob(1)), Ok(compute(1, 1))]).unwrap();
        psodb::write_raw_file(&inbox, &[blob(1), blob(2), compute(1, 1), compute(1, 2)]).unwrap();
        let _l = lock(&store).unwrap();
        let _l2 = lock(&store).unwrap(); // re-entrant
        let added = merge(&store, Some(&inbox)).unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(psodb::read_all(&store).unwrap().len(), 4);
        assert!(rotate(&inbox, std::fs::metadata(&inbox).unwrap().len()));
        assert_eq!(std::fs::metadata(&inbox).unwrap().len(), 0);
    }

    #[test]
    fn keys_and_warmed() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("recording.db");
        psodb::write_compact(&store, vec![Ok(blob(1)), Ok(compute(1, 1)), Ok(compute(2, 2))]).unwrap();
        let keys = dir.path().join(KEYS_FILE);
        let left = write_keys(&store, &keys).unwrap();
        assert_eq!(left, 1); // compute(2,2) names blobs the store lacks
        let k = std::fs::read(&keys).unwrap();
        assert_eq!(&k[..8], KEYS_MAGIC);
        assert_eq!(k.len(), 8 + 20 + 20);

        let w = warmed(&[store.clone()], &[]).unwrap();
        assert_eq!(&w[..8], WARMED_MAGIC);
        assert_eq!(u32::from_le_bytes(w[8..12].try_into().unwrap()), 2);
    }
}
