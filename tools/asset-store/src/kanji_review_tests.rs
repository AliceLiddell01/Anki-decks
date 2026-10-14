use super::*;

fn snapshot() -> ReviewSnapshot {
    ReviewSnapshot {
        schema_version: SCHEMA,
        batch_id: "review-test".into(),
        validator: crate::kanji_validator::KanjiImageValidator::validator_identity(),
        rows: ['保', '倉', '点', '門']
            .into_iter()
            .map(|character| ReviewRow {
                identity: AssetIdentity::new("kanji", character.to_string()).unwrap(),
                generation: 3,
                attempts: 7,
                sha256: "a".repeat(64),
                candidate_path: format!("candidates/{}.gif", "a".repeat(64)),
                filename: format!("{character}-{}.gif", "a".repeat(64)),
            })
            .collect(),
    }
}
fn text(snapshot: &ReviewSnapshot, actions: &[&str]) -> String {
    let mut text = format!(
        "# комментарий\n@review\t1\t{}\t{}\n",
        snapshot.batch_id,
        "b".repeat(64)
    );
    for (row, action) in snapshot.rows.iter().zip(actions) {
        text.push_str(&format!("{action}\t{}\n", row.columns()));
    }
    text
}
#[test]
fn only_explicit_y_confirms_and_windows_text_is_accepted() {
    let snapshot = snapshot();
    let text = format!(
        "\u{feff}{}",
        text(&snapshot, &["y", "e", "侏", "-"]).replace('\n', "\r\n")
    );
    let parsed = parse(&snapshot, &"b".repeat(64), &text).unwrap();
    assert_eq!(
        parsed[0].as_ref().unwrap().action,
        HumanBatchAction::Confirm
    );
    assert_eq!(
        parsed[1].as_ref().unwrap().action,
        HumanBatchAction::RejectAndHold
    );
    assert!(
        parsed[2]
            .as_ref()
            .unwrap()
            .reason
            .contains("actual_character=侏")
    );
    assert!(parsed[3].is_none());
    let parsed = parse(&snapshot, &"b".repeat(64), &text.replace("y\t", "\t")).unwrap();
    assert!(parsed[0].is_none());
}
#[test]
fn edited_metadata_duplicates_paths_and_unknown_decisions_reject_entire_file() {
    let snapshot = snapshot();
    let original = text(&snapshot, &["y", "-", "-", "-"]);
    let second = original
        .lines()
        .find(|line| line.starts_with("-\t倉"))
        .unwrap();
    let corruptions = [
        original.replacen("@review\t1", "@review\t2", 1),
        original.replacen("review-test", "other-batch", 1),
        original.replace("\t3\t7\t", "\t4\t7\t"),
        original.replace("\t3\t7\t", "\t3\t8\t"),
        original.replacen(&"a".repeat(64), &"c".repeat(64), 1),
        original.replace(
            second,
            original
                .lines()
                .find(|line| line.starts_with("y\t保"))
                .unwrap(),
        ),
        original.replacen("candidates/", "../", 1),
        original.replace("y\t", "yes\t"),
        original.replace("y\t", "保\t"),
        original.replace("y\t", "🙂\t"),
        original.replace("y\t", "侏保\t"),
        format!("{original}{second}\n"),
        original.lines().take(4).collect::<Vec<_>>().join("\n"),
    ];
    for text in corruptions {
        assert!(parse(&snapshot, &"b".repeat(64), &text).is_err(), "{text}");
    }
}
