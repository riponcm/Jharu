//! How much disk space files really use.
//!
//! Adding up every file's length overstates usage, and on a developer machine
//! by a lot. Measured on the owner's Windows laptop, C:\Users summed to
//! 462 GB on a drive with 462 GB used in total, because:
//!
//! - Hard links: uv installs packages into virtualenvs as hard links to its
//!   cache, pnpm does the same for node_modules, and Windows keeps its
//!   component store in C:\Windows\WinSxS that way. One file, many paths.
//! - App-container views: Windows shows files that a packaged (MSIX) app such
//!   as Claude wrote at their usual AppData paths as well as inside the
//!   package folder, but only to programs started from that app.
//! - Online-only cloud files: OneDrive and iCloud placeholders report their
//!   full length but occupy no space on this disk.
//! - Sparse and compressed files, such as virtual disks, report a length
//!   larger than what they occupy.
//! - APFS clones (macOS): `cp -c`, Finder's Duplicate and tools such as uv
//!   make a second file that shares the first one's data blocks until either
//!   is changed. Each reports its full size.
//!
//! [`Usage::add_file`] counts each physical file once per measurement, keeps
//! online-only files apart, and uses the allocated size where it differs.
//! [`Seen::linked_outside`] then tells what deleting the measured folder
//! would *not* free: files that stay alive through hard links elsewhere.
//! Clearing a uv cache whose packages are linked into virtualenvs frees only
//! the part no environment uses.

use serde::Serialize;
use std::collections::HashMap;
use std::fs::Metadata;
use std::path::Path;

/// On Windows, a file's identity costs one extra open. A quick measurement
/// (Deep clean, millions of files) checks only files at least this big;
/// smaller duplicates are counted twice, a small error next to the gigabytes
/// the check catches. A thorough one (a single cache) checks every file.
const QUICK_ID_MIN: u64 = 1 << 20;

#[derive(Serialize, Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Usage {
    /// Space these files occupy on this disk, each physical file once.
    pub bytes: u64,
    pub files: u64,
    /// Cloud placeholders: present in the folder, stored only online.
    pub online_only_bytes: u64,
    /// Files already counted at another path in the same measurement (hard
    /// links). Removing this path alone does not free them.
    pub shared_bytes: u64,
    /// Folders and files that could not be read (no rights, or gone during
    /// the walk). What is below them is not counted, so `bytes` is a floor.
    pub unreadable: u64,
}

/// Physical files already counted, by (volume, file id), with how many
/// links the file has in total and how many of them this measurement met.
pub struct Seen {
    files: HashMap<(u64, u64), Links>,
    /// APFS clone families met, by (volume, clone id).
    clones: HashMap<(u64, u64), CloneMet>,
    /// Files smaller than this skip the identity check (Windows only).
    min_id_size: u64,
}

impl Default for Seen {
    fn default() -> Self {
        Seen { files: HashMap::new(), clones: HashMap::new(), min_id_size: QUICK_ID_MIN }
    }
}

struct Links {
    total: u64,
    met: u64,
    bytes: u64,
}

struct CloneMet {
    /// Bytes the first file met shares with its clones.
    shared: u64,
    met: u64,
}

impl Seen {
    /// Checks every file, for exact "what would clearing free" answers on
    /// folders small enough to afford it.
    pub fn thorough() -> Self {
        Seen { files: HashMap::new(), clones: HashMap::new(), min_id_size: 0 }
    }

    /// Bytes of files that have hard links outside everything measured with
    /// this record. Deleting what was measured leaves those files on disk.
    pub fn linked_outside(&self) -> u64 {
        let links: u64 = self.files.values().filter(|l| l.met < l.total).map(|l| l.bytes).sum();
        // A clone met only once shares its data with a file that was not
        // measured. How many clones there are is not known, so this assumes
        // the rest are elsewhere: an underestimate, never an overestimate.
        let clones: u64 = self.clones.values().filter(|c| c.met == 1).map(|c| c.shared).sum();
        links + clones
    }
}

/// What deleting `path` would free: each physical file once, without
/// online-only files or files kept alive by hard links outside `path`.
pub fn reclaimable(path: &Path) -> u64 {
    if path.is_file() {
        let mut usage = Usage::default();
        let mut seen = Seen::thorough();
        if let Ok(meta) = path.symlink_metadata() {
            usage.add_file(path, &meta, &mut seen);
        }
        return usage.bytes.saturating_sub(seen.linked_outside());
    }
    let mut usage = Usage::default();
    let mut seen = Seen::thorough();
    for entry in crate::walk::walk(path).skip_hidden(false).into_iter().flatten() {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                usage.add_file(&entry.path(), &meta, &mut seen);
            }
        }
    }
    usage.bytes.saturating_sub(seen.linked_outside())
}

