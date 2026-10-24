//! The web build's file system: a tree in memory, over read-only zip mounts.
//!
//! The prepared resources arrive as the player's zip of their resources
//! folder and are read from it in place (`mount_zip`): a file is inflated
//! when it is opened. Everything the game writes lives in the tree; a write
//! under a mounted folder shadows the zip's file. The folders `persist` names
//! (the game folder) are kept by the page in IndexedDB: `drain` hands it
//! what changed since the last call.
use crate::sync::Mutex;
use crate::time::SystemTime;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsString,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, MutexGuard},
};

struct Data {
    bytes: Vec<u8>,
    modified: SystemTime,
}
type Shared = Arc<Mutex<Data>>;

enum Node {
    Dir(SystemTime),
    File(Shared),
}

struct Mount {
    /// Where it is mounted, normalized.
    at: String,
    archive: zip::ZipArchive<Cursor<Arc<[u8]>>>,
    /// The archive's bytes, and where each entry's lie in them (`unzip`).
    bytes: Arc<[u8]>,
    raw: Vec<Option<Raw>>,
    /// The archive's files under `at`: entry index and size.
    files: HashMap<String, (usize, u64)>,
    /// The archive's folders under `at` ("" is `at` itself) and their children.
    dirs: HashMap<String, BTreeSet<String>>,
    modified: SystemTime,
}
impl Mount {
    /// `path` relative to the mount, if it is inside.
    fn relative<'a>(&self, path: &'a str) -> Option<&'a str> {
        if path == self.at {
            return Some("");
        }
        path.strip_prefix(self.at.as_str())?.strip_prefix('/')
    }
}

/// A stored or deflated zip entry's bytes in the archive.
#[derive(Clone, Copy)]
struct Raw {
    start: usize,
    len: usize,
    deflated: bool,
}

/// A zipped file read without holding the file system: the threaded build's
/// workers read sounds and models out of the resources while the page's
/// thread opens files.
struct Unzip {
    bytes: Arc<[u8]>,
    raw: Raw,
    size: u64,
    modified: SystemTime,
}
impl Unzip {
    fn data(&self, path: &str) -> io::Result<Data> {
        let packed = self
            .bytes
            .get(self.raw.start..self.raw.start + self.raw.len)
            .ok_or_else(|| error(io::ErrorKind::InvalidData, path, "entry past the zip's end"))?;
        let bytes = if self.raw.deflated {
            let mut bytes = Vec::with_capacity(self.size as usize);
            flate2::read::DeflateDecoder::new(packed).read_to_end(&mut bytes)?;
            bytes
        } else {
            packed.to_vec()
        };
        Ok(Data {
            bytes,
            modified: self.modified,
        })
    }
}

/// What a path is.
enum Found {
    Dir(SystemTime),
    File(Shared),
    /// A file in mount `.0`, entry `.1`, `.2` bytes.
    Zipped(usize, usize, u64),
}

pub(crate) struct Vfs {
    nodes: BTreeMap<String, Node>,
    mounts: Vec<Mount>,
    cwd: String,
    persisted: Vec<String>,
    changed: BTreeSet<String>,
}

static VFS: LazyLock<Mutex<Vfs>> = LazyLock::new(|| {
    let now = SystemTime::now();
    let mut nodes = BTreeMap::new();
    for dir in ["/", "/app", "/home", crate::env::HOME, "/tmp"] {
        nodes.insert(dir.to_owned(), Node::Dir(now));
    }
    Mutex::new(Vfs {
        nodes,
        mounts: Vec::new(),
        cwd: crate::env::HOME.to_owned(),
        persisted: Vec::new(),
        changed: BTreeSet::new(),
    })
});

fn vfs() -> MutexGuard<'static, Vfs> {
    VFS.lock().unwrap_or_else(|e| e.into_inner())
}

