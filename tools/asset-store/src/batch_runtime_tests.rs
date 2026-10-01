use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::batch::{RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime};
use crate::error::{AssetError, ErrorCode};

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "asset-store-generic-batch-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlainTextBatch {
    schema_version: u32,
    batch_id: String,
    revision: u64,
    description: String,
    blob: Option<RuntimeBlobRef>,
}

impl RuntimeBatchState for PlainTextBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != 1 || self.description.trim().is_empty() {
            return Err(AssetError::new(
                ErrorCode::InvalidTransition,
                "invalid plain-text fixture state",
            ));
        }
        Ok(())
    }

    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        self.blob.iter().cloned().collect()
    }
}

#[test]
fn generic_runtime_resumes_non_image_state_and_exact_blob_bytes() {
    let temporary = TemporaryDirectory::new();
    let bytes = b"plain text fixture bytes, not an image";
    let state = {
        let mut runtime = SafeBatchRuntime::open(&temporary.0, "plain-text").unwrap();
        assert!(runtime.load::<PlainTextBatch>().unwrap().is_none());
        let blob = runtime.persist_blob(bytes, "bin").unwrap();
        let state = PlainTextBatch {
            schema_version: 1,
            batch_id: "plain-text".into(),
            revision: 1,
            description: "domain-owned state with no image semantics".into(),
            blob: Some(blob),
        };
        runtime.save(&state).unwrap();
        state
    };

    let mut runtime = SafeBatchRuntime::open(&temporary.0, "plain-text").unwrap();
    let resumed = runtime.load::<PlainTextBatch>().unwrap().unwrap();
    assert_eq!(resumed, state);
    assert_eq!(
        runtime.read_blob(resumed.blob.as_ref().unwrap()).unwrap(),
        bytes
    );
}
