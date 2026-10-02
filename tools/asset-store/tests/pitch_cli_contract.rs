use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use asset_store::{AssetStore, PitchAccentDomainPolicy, StoreOptions};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "pitch-cli-contract-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        AssetStore::open_with_policy(
            StoreOptions::new(path.join("store")),
            PitchAccentDomainPolicy,
        )
        .unwrap();
        Self(path)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pitch-assets"))
            .current_dir(&self.0)
            .arg("--repository-root")
            .arg(&self.0)
            .arg("--store")
            .arg(self.0.join("store"))
            .args(args)
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn binary_sends_success_to_stdout_and_human_errors_to_stderr() {
    let fixture = Fixture::new();
    let success = fixture.run(&["corpus", "list"]);
    assert!(success.status.success());
    assert!(!success.stdout.is_empty());
    assert!(success.stderr.is_empty());

    for operation in ["status", "run"] {
        let human = fixture.run(&["batch", operation, "--batch-id", "missing"]);
        assert!(!human.status.success());
        assert!(human.stdout.is_empty());
        assert!(String::from_utf8_lossy(&human.stderr).contains("не найдено"));

        let json = fixture.run(&[
            "--output",
            "json",
            "batch",
            operation,
            "--batch-id",
            "missing",
        ]);
        assert_eq!(json.status.code(), human.status.code());
        assert!(json.stderr.is_empty());
        let response: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        assert_eq!(response["outcome"], "failed");
        assert_eq!(response["changed"], false);
        assert!(response["error"]["code"].is_string());
    }
}