fn not_found(path: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{path}: no such file or directory"),
    )
}
fn error(kind: io::ErrorKind, path: &str, what: &str) -> io::Error {
    io::Error::new(kind, format!("{path}: {what}"))
}

fn parent(path: &str) -> Option<&str> {
    if path == "/" {
        return None;
    }
    Some(match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    })
}
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}
fn child(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

impl Vfs {
    /// `path` as an absolute path with no `.`, `..` or repeated separators.
    fn normalize(&self, path: &Path) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut rooted = false;
        for component in path.components() {
            match component {
                Component::RootDir => {
                    parts.clear();
                    rooted = true;
                }
                Component::CurDir | Component::Prefix(_) => {}
                Component::ParentDir => {
                    parts.pop();
                }
                Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            }
        }
        let mut out = if rooted {
            String::new()
        } else {
            self.cwd.trim_end_matches('/').to_owned()
        };
        for part in parts {
            out.push('/');
            out.push_str(&part);
        }
        if out.is_empty() { "/".to_owned() } else { out }
    }

    fn find(&mut self, path: &str) -> Option<Found> {
        match self.nodes.get(path) {
            Some(Node::Dir(time)) => return Some(Found::Dir(*time)),
            Some(Node::File(data)) => return Some(Found::File(data.clone())),
            None => {}
        }
        for (i, mount) in self.mounts.iter().enumerate() {
            let Some(rel) = mount.relative(path) else {
                // The folders a mount point sits in.
                if mount.at.starts_with(path)
                    && (path == "/" || mount.at[path.len()..].starts_with('/'))
                {
                    return Some(Found::Dir(mount.modified));
                }
                continue;
            };
            if let Some(&(index, size)) = mount.files.get(rel) {
                return Some(Found::Zipped(i, index, size));
            }
            if mount.dirs.contains_key(rel) {
                return Some(Found::Dir(mount.modified));
            }
        }
        None
    }
    fn is_dir(&mut self, path: &str) -> bool {
        matches!(self.find(path), Some(Found::Dir(_)))
    }
    /// Entry `index` of mount `mount`, to read once the lock is let go
    /// (stored and deflated entries, which are all a resources zip holds).
    fn unzip(&self, mount: usize, index: usize, size: u64) -> Option<Unzip> {
        let mount = &self.mounts[mount];
        Some(Unzip {
            bytes: mount.bytes.clone(),
            raw: (*mount.raw.get(index)?)?,
            size,
            modified: mount.modified,
        })
    }
    fn inflate(&mut self, mount: usize, index: usize, size: u64, path: &str) -> io::Result<Data> {
        let mount = &mut self.mounts[mount];
        let modified = mount.modified;
        let mut entry = mount
            .archive
            .by_index(index)
            .map_err(|e| error(io::ErrorKind::InvalidData, path, &e.to_string()))?;
        let mut bytes = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut bytes)?;
        Ok(Data { bytes, modified })
    }
    fn persisted(&self, path: &str) -> bool {
        self.persisted.iter().any(|root| {
            path == root
                || path
                    .strip_prefix(root.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        })
    }
    fn touch(&mut self, path: &str) {
        if self.persisted(path) {
            self.changed.insert(path.to_owned());
        }
    }
    /// The parent of `path` must be a folder.
    fn check_parent(&mut self, path: &str) -> io::Result<()> {
        match parent(path) {
            Some(dir) if !self.is_dir(dir) => Err(not_found(dir)),
            _ => Ok(()),
        }
    }
    fn put_dir(&mut self, path: &str) {
        self.nodes
            .insert(path.to_owned(), Node::Dir(SystemTime::now()));
        self.touch(path);
    }
    fn put_file(&mut self, path: &str, data: Data) -> Shared {
        let shared = Arc::new(Mutex::new(data));
        self.nodes
            .insert(path.to_owned(), Node::File(shared.clone()));
        self.touch(path);
        shared
    }
    /// The tree's paths strictly inside `dir`.
    fn descendants(&self, dir: &str) -> Vec<String> {
        let prefix = if dir == "/" {
            "/".to_owned()
        } else {
            format!("{dir}/")
        };
        self.nodes
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone())
            .collect()
    }
    fn children(&mut self, dir: &str) -> BTreeSet<String> {
        let prefix = if dir == "/" {
            "/".to_owned()
        } else {
            format!("{dir}/")
        };
        let mut names: BTreeSet<String> = self
            .nodes
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .filter_map(|(k, _)| {
                let rest = &k[prefix.len()..];
                (!rest.is_empty() && !rest.contains('/')).then(|| rest.to_owned())
            })
            .collect();
        for mount in &self.mounts {
            match mount.relative(dir) {
                Some(rel) => {
                    if let Some(kids) = mount.dirs.get(rel) {
                        names.extend(kids.iter().cloned());
                    }
                }
                None => {
                    if let Some(rest) = mount.at.strip_prefix(&prefix) {
                        names.insert(rest.split('/').next().unwrap_or(rest).to_owned());
                    }
                }
            }
        }
        names
    }
    /// Opens `path` as `options` say.
    fn open(&mut self, path: &str, options: &OpenOptions) -> io::Result<File> {
        let writes = options.write || options.append;
        let found = self.find(path);
        let shared = match found {
            Some(Found::Dir(_)) => {
                return Err(error(io::ErrorKind::IsADirectory, path, "is a directory"));
            }
            Some(_) if options.create_new => {
                return Err(error(io::ErrorKind::AlreadyExists, path, "file exists"));
            }
            Some(Found::File(shared)) => {
                if options.truncate && writes {
                    let mut data = shared.lock().unwrap();
                    data.bytes.clear();
                    data.modified = SystemTime::now();
                    drop(data);
                    self.touch(path);
                }
                shared
            }
            Some(Found::Zipped(mount, index, size)) => {
                let data = if options.truncate && writes {
                    Data {
                        bytes: Vec::new(),
                        modified: SystemTime::now(),
                    }
                } else {
                    self.inflate(mount, index, size, path)?
                };
                if writes {
                    // Written files shadow the zip's.
                    self.put_file(path, data)
                } else {
                    Arc::new(Mutex::new(data))
                }
            }
            None if writes && (options.create || options.create_new) => {
                self.check_parent(path)?;
                self.put_file(
                    path,
                    Data {
                        bytes: Vec::new(),
                        modified: SystemTime::now(),
                    },
                )
            }
            None => return Err(not_found(path)),
        };
        let pos = if options.append {
            shared.lock().unwrap().bytes.len() as u64
        } else {
            0
        };
        Ok(File {
            data: shared,
            path: path.to_owned(),
            pos,
            read: options.read,
            write: writes,
            append: options.append,
        })
    }
    fn metadata(&mut self, path: &str) -> io::Result<Metadata> {
        match self.find(path) {
            Some(Found::Dir(modified)) => Ok(Metadata {
                dir: true,
                len: 0,
                modified,
            }),
            Some(Found::File(shared)) => {
                let data = shared.lock().unwrap();
                Ok(Metadata {
                    dir: false,
                    len: data.bytes.len() as u64,
                    modified: data.modified,
                })
            }
            Some(Found::Zipped(mount, _, len)) => Ok(Metadata {
                dir: false,
                len,
                modified: self.mounts[mount].modified,
            }),
            None => Err(not_found(path)),
        }
    }
    fn remove_file(&mut self, path: &str) -> io::Result<()> {
        match self.nodes.get(path) {
            Some(Node::File(_)) => {
                self.nodes.remove(path);
                self.touch(path);
                Ok(())
            }
            Some(Node::Dir(_)) => Err(error(io::ErrorKind::IsADirectory, path, "is a directory")),
            None => match self.find(path) {
                Some(_) => Err(error(io::ErrorKind::PermissionDenied, path, "read-only")),
                None => Err(not_found(path)),
            },
        }
    }
    fn remove_tree(&mut self, path: &str) {
        for p in self.descendants(path) {
            self.nodes.remove(&p);
            self.touch(&p);
        }
        self.nodes.remove(path);
        self.touch(path);
    }
    fn rename(&mut self, from: &str, to: &str) -> io::Result<()> {
        if from == to {
            return Ok(());
        }
        match self.find(from) {
            None => return Err(not_found(from)),
            Some(Found::Zipped(mount, index, size)) => {
                let data = self.inflate(mount, index, size, from)?;
                self.check_parent(to)?;
                self.put_file(to, data);
                return Ok(());
            }
            Some(Found::File(_)) => {
                self.check_parent(to)?;
                if self.is_dir(to) {
                    return Err(error(io::ErrorKind::IsADirectory, to, "is a directory"));
                }
                let node = self.nodes.remove(from).expect("found");
                self.touch(from);
                self.nodes.insert(to.to_owned(), node);
                self.touch(to);
            }
            Some(Found::Dir(_)) => {
                if to.starts_with(&format!("{from}/")) {
                    return Err(error(
                        io::ErrorKind::InvalidInput,
                        to,
                        "inside the folder moved",
                    ));
                }
                self.check_parent(to)?;
                if self.is_dir(to) && !self.children(to).is_empty() {
                    return Err(error(
                        io::ErrorKind::DirectoryNotEmpty,
                        to,
                        "directory not empty",
                    ));
                }
                let moved = self.descendants(from);
                let node = self
                    .nodes
                    .remove(from)
                    .unwrap_or(Node::Dir(SystemTime::now()));
                self.touch(from);
                self.remove_tree(to);
                self.nodes.insert(to.to_owned(), node);
                self.touch(to);
                for old in moved {
                    let new = format!("{to}{}", &old[from.len()..]);
                    if let Some(node) = self.nodes.remove(&old) {
                        self.nodes.insert(new.clone(), node);
                    }
                    self.touch(&old);
                    self.touch(&new);
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn cwd() -> PathBuf {
    PathBuf::from(&vfs().cwd)
}
pub(crate) fn set_cwd(path: &Path) -> io::Result<()> {
    let mut v = vfs();
    let path = v.normalize(path);
    if !v.is_dir(&path) {
        return Err(not_found(&path));
    }
    v.cwd = path;
    Ok(())
}

/// Mounts the zip `bytes` read-only at `at`. With `marker`, the folder in
/// the zip that holds the shallowest file of that name is what is mounted
/// (a zip of `26.3/` or of `resources/` mounts the same).
pub(crate) fn mount_zip(at: &Path, bytes: Vec<u8>, marker: Option<&str>) -> io::Result<()> {
    let bytes: Arc<[u8]> = bytes.into();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes.clone()))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut names = Vec::with_capacity(archive.len());
    let mut raw = Vec::with_capacity(archive.len());
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        names.push((
            entry.name().replace('\\', "/"),
            entry.is_dir(),
            entry.size(),
        ));
        let deflated = match entry.compression() {
            zip::CompressionMethod::Stored => Some(false),
            zip::CompressionMethod::Deflated => Some(true),
            _ => None,
        };
        raw.push(deflated.and_then(|deflated| {
            Some(Raw {
                start: usize::try_from(entry.data_start()).ok()?,
                len: usize::try_from(entry.compressed_size()).ok()?,
                deflated,
            })
        }));
    }
    let prefix = marker
        .and_then(|marker| {
            names
                .iter()
                .filter(|(name, dir, _)| !dir && file_name(name) == marker)
                .map(|(name, _, _)| &name[..name.len() - marker.len()])
                .min_by_key(|prefix| prefix.matches('/').count())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let mut files = HashMap::new();
    let mut dirs: HashMap<String, BTreeSet<String>> = HashMap::new();
    dirs.insert(String::new(), BTreeSet::new());
    for (i, (name, dir, size)) in names.into_iter().enumerate() {
        let Some(rel) = name.strip_prefix(&prefix) else {
            continue;
        };
        let rel = rel.trim_matches('/');
        if rel.is_empty()
            || rel
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
        {
            continue;
        }
        // Every folder on the way, explicit in the zip or not.
        let mut at_dir = String::new();
        let parts: Vec<&str> = rel.split('/').collect();
        for (n, part) in parts.iter().enumerate() {
            let last = n + 1 == parts.len();
            dirs.entry(at_dir.clone())
                .or_default()
                .insert((*part).to_owned());
            if last && !dir {
                break;
            }
            at_dir = if at_dir.is_empty() {
                (*part).to_owned()
            } else {
                format!("{at_dir}/{part}")
            };
            dirs.entry(at_dir.clone()).or_default();
        }
        if !dir {
            files.insert(rel.to_owned(), (i, size));
        }
    }
    let mut v = vfs();
    let at = v.normalize(at);
    v.mounts.retain(|m| m.at != at);
    v.mounts.push(Mount {
        at,
        archive,
        bytes,
        raw,
        files,
        dirs,
        modified: SystemTime::now(),
    });
    Ok(())
}

/// Keeps what changes under `root` for [`drain`].
pub(crate) fn persist(root: &Path) {
    let mut v = vfs();
    let root = v.normalize(root);
    v.persisted.push(root);
}

/// A kept path's state, for the page's store.
pub(crate) enum Kept {
    Dir(SystemTime),
    File(Vec<u8>, SystemTime),
    Removed,
}
/// What changed under the kept folders since the last call.
pub(crate) fn drain() -> Vec<(String, Kept)> {
    let mut v = vfs();
    let changed = std::mem::take(&mut v.changed);
    changed
        .into_iter()
        .map(|path| {
            let kept = match v.nodes.get(&path) {
                Some(Node::Dir(time)) => Kept::Dir(*time),
                Some(Node::File(shared)) => {
                    let data = shared.lock().unwrap();
                    Kept::File(data.bytes.clone(), data.modified)
                }
                None => Kept::Removed,
            };
            (path, kept)
        })
        .collect()
}
/// Puts back a kept path (and the folders it is in) without marking it changed.
pub(crate) fn restore(path: &str, kept: Kept) {
    let mut v = vfs();
    let path = v.normalize(Path::new(path));
    let mut dir = parent(&path);
    while let Some(d) = dir {
        if !v.nodes.contains_key(d) {
            v.nodes.insert(d.to_owned(), Node::Dir(SystemTime::now()));
        }
        dir = parent(d);
    }
    match kept {
        Kept::Dir(time) => {
            v.nodes.insert(path, Node::Dir(time));
        }
        Kept::File(bytes, modified) => {
            v.nodes.insert(
                path,
                Node::File(Arc::new(Mutex::new(Data { bytes, modified }))),
            );
        }
        Kept::Removed => {
            v.nodes.remove(&path);
        }
    }
}

/// An open file. Files in the tree are shared with it: what one handle
/// writes, the others and later opens read.
#[derive(Debug)]
pub struct File {
    data: Shared,
    path: String,
    pos: u64,
    read: bool,
    write: bool,
    append: bool,
}
impl std::fmt::Debug for Data {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Data")
            .field("len", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

#[derive(Clone, Debug)]
pub struct Metadata {
    dir: bool,
    len: u64,
    modified: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileType {
    dir: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permissions {
    readonly: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FileTimes {
    modified: Option<SystemTime>,
}

#[derive(Debug)]
pub struct DirEntry {
    dir: String,
    name: String,
}

#[derive(Debug)]
pub struct ReadDir {
    entries: std::vec::IntoIter<DirEntry>,
}

pub mod fs {
    //! The std::fs names over [`super`]'s tree.
    use super::*;
    pub use super::{
        DirEntry, File, FileTimes, FileType, Metadata, OpenOptions, Permissions, ReadDir,
        TryLockError,
    };

    fn norm(path: &Path) -> (MutexGuard<'static, Vfs>, String) {
        let v = vfs();
        let p = v.normalize(path);
        (v, p)
    }

    pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
        let (mut v, p) = norm(path.as_ref());
        match v.find(&p) {
            Some(Found::File(shared)) => Ok(shared.lock().unwrap().bytes.clone()),
            Some(Found::Zipped(mount, index, size)) => match v.unzip(mount, index, size) {
                Some(unzip) => {
                    drop(v);
                    Ok(unzip.data(&p)?.bytes)
                }
                None => Ok(v.inflate(mount, index, size, &p)?.bytes),
            },
            Some(Found::Dir(_)) => Err(error(io::ErrorKind::IsADirectory, &p, "is a directory")),
            None => Err(not_found(&p)),
        }
    }
    pub fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
        String::from_utf8(read(path)?).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        })
    }
    pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
        let (mut v, p) = norm(path.as_ref());
        match v.nodes.get(&p) {
            Some(Node::Dir(_)) => {
                return Err(error(io::ErrorKind::IsADirectory, &p, "is a directory"));
            }
            Some(Node::File(shared)) => {
                let mut data = shared.lock().unwrap();
                data.bytes.clear();
                data.bytes.extend_from_slice(contents.as_ref());
                data.modified = SystemTime::now();
                drop(data);
                v.touch(&p);
                return Ok(());
            }
            None => {}
        }
        v.check_parent(&p)?;
        if v.is_dir(&p) {
            return Err(error(io::ErrorKind::IsADirectory, &p, "is a directory"));
        }
        v.put_file(
            &p,
            Data {
                bytes: contents.as_ref().to_vec(),
                modified: SystemTime::now(),
            },
        );
        Ok(())
    }
    pub fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
        let (mut v, p) = norm(path.as_ref());
        if v.find(&p).is_some() {
            return Err(error(io::ErrorKind::AlreadyExists, &p, "file exists"));
        }
        v.check_parent(&p)?;
        v.put_dir(&p);
        Ok(())
    }
    pub fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
        let (mut v, p) = norm(path.as_ref());
        let mut missing = Vec::new();
        let mut at = Some(p.as_str());
        while let Some(dir) = at {
            match v.find(dir) {
                Some(Found::Dir(_)) => break,
                Some(_) => return Err(error(io::ErrorKind::AlreadyExists, dir, "not a directory")),
                None => missing.push(dir.to_owned()),
            }
            at = parent(dir);
        }
        for dir in missing.iter().rev() {
            v.put_dir(dir);
        }
        Ok(())
    }
    pub fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
        let (mut v, p) = norm(path.as_ref());
        v.remove_file(&p)
    }
    pub fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
        let (mut v, p) = norm(path.as_ref());
        let node = v.nodes.get(&p).map(|n| matches!(n, Node::Dir(_)));
        match node {
            Some(true) => {
                if !v.children(&p).is_empty() {
                    return Err(error(
                        io::ErrorKind::DirectoryNotEmpty,
                        &p,
                        "directory not empty",
                    ));
                }
                v.nodes.remove(&p);
                v.touch(&p);
                Ok(())
            }
            Some(false) => Err(error(io::ErrorKind::NotADirectory, &p, "not a directory")),
            None if v.find(&p).is_some() => {
                Err(error(io::ErrorKind::PermissionDenied, &p, "read-only"))
            }
            None => Err(not_found(&p)),
        }
    }
    pub fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
        let (mut v, p) = norm(path.as_ref());
        match v.find(&p) {
            Some(Found::Dir(_)) => {
                v.remove_tree(&p);
                Ok(())
            }
            Some(_) => Err(error(io::ErrorKind::NotADirectory, &p, "not a directory")),
            None => Err(not_found(&p)),
        }
    }
    pub fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
        let (mut v, p) = norm(path.as_ref());
        match v.find(&p) {
            Some(Found::Dir(_)) => {}
            Some(_) => return Err(error(io::ErrorKind::NotADirectory, &p, "not a directory")),
            None => return Err(not_found(&p)),
        }
        let entries: Vec<DirEntry> = v
            .children(&p)
            .into_iter()
            .map(|name| DirEntry {
                dir: p.clone(),
                name,
            })
            .collect();
        Ok(ReadDir {
            entries: entries.into_iter(),
        })
    }
    pub fn metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
        let (mut v, p) = norm(path.as_ref());
        v.metadata(&p)
    }
    pub fn symlink_metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
        metadata(path)
    }
    pub fn exists<P: AsRef<Path>>(path: P) -> io::Result<bool> {
        Ok(metadata(path).is_ok())
    }
    pub fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
        let mut v = vfs();
        let (from, to) = (v.normalize(from.as_ref()), v.normalize(to.as_ref()));
        v.rename(&from, &to)
    }
    pub fn copy<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<u64> {
        let bytes = read(from)?;
        write(to, &bytes)?;
        Ok(bytes.len() as u64)
    }
    pub fn hard_link<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
        copy(from, to).map(|_| ())
    }
    pub fn canonicalize<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
        let (mut v, p) = norm(path.as_ref());
        match v.find(&p) {
            Some(_) => Ok(PathBuf::from(p)),
            None => Err(not_found(&p)),
        }
    }
    pub fn read_link<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
        let p = vfs().normalize(path.as_ref());
        Err(error(
            io::ErrorKind::InvalidInput,
            &p,
            "not a symbolic link",
        ))
    }
    pub fn set_permissions<P: AsRef<Path>>(path: P, _: Permissions) -> io::Result<()> {
        metadata(path).map(|_| ())
    }
}

