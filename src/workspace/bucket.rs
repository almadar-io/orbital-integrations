//! The object store a production host keeps its workspaces in (Firebase Storage in the TS host).

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::error::IntegrationError;

pub trait WorkspaceBucket: Send + Sync {
    /// Every object name under `prefix`.
    fn list(&self, prefix: &str) -> Result<Vec<String>, IntegrationError>;
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, IntegrationError>;
    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), IntegrationError>;
    fn remove(&self, name: &str) -> Result<(), IntegrationError>;
}

/// An in-memory bucket (tests, replays, hosts without an object store).
#[derive(Default)]
pub struct MemoryBucket {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryBucket {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every object, name-ordered.
    pub fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
        self.objects.lock().expect("bucket lock").clone()
    }
}

impl WorkspaceBucket for MemoryBucket {
    fn list(&self, prefix: &str) -> Result<Vec<String>, IntegrationError> {
        Ok(self
            .objects
            .lock()
            .expect("bucket lock")
            .keys()
            .filter(|name| name.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, IntegrationError> {
        Ok(self.objects.lock().expect("bucket lock").get(name).cloned())
    }

    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), IntegrationError> {
        self.objects
            .lock()
            .expect("bucket lock")
            .insert(name.to_string(), bytes.to_vec());
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), IntegrationError> {
        self.objects.lock().expect("bucket lock").remove(name);
        Ok(())
    }
}
