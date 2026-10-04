use std::process::{Command, Output};

use asset_store::temp_workspace::TempWorkspace;
use asset_store::{AssetStore, PitchAccentDomainPolicy, StoreOptions};

struct Fixture {
    workspace: TempWorkspace,
}

impl Fixture {
    fn new() -> Self {
        let workspace =
            TempWorkspace::create("pitch-cli-contract").expect("fixture root создаётся");
        AssetStore::open_with_policy(
            StoreOptions::new(workspace.path().join("store")),
            PitchAccentDomainPolicy,
        )
        .unwrap();
        Self { workspace }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pitch-assets"))
            .current_dir(self.workspace.path())
            .arg("--repository-root")
            .arg(self.workspace.path())
            .arg("--store")
            .arg(self.workspace.path().join("store"))
            .args(args)
            .output()
            .unwrap()
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