impl Usage {
    /// Counts one regular file found at `path`.
    pub fn add_file(&mut self, path: &Path, meta: &Metadata, seen: &mut Seen) {
        let len = meta.len();
        if online_only(meta) {
            self.online_only_bytes += len;
            return;
        }
        let (id, links, allocated) = identity(path, meta, seen.min_id_size);
        if let Some(id) = id {
            match seen.files.get_mut(&id) {
                Some(known) => {
                    known.met += 1;
                    self.shared_bytes += allocated;
                    return;
                }
                None => {
                    // A container view shows one link at two paths, so the
                    // count can exceed the links; that is not "outside".
                    seen.files.insert(id, Links { total: links, met: 1, bytes: allocated });
                }
            }
        } else if allocated >= seen.min_id_size.max(1) {
            if let Some(c) = clone_info(path) {
                let shared = allocated.saturating_sub(c.private_bytes);
                if shared > 0 {
                    match seen.clones.get_mut(&c.key) {
                        // Another clone of data already counted: only what
                        // this file holds alone is new space.
                        Some(known) => {
                            known.met += 1;
                            self.shared_bytes += shared;
                            self.bytes += allocated - shared;
                            self.files += 1;
                            return;
                        }
                        None => {
                            seen.clones.insert(c.key, CloneMet { shared, met: 1 });
                        }
                    }
                }
            }
        }
        self.bytes += allocated;
        self.files += 1;
    }
}

#[cfg(target_os = "windows")]
fn online_only(meta: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x40000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x400000;
    meta.file_attributes() & (FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS) != 0
}

