use std::collections::HashSet;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::config::WatchOptions;
use crate::debounce::Debounce;
use crate::ignore::Ignore;

fn qualifies(kind: EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(kind) => !matches!(kind, ModifyKind::Metadata(_)),
        EventKind::Access(_) | EventKind::Any | EventKind::Other => false,
    }
}

struct DirectoryWatches {
    watcher: RecommendedWatcher,
    roots: Vec<PathBuf>,
    ignore: Ignore,
    dirs: HashSet<PathBuf>,
    events: mpsc::WeakUnboundedSender<Event>,
}

// A directory symlink should never cause a new watch outside the tree or a walk in circles.
fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|entry| entry.is_dir())
}

impl DirectoryWatches {
    fn new(
        cwd: &Path,
        options: WatchOptions,
        send: mpsc::UnboundedSender<Event>,
    ) -> Result<Self, String> {
        let ignore = Ignore::new(&options.ignore).map_err(|e| e.to_string())?;
        let events = send.downgrade();
        let watcher =
            notify::recommended_watcher(move |result: notify::Result<Event>| match result {
                Ok(event) if qualifies(event.kind) => {
                    let _ = send.send(event);
                }
                Ok(_) => {}
                Err(err) => eprintln!("watch: file notification failed: {err}"),
            })
            .map_err(|e| e.to_string())?;
        let mut watches = Self {
            watcher,
            roots: Vec::new(),
            ignore,
            dirs: HashSet::new(),
            events,
        };
        for path in options.paths {
            let path = cwd.join(path);
            if !is_dir(&path) {
                eprintln!("watch: {} is not a directory", path.display());
                continue;
            }
            match path.canonicalize() {
                Ok(root) if !watches.roots.contains(&root) => {
                    watches.roots.push(root);
                }
                Ok(_) => {}
                Err(err) => eprintln!("watch: cannot resolve {}: {err}", path.display()),
            }
        }
        for root in watches.roots.clone() {
            watches.add_tree(&root);
        }
        Ok(watches)
    }

    fn add_tree(&mut self, path: &Path) {
        let mut pending = vec![path.to_path_buf()];
        while let Some(dir) = pending.pop() {
            // The async task can be cancelled while this walk is on the blocking pool.
            if self.events.upgrade().map_or(true, |send| send.is_closed()) {
                return;
            }
            if self.is_ignored(&dir) || !is_dir(&dir) {
                continue;
            }
            if !self.dirs.contains(&dir) {
                if let Err(err) = self.watcher.watch(&dir, RecursiveMode::NonRecursive) {
                    eprintln!("watch: cannot watch {}: {err}", dir.display());
                    continue;
                }
                self.dirs.insert(dir.clone());
            }
            match fs::read_dir(&dir) {
                Ok(entries) => pending.extend(entries.filter_map(|entry| {
                    let entry = entry.ok()?;
                    entry.file_type().ok()?.is_dir().then(|| entry.path())
                })),
                Err(err) => eprintln!("watch: cannot list {}: {err}", dir.display()),
            }
        }
    }

    fn is_ignored(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| {
            path.ancestors()
                .take_while(|ancestor| ancestor.starts_with(root))
                .any(|ancestor| self.ignore.matches(root, ancestor))
        })
    }

    fn paths_to_watch(&self, paths: Vec<PathBuf>) -> Vec<PathBuf> {
        paths
            .into_iter()
            .filter(|path| {
                self.roots.iter().any(|root| path.starts_with(root)) && !self.is_ignored(path)
            })
            .collect()
    }

    fn update_dirs(&mut self, paths: &[PathBuf]) {
        for path in paths {
            if is_dir(path) {
                self.add_tree(path);
            } else if self.dirs.contains(path) {
                self.dirs.retain(|dir| {
                    if dir.starts_with(path) {
                        // The backend may already have removed this watch with the directory.
                        let _ = self.watcher.unwatch(dir);
                        false
                    } else {
                        true
                    }
                });
            }
        }
    }
}

