use crate::document::Document;
use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dark: bool,
    pub vim: bool,
    pub font_size: f32,
    pub tab_width: usize,
    pub insert_spaces: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            dark: true,
            vim: false,
            font_size: 15.0,
            tab_width: 4,
            insert_spaces: true,
        }
    }
}
pub fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("twill")
}
impl Settings {
    pub fn load() -> (Self, Option<String>) {
        let path = data_dir().join("settings.toml");
        match fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<Self>(&s) {
                Ok(mut settings) => {
                    settings.font_size = settings.font_size.clamp(10.0, 32.0);
                    settings.tab_width = settings.tab_width.clamp(1, 16);
                    (settings, None)
                }
                Err(e) => (
                    Self::default(),
                    Some(format!("Settings could not be read: {e}")),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Self::default(), None),
            Err(e) => (Self::default(), Some(e.to_string())),
        }
    }
    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(data_dir())?;
        fs::write(
            data_dir().join("settings.toml"),
            toml::to_string_pretty(self)?,
        )?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct Recovery {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub text: String,
    #[serde(default)]
    pub bom: bool,
    #[serde(default = "default_newline")]
    pub newline: String,
}
fn default_newline() -> String {
    "\n".into()
}
pub struct RecoveryStore {
    pub directory: PathBuf,
    _session_lock: Option<File>,
}
impl RecoveryStore {
    pub fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |t| t.as_nanos());
        let directory = data_dir()
            .join("recovery")
            .join(format!("{}-{nonce}", std::process::id()));
        let _ = fs::create_dir_all(&directory);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join(".session.lock"))
            .ok()
            .and_then(|file| file.try_lock().ok().map(|_| file));
        Self {
            directory,
            _session_lock: lock,
        }
    }
    #[cfg(test)]
    pub fn write(&self, id: u64, path: Option<PathBuf>, text: String) -> Result<()> {
        self.write_recovery(Recovery {
            id,
            path,
            text,
            bom: false,
            newline: default_newline(),
        })
    }
    pub fn write_document(&self, doc: &Document) -> Result<()> {
        self.write_recovery(Recovery {
            id: doc.id,
            path: doc.path.clone(),
            text: doc.text(),
            bom: doc.has_bom(),
            newline: doc.preferred_newline().to_owned(),
        })
    }
    fn write_recovery(&self, recovery: Recovery) -> Result<()> {
        fs::create_dir_all(&self.directory)?;
        let destination = self.directory.join(format!("{}.json", recovery.id));
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let temporary = self
            .directory
            .join(format!(".{}.{}.tmp", recovery.id, nonce));
        fs::write(&temporary, serde_json::to_vec(&recovery)?)?;
        if let Err(error) = fs::rename(&temporary, &destination) {
            let _ = fs::remove_file(&temporary);
            return Err(error)
                .with_context(|| format!("replacing recovery snapshot {}", destination.display()));
        }
        Ok(())
    }
    pub fn remove(&self, id: u64) {
        let _ = fs::remove_file(self.directory.join(format!("{id}.json")));
    }
    pub fn pending(&self) -> Vec<(PathBuf, Recovery)> {
        self.pending_from(&data_dir().join("recovery"))
    }
    fn pending_from(&self, root: &Path) -> Vec<(PathBuf, Recovery)> {
        let mut result = Vec::new();
        if let Ok(dirs) = fs::read_dir(root) {
            for dir in dirs.flatten() {
                if dir.path() == self.directory {
                    continue;
                }
                let lock_path = dir.path().join(".session.lock");
                if lock_path.exists() {
                    let Ok(lock_file) = OpenOptions::new().read(true).write(true).open(lock_path)
                    else {
                        continue;
                    };
                    if lock_file.try_lock().is_err() {
                        continue;
                    }
                    let _ = lock_file.unlock();
                }
                if let Ok(files) = fs::read_dir(dir.path()) {
                    for file in files.flatten() {
                        if file.path().extension().and_then(|x| x.to_str()) != Some("json") {
                            continue;
                        }
                        if let Ok(bytes) = fs::read(file.path()) {
                            if let Ok(r) = serde_json::from_slice(&bytes) {
                                result.push((file.path(), r));
                            }
                        }
                    }
                }
            }
        }
        result
    }
}

pub struct FileWatcher {
    watcher: Option<RecommendedWatcher>,
    receiver: Receiver<notify::Result<notify::Event>>,
    parents: HashSet<PathBuf>,
}
impl FileWatcher {
    pub fn new() -> Self {
        let (tx, receiver) = mpsc::channel();
        let watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })
        .ok();
        Self {
            watcher,
            receiver,
            parents: HashSet::new(),
        }
    }
    pub fn watch(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            if self.parents.insert(parent.to_path_buf()) {
                if let Some(w) = self.watcher.as_mut() {
                    let _ = w.watch(parent, RecursiveMode::NonRecursive);
                }
            }
        }
    }
    pub fn changed(&self) -> bool {
        let mut changed = false;
        while self.receiver.try_recv().is_ok() {
            changed = true;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_roundtrip() {
        let original = Settings::default();
        let parsed: Settings = toml::from_str(&toml::to_string(&original).unwrap()).unwrap();
        assert_eq!(parsed.tab_width, 4);
        assert_eq!(parsed.font_size, 15.0);
    }
    #[test]
    fn recovery_skips_live_sessions_and_corrupt_snapshots() {
        let root = std::env::temp_dir().join(format!(
            "twill-recovery-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let live = root.join("live");
        let crashed = root.join("crashed");
        fs::create_dir_all(&live).unwrap();
        fs::create_dir_all(&crashed).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(live.join(".session.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let live_store = RecoveryStore {
            directory: live.clone(),
            _session_lock: Some(lock),
        };
        live_store.write(1, None, "live text".into()).unwrap();
        let crashed_store = RecoveryStore {
            directory: crashed.clone(),
            _session_lock: None,
        };
        crashed_store.write(2, None, "saved text".into()).unwrap();
        crashed_store
            .write(2, None, "saved text again".into())
            .unwrap();
        fs::write(crashed.join("broken.json"), "{broken").unwrap();
        let observer = RecoveryStore {
            directory: root.join("observer"),
            _session_lock: None,
        };
        let pending = observer.pending_from(&root);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.text, "saved text again");
        drop(live_store);
        assert_eq!(observer.pending_from(&root).len(), 2);
        let _ = fs::remove_dir_all(root);
    }
    #[test]
    fn recovery_metadata_defaults_for_old_snapshots() {
        let old = br#"{"id":1,"path":null,"text":"hello"}"#;
        let recovery: Recovery = serde_json::from_slice(old).unwrap();
        assert!(!recovery.bom);
        assert_eq!(recovery.newline, "\n");
    }
}
