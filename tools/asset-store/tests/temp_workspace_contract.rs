use std::fs;

use asset_store::temp_workspace::TempWorkspace;

#[test]
fn close_and_drop_clean_only_their_owned_workspaces() {
    let first = TempWorkspace::create("temp-workspace-contract").expect("first workspace creates");
    let second =
        TempWorkspace::create("temp-workspace-contract").expect("second workspace creates");
    let first_path = first.path().to_path_buf();
    let second_path = second.path().to_path_buf();

    assert_ne!(first_path, second_path);
    fs::write(first.path().join("first.marker"), b"first")
        .expect("first workspace accepts its marker");
    fs::write(second.path().join("second.marker"), b"second")
        .expect("second workspace accepts its marker");

    first.close().expect("first workspace closes cleanly");
    assert!(!first_path.exists());
    assert!(second_path.join("second.marker").is_file());

    drop(second);
    assert!(!second_path.exists());
}

#[test]
fn selected_root_child_probe() {
    let Some(expected) = std::env::var_os("ANKI_TEST_EXPECTED_TEMP_ROOT") else {
        return;
    };
    use asset_store::temp_workspace::{
        DEFAULT_ORPHAN_MIN_AGE, cleanup_legacy_orphans, cleanup_orphans, plan_orphans,
        selected_temp_root, snapshot,
    };
    let expected = std::path::PathBuf::from(expected);
    if !expected.is_absolute() {
        assert_eq!(
            selected_temp_root().unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            TempWorkspace::create("invalid-root").unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            cleanup_orphans(DEFAULT_ORPHAN_MIN_AGE).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        return;
    }
    assert_eq!(selected_temp_root().unwrap(), expected);
    let workspace = TempWorkspace::create("selected-root-contract").unwrap();
    assert!(workspace.path().starts_with(&expected));
    for report in [
        cleanup_orphans(DEFAULT_ORPHAN_MIN_AGE).unwrap(),
        cleanup_legacy_orphans(DEFAULT_ORPHAN_MIN_AGE).unwrap(),
        plan_orphans(DEFAULT_ORPHAN_MIN_AGE).unwrap(),
    ] {
        assert_eq!(report.root, expected);
        assert!(
            report
                .entries
                .iter()
                .all(|entry| entry.path.starts_with(&expected))
        );
    }
    assert!(
        snapshot()
            .unwrap()
            .paths
            .iter()
            .all(|path| path.starts_with(&expected))
    );
    assert!(
        workspace.path().exists(),
        "активное дерево сохраняется при уборке"
    );
    workspace.close().unwrap();
}

#[test]
fn implicit_gc_and_workspaces_share_default_or_configured_root_without_touching_peers() {
    let owner = TempWorkspace::create("selected-root-parent").unwrap();
    let configured = owner.path().join("configured");
    let peer = owner.path().join("peer");
    fs::create_dir(&configured).unwrap();
    fs::create_dir(&peer).unwrap();
    fs::write(peer.join("keep"), b"independent").unwrap();
    for root in [
        std::path::Path::new("/tmp"),
        configured.as_path(),
        std::path::Path::new("relative"),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "temp_workspace_contract::selected_root_child_probe",
        ]);
        command.env("ANKI_TEST_EXPECTED_TEMP_ROOT", root);
        if root == std::path::Path::new("/tmp") {
            command.env_remove("ASSET_STORE_TEMP_ROOT");
        } else {
            command.env("ASSET_STORE_TEMP_ROOT", root);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        assert_eq!(fs::read(peer.join("keep")).unwrap(), b"independent");
    }
}