impl File {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions::new().read(true).open(path)
    }
    pub fn create<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }
    pub fn create_new<P: AsRef<Path>>(path: P) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    }
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        let data = self.data.lock().unwrap();
        Ok(Metadata {
            dir: false,
            len: data.bytes.len() as u64,
            modified: data.modified,
        })
    }
    pub fn set_len(&self, size: u64) -> io::Result<()> {
        self.writable()?;
        let mut data = self.data.lock().unwrap();
        data.bytes.resize(size as usize, 0);
        data.modified = SystemTime::now();
        drop(data);
        vfs().touch(&self.path);
        Ok(())
    }
    pub fn sync_all(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn sync_data(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn try_clone(&self) -> io::Result<File> {
        Ok(File {
            data: self.data.clone(),
            path: self.path.clone(),
            pos: self.pos,
            read: self.read,
            write: self.write,
            append: self.append,
        })
    }
    pub fn set_times(&self, times: FileTimes) -> io::Result<()> {
        if let Some(modified) = times.modified {
            self.data.lock().unwrap().modified = modified;
            vfs().touch(&self.path);
        }
        Ok(())
    }
    pub fn set_modified(&self, time: SystemTime) -> io::Result<()> {
        self.set_times(FileTimes::new().set_modified(time))
    }
    pub fn set_permissions(&self, _: Permissions) -> io::Result<()> {
        Ok(())
    }
    /// One game runs per page: locks always succeed.
    pub fn lock(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn lock_shared(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn try_lock(&self) -> Result<(), TryLockError> {
        Ok(())
    }
    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Ok(())
    }
    pub fn unlock(&self) -> io::Result<()> {
        Ok(())
    }
    fn writable(&self) -> io::Result<()> {
        if self.write {
            Ok(())
        } else {
            Err(error(
                io::ErrorKind::PermissionDenied,
                &self.path,
                "not open for writing",
            ))
        }
    }
    fn read_at(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.read {
            return Err(error(
                io::ErrorKind::PermissionDenied,
                &self.path,
                "not open for reading",
            ));
        }
        let data = self.data.lock().unwrap();
        let start = (self.pos as usize).min(data.bytes.len());
        let n = buf.len().min(data.bytes.len() - start);
        buf[..n].copy_from_slice(&data.bytes[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
    fn write_at(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writable()?;
        let mut data = self.data.lock().unwrap();
        if self.append {
            self.pos = data.bytes.len() as u64;
        }
        let start = self.pos as usize;
        let end = start + buf.len();
        if data.bytes.len() < end {
            data.bytes.resize(end, 0);
        }
        data.bytes[start..end].copy_from_slice(buf);
        data.modified = SystemTime::now();
        drop(data);
        self.pos = end as u64;
        vfs().touch(&self.path);
        Ok(buf.len())
    }
    fn seek_to(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = self.data.lock().unwrap().bytes.len() as i64;
        let target = match pos {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => len + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}
impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_at(buf)
    }
}
impl Write for File {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_at(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Seek for File {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.seek_to(pos)
    }
}

impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }
    pub fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        let (mut v, p) = {
            let v = vfs();
            let p = v.normalize(path.as_ref());
            (v, p)
        };
        // A zipped file opened to read is read once the lock is let go.
        let reads_only = !(self.write || self.append || self.truncate || self.create_new);
        if reads_only
            && let Some(Found::Zipped(mount, index, size)) = v.find(&p)
            && let Some(unzip) = v.unzip(mount, index, size)
        {
            drop(v);
            return Ok(File {
                data: Arc::new(Mutex::new(unzip.data(&p)?)),
                path: p,
                pos: 0,
                read: self.read,
                write: false,
                append: false,
            });
        }
        v.open(&p, self)
    }
}

// std's Metadata has no `is_empty` either.
#[allow(clippy::len_without_is_empty)]
impl Metadata {
    pub fn is_dir(&self) -> bool {
        self.dir
    }
    pub fn is_file(&self) -> bool {
        !self.dir
    }
    pub fn is_symlink(&self) -> bool {
        false
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn file_type(&self) -> FileType {
        FileType { dir: self.dir }
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }
    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }
    pub fn created(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }
    pub fn permissions(&self) -> Permissions {
        Permissions { readonly: false }
    }
}
impl FileType {
    pub fn is_dir(&self) -> bool {
        self.dir
    }
    pub fn is_file(&self) -> bool {
        !self.dir
    }
    pub fn is_symlink(&self) -> bool {
        false
    }
}
impl Permissions {
    pub fn readonly(&self) -> bool {
        self.readonly
    }
    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }
}
impl FileTimes {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_modified(mut self, time: SystemTime) -> Self {
        self.modified = Some(time);
        self
    }
    pub fn set_accessed(self, _: SystemTime) -> Self {
        self
    }
}
impl DirEntry {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(child(&self.dir, &self.name))
    }
    pub fn file_name(&self) -> OsString {
        OsString::from(&self.name)
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        vfs().metadata(&child(&self.dir, &self.name))
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        self.metadata().map(|m| m.file_type())
    }
}
impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(Ok)
    }
}

/// `std::fs::TryLockError`.
#[derive(Debug)]
pub enum TryLockError {
    Error(io::Error),
    WouldBlock,
}
impl std::fmt::Display for TryLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TryLockError::Error(e) => e.fmt(f),
            TryLockError::WouldBlock => {
                f.write_str("try_lock failed because the operation would block")
            }
        }
    }
}
impl std::error::Error for TryLockError {}
impl From<TryLockError> for io::Error {
    fn from(e: TryLockError) -> io::Error {
        match e {
            TryLockError::Error(e) => e,
            TryLockError::WouldBlock => io::ErrorKind::WouldBlock.into(),
        }
    }
}