#[cfg(target_os = "macos")]
fn online_only(meta: &Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    // iCloud Drive's "dataless" files: listed, but evicted to the cloud.
    const SF_DATALESS: u32 = 0x4000_0000;
    meta.st_flags() & SF_DATALESS != 0
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn online_only(_meta: &Metadata) -> bool {
    false
}

/// The file's identity when it may be one of several paths to the same file,
/// its number of hard links, and the space it occupies.
#[cfg(target_os = "windows")]
fn identity(path: &Path, meta: &Metadata, min_id_size: u64) -> (Option<(u64, u64)>, u64, u64) {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetCompressedFileSizeW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_SPARSE_FILE, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let len = meta.len();
    if len < min_id_size {
        return (None, 1, len);
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut allocated = len;
    if meta.file_attributes() & (FILE_ATTRIBUTE_SPARSE_FILE | FILE_ATTRIBUTE_COMPRESSED) != 0 {
        let mut high = 0u32;
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
        if low != u32::MAX {
            allocated = ((high as u64) << 32) | low as u64;
        }
    }
    // Opening with no access rights reads metadata only: it never downloads
    // a cloud file or touches its contents.
    let mut links = 1u64;
    let id = unsafe {
        let h = CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            None
        } else {
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            let ok = GetFileInformationByHandle(h, &mut info);
            CloseHandle(h);
            if ok != 0 {
                links = info.nNumberOfLinks.max(1) as u64;
            }
            (ok != 0).then(|| {
                (info.dwVolumeSerialNumber as u64, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
            })
        }
    };
    (id, links, allocated)
}

#[cfg(unix)]
fn identity(_path: &Path, meta: &Metadata, _min_id_size: u64) -> (Option<(u64, u64)>, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    // Only hard-linked files can be met twice, and the link count is free.
    let id = (meta.nlink() > 1).then(|| (meta.dev(), meta.ino()));
    // Sparse files occupy fewer blocks than their length says.
    let allocated = (meta.blocks() * 512).min(meta.len());
    (id, meta.nlink().max(1), if allocated > 0 { allocated } else { meta.len() })
}

/// What the duplicate finder needs to know about one file, from metadata only
/// (nothing is read, so cloud placeholders are never downloaded).
pub struct FileInfo {
    /// (volume, file id) when the file may share its data with other paths.
    /// On Unix only hard-linked files have one.
    pub id: Option<(u64, u64)>,
    /// (volume, clone id) of an APFS clone that holds no data of its own:
    /// every block is shared with another file of the same clone id.
    pub clone: Option<(u64, u64)>,
    /// How many paths lead to this file in total, inside or outside any scan.
    pub links: u64,
    /// A cloud placeholder: listed here, stored only online.
    pub online_only: bool,
}

pub fn file_info(path: &Path, meta: &Metadata) -> FileInfo {
    if online_only(meta) {
        return FileInfo { id: None, clone: None, links: 1, online_only: true };
    }
    let (id, links, _) = identity(path, meta, 0);
    let clone = if id.is_none() { clone_info(path).filter(|c| c.private_bytes == 0).map(|c| c.key) } else { None };
    FileInfo { id, clone, links, online_only: false }
}

/// How an APFS file shares its data with clones of it.
pub struct CloneInfo {
    /// (volume, clone id). Files made by cloning one another share an id
    /// until one of them is changed.
    pub key: (u64, u64),
    /// Bytes only this file holds; 0 for an untouched clone.
    pub private_bytes: u64,
}

/// Reads the clone id and private size from metadata (no contents read).
/// `None` off APFS, for files that are not clones, and on other systems.
#[cfg(target_os = "macos")]
pub fn clone_info(path: &Path) -> Option<CloneInfo> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    // Attributes come back packed in bit order: private size, then clone id.
    #[repr(C)]
    struct Reply {
        len: u32,
        returned: libc::attribute_set_t,
        private: i64,
        clone_id: u64,
    }
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut req: libc::attrlist = unsafe { std::mem::zeroed() };
    req.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    req.commonattr = libc::ATTR_CMN_RETURNED_ATTRS;
    let want = libc::ATTR_CMNEXT_PRIVATESIZE | libc::ATTR_CMNEXT_CLONEID;
    req.forkattr = want;
    let mut reply: Reply = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            &mut req as *mut _ as *mut libc::c_void,
            &mut reply as *mut _ as *mut libc::c_void,
            std::mem::size_of::<Reply>(),
            libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    // A file system without clones does not return the attributes.
    if r != 0 || reply.returned.forkattr & want != want || reply.clone_id == 0 {
        return None;
    }
    let dev = path.symlink_metadata().ok()?.dev();
    Some(CloneInfo { key: (dev, reply.clone_id), private_bytes: reply.private.max(0) as u64 })
}

#[cfg(not(target_os = "macos"))]
pub fn clone_info(_path: &Path) -> Option<CloneInfo> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cp -c` makes an APFS clone: two files, one copy of the data.
    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_clones_count_once() {
        let root = std::env::temp_dir().join(format!("jharu clones {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let big = vec![3u8; 2 << 20];
        let (a, b, plain) = (root.join("a.bin"), root.join("b.bin"), root.join("plain.bin"));
        std::fs::write(&a, &big).unwrap();
        std::fs::write(&plain, &big).unwrap();
        let cloned = std::process::Command::new("/bin/cp").arg("-c").arg(&a).arg(&b).status().map(|s| s.success()).unwrap_or(false);
        if !cloned || clone_info(&a).is_none() {
            eprintln!("temp folder is not on APFS; skipped");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let ia = file_info(&a, &a.symlink_metadata().unwrap());
        let ib = file_info(&b, &b.symlink_metadata().unwrap());
        assert!(ia.clone.is_some() && ia.clone == ib.clone, "the two clones share an id");
        assert!(file_info(&plain, &plain.symlink_metadata().unwrap()).clone.is_none(), "a plain copy is no clone");

        let size = big.len() as u64;
        let mut seen = Seen::default();
        let mut u = Usage::default();
        for p in [&a, &b, &plain] {
            u.add_file(p, &p.symlink_metadata().unwrap(), &mut seen);
        }
        assert_eq!(u.bytes, 2 * size, "the clone pair once, the plain copy once");
        assert_eq!(u.shared_bytes, size);
        assert_eq!(seen.linked_outside(), 0);
        // One clone alone frees nothing while the other remains.
        assert_eq!(reclaimable(&b), 0);
        std::fs::remove_file(&a).unwrap();
        assert_eq!(reclaimable(&b), size, "the last clone owns the data");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hard_links_count_once() {
        let root = std::env::temp_dir().join(format!("jharu usage {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("cache")).unwrap();
        std::fs::create_dir_all(root.join("venv")).unwrap();
        // Big enough for the Windows identity check.
        let big = vec![7u8; 2 << 20];
        std::fs::write(root.join("cache").join("torch.bin"), &big).unwrap();
        std::fs::hard_link(root.join("cache").join("torch.bin"), root.join("venv").join("torch.bin")).unwrap();
        std::fs::write(root.join("venv").join("own.bin"), &big).unwrap();

        let mut seen = Seen::default();
        let measure = |dir: &str, seen: &mut Seen| {
            let mut u = Usage::default();
            for e in std::fs::read_dir(root.join(dir)).unwrap().flatten() {
                u.add_file(&e.path(), &e.metadata().unwrap(), seen);
            }
            u
        };
        let cache = measure("cache", &mut seen);
        let venv = measure("venv", &mut seen);
        let size = big.len() as u64;
        assert_eq!(cache.bytes, size);
        assert_eq!(venv.bytes, size, "only the venv's own file is new space");
        assert_eq!(venv.shared_bytes, size, "the hard link is reported as shared");
        assert_eq!(cache.bytes + venv.bytes, 2 * size, "two physical files in total");
        // Both folders measured together: every link was met, so deleting
        // both frees everything.
        assert_eq!(seen.linked_outside(), 0);

        // The cache alone: its torch.bin is still used by the venv, so
        // deleting the cache frees nothing of it.
        assert_eq!(reclaimable(&root.join("cache")), 0, "the venv keeps the cache's file alive");
        // The venv alone: own.bin is freed, the linked torch.bin is not.
        assert_eq!(reclaimable(&root.join("venv")), size);
        // A link whose both paths sit inside one folder is freed with it.
        std::fs::hard_link(root.join("venv").join("own.bin"), root.join("venv").join("own copy.bin")).unwrap();
        assert_eq!(reclaimable(&root.join("venv")), size);
        let _ = std::fs::remove_dir_all(&root);
    }
}
