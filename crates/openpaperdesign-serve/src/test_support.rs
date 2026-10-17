//! Bounded deterministic scheduling points, compiled only into library tests.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Duration;

type Key = (PathBuf, &'static str);
type Hook = (tokio::sync::oneshot::Sender<()>, Option<mpsc::Receiver<()>>);
fn hooks() -> &'static Mutex<HashMap<Key, Hook>> {
    static HOOKS: OnceLock<Mutex<HashMap<Key, Hook>>> = OnceLock::new();
    HOOKS.get_or_init(Default::default)
}

pub struct Point {
    key: Key,
    reached: Option<tokio::sync::oneshot::Receiver<()>>,
    resume: mpsc::Sender<()>,
}

impl Point {
    pub fn arm(dir: &Path, point: &'static str, pause: bool) -> Self {
        let key = (dir.to_path_buf(), point);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (resume, wait) = mpsc::channel();
        assert!(
            hooks()
                .lock()
                .unwrap()
                .insert(key.clone(), (tx, pause.then_some(wait)))
                .is_none()
        );
        Self {
            key,
            reached: Some(rx),
            resume,
        }
    }

    pub async fn reached(&mut self) {
        tokio::time::timeout(Duration::from_secs(15), self.reached.take().unwrap())
            .await
            .expect("test scheduling point was not reached")
            .unwrap();
    }

    pub fn resume(&self) {
        let _ = self.resume.send(());
    }
}

impl Drop for Point {
    fn drop(&mut self) {
        hooks().lock().unwrap().remove(&self.key);
        self.resume();
    }
}

pub fn hit(dir: &Path, point: &'static str) {
    let hook = hooks().lock().unwrap().remove(&(dir.to_path_buf(), point));
    if let Some((reached, wait)) = hook {
        let _ = reached.send(());
        if let Some(wait) = wait {
            wait.recv_timeout(Duration::from_secs(15))
                .expect("test pause was not released");
        }
    }
}

/// A one-shot injected failure: true (and the arming `Point` sees it reached) only when a test
/// armed `point` for `dir`.
pub fn fault(dir: &Path, point: &'static str) -> bool {
    let hook = hooks().lock().unwrap().remove(&(dir.to_path_buf(), point));
    match hook {
        Some((reached, _)) => {
            let _ = reached.send(());
            true
        }
        None => false,
    }
}
