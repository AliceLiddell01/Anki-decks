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
