//! Every directory walk in Jharu starts here.
//!
//! By default jwalk borrows rayon's global thread pool, and if that pool is
//! too busy to start a walk within one second it gives up: the walk yields a
//! single "thread-pool busy" error and ends. Jharu's tabs scan at the same
//! time, so large folders (the user profile, `C:\Windows`) regularly came back
//! as zero bytes, because every caller drops errors with `.flatten()`. Deep
//! clean then reported a 500 GB drive as holding 107 GB.
//!
//! Instead, every walk runs on one pool owned by Jharu with that busy check
//! turned off. Waiting is safe here: jwalk's queues are unbounded, so pool
//! threads never wait on the code reading the results, and Jharu never starts
//! a walk from inside the pool. Concurrent scans now share threads and finish
//! later, rather than silently returning nothing.

use jwalk::rayon::{ThreadPool, ThreadPoolBuilder};
use jwalk::{Parallelism, WalkDir};
use std::path::Path;
use std::sync::{Arc, OnceLock};

fn pool() -> Arc<ThreadPool> {
    static POOL: OnceLock<Arc<ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        Arc::new(
            ThreadPoolBuilder::new()
                .thread_name(|i| format!("jharu-walk-{i}"))
                .build()
                .expect("could not start the directory walk thread pool"),
        )
    })
    .clone()
}

/// A walk of `path` on Jharu's own thread pool. Callers set the remaining
/// options (hidden files, depth, filters) as before.
pub fn walk(path: impl AsRef<Path>) -> WalkDir {
    WalkDir::new(path.as_ref()).parallelism(Parallelism::RayonExistingPool { pool: pool(), busy_timeout: None })
}

/// The same, carrying per-branch state through the walk (see jwalk's
/// `ClientState`), for walks that mark whole subtrees, such as "inside a
/// folder tagged as a cache".
pub fn walk_with<C: jwalk::ClientState>(path: impl AsRef<Path>) -> jwalk::WalkDirGeneric<C> {
    jwalk::WalkDirGeneric::<C>::new(path.as_ref())
        .parallelism(Parallelism::RayonExistingPool { pool: pool(), busy_timeout: None })
}

#[cfg(test)]
mod tests {
    use jwalk::rayon::ThreadPool;
    use std::path::Path;
    use std::time::Duration;

    fn count(walker: jwalk::WalkDir) -> (u64, bool) {
        let mut files = 0;
        let mut busy = false;
        for r in walker.skip_hidden(false) {
            match r {
                Ok(e) if e.file_type().is_file() => files += 1,
                Ok(_) => {}
                Err(e) => busy |= e.is_busy(),
            }
        }
        (files, busy)
    }

    /// Keeps every thread of a pool busy for `hold`, the way another tab's
    /// long scan does.
    fn occupy(spawn: impl Fn(Box<dyn FnOnce() + Send>), threads: usize, hold: Duration) {
        for _ in 0..threads {
            spawn(Box::new(move || std::thread::sleep(hold)));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    fn fixture(root: &Path) -> u64 {
        let _ = std::fs::remove_dir_all(root);
        for d in 0..20 {
            let dir = root.join(format!("d{d}"));
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..10 {
                std::fs::write(dir.join(format!("f{f}")), b"x").unwrap();
            }
        }
        200
    }

    /// A walk that starts while other scans fill the pool. jwalk's default
    /// gives up after one second and reports nothing, which is the bug that
    /// hid C:\Users from Deep clean; Jharu's pool must wait and see it all.
    #[test]
    fn walk_started_on_a_busy_pool_still_sees_everything() {
        let root = std::env::temp_dir().join(format!("jharu walk {}", std::process::id()));
        let expected = fixture(&root);
        let hold = Duration::from_secs(2);

        // The hazard, so this test keeps proving something: on the busy
        // global pool, jwalk's default walk ends with a busy error.
        let global = jwalk::rayon::current_num_threads();
        occupy(|job| jwalk::rayon::spawn(job), global, hold);
        let (files, busy) = count(jwalk::WalkDir::new(&root));
        assert!(busy && files == 0, "expected jwalk's default walk to give up, got {files} files");

        // The fix: same situation on Jharu's own pool.
        let pool: std::sync::Arc<ThreadPool> = super::pool();
        let threads = pool.current_num_threads();
        let p = pool.clone();
        occupy(move |job| p.spawn(job), threads, hold);
        let (files, busy) = count(super::walk(&root));
        assert!(!busy, "a walk gave up because the pool was busy");
        assert_eq!(files, expected);

        let _ = std::fs::remove_dir_all(&root);
    }
}