pub async fn run<F: Future<Output = ()>>(
    cwd: PathBuf,
    options: WatchOptions,
    mut restart: impl FnMut() -> F,
) -> Result<(), String> {
    let mut debounce = Debounce::new(options.debounce());
    let (send, mut events) = mpsc::unbounded_channel();
    let mut watches =
        tokio::task::spawn_blocking(move || DirectoryWatches::new(&cwd, options, send))
            .await
            .map_err(|e| e.to_string())??;
    if watches.dirs.is_empty() {
        return Ok(());
    }
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { return Ok(()) };
                let paths = watches.paths_to_watch(event.paths);
                if !paths.is_empty() && matches!(event.kind, EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))) {
                    let (updated, paths) = tokio::task::spawn_blocking(move || {
                        watches.update_dirs(&paths);
                        (watches, paths)
                    }).await.map_err(|e| e.to_string())?;
                    watches = updated;
                    debounce.record(paths);
                } else {
                    debounce.record(paths);
                }
            }
            _ = debounce.ready() => restart().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, RemoveKind};

    #[cfg(target_os = "linux")]
    #[test]
    fn non_recursive_walk_skips_ignored_trees() {
        use std::os::unix::fs::MetadataExt;

        let home = tempfile::tempdir().unwrap();
        let dir = home.path().canonicalize().unwrap();
        fs::create_dir_all(dir.join("node_modules/a/b")).unwrap();
        fs::create_dir_all(dir.join("src/sub")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&dir, dir.join("src/cycle")).unwrap();
        let (send, _events) = mpsc::unbounded_channel();
        let watches = DirectoryWatches::new(&dir, WatchOptions::default(), send).unwrap();
        assert_eq!(
            watches.dirs,
            [dir.clone(), dir.join("src"), dir.join("src/sub")]
                .into_iter()
                .collect()
        );
        let ignored = fs::metadata(dir.join("node_modules/a/b")).unwrap();
        let inode = format!("ino:{:x}", ignored.ino());
        for info in fs::read_dir("/proc/self/fdinfo").unwrap().flatten() {
            if let Ok(info) = fs::read_to_string(info.path()) {
                assert!(
                    !info.lines().any(|line| line.starts_with("inotify ")
                        && line.split_whitespace().any(|field| field == inode)),
                    "ignored tree has a kernel watch"
                );
            }
        }
    }

    #[test]
    fn overlapping_roots_do_not_reinclude_ignored_paths() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().canonicalize().unwrap();
        let ignored = dir.join("src/generated");
        let source = dir.join("src/main.rs");
        fs::create_dir_all(ignored.join("nested")).unwrap();
        for paths in [["src/generated", "src", "."], [".", "src", "src/generated"]] {
            let mut options = WatchOptions::default();
            options.paths = paths.map(PathBuf::from).into();
            options.ignore = vec!["src/generated".into()];
            let (send, _events) = mpsc::unbounded_channel();
            let mut watches = DirectoryWatches::new(&dir, options, send).unwrap();
            let ignored_file = ignored.join("nested/file.rs");
            assert_eq!(
                watches.paths_to_watch(vec![ignored_file, source.clone()]),
                std::slice::from_ref(&source)
            );
            assert!(watches.dirs.contains(&dir.join("src")));
            assert!(!watches.dirs.iter().any(|path| path.starts_with(&ignored)));
            let new_dir = ignored.join("new");
            fs::create_dir_all(&new_dir).unwrap();
            watches.update_dirs(&[new_dir]);
            assert!(!watches.dirs.iter().any(|path| path.starts_with(&ignored)));
        }
    }

    #[test]
    fn access_and_open_events_are_dropped() {
        assert!(!qualifies(EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
        assert!(!qualifies(EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(!qualifies(EventKind::Any));
        assert!(!qualifies(EventKind::Other));
        assert!(qualifies(EventKind::Create(CreateKind::File)));
        assert!(qualifies(EventKind::Modify(ModifyKind::Data(
            DataChange::Content
        ))));
        assert!(qualifies(EventKind::Remove(RemoveKind::File)));
    }
}
