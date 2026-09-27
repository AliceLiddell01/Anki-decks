//! Контракт CLI команды `qa`: коды правил, границы вывода, exit codes.
//!
//! Свойства findings проверяются на синтетических экспортах, а на канонических
//! колодах — только то, что не зависит от текущего содержимого репозитория:
//! форма ответа, детерминизм и согласованность counts с самим выводом.

mod common;

use common::{TempDir, export_with, parse_json, run_cli, words_deck};
use serde_json::{Value, json};

fn qa_json(export: &str, extra: &[&str]) -> (i32, Value) {
    let mut args = vec!["--json", "qa", export];
    args.extend_from_slice(extra);
    let (code, stdout, _) = run_cli(&args);
    (code, parse_json(&stdout))
}

/// Экспорт, в котором есть по одному finding каждого правила.
fn rules_export() -> Value {
    export_with(|value| {
        value["notes"][0]["fields"] = json!([" <span style=\"color: #fff\">x</span> ", "значение"]);
        value["notes"][1]["fields"] = json!(["", "значение"]);
        let mut duplicate = value["notes"][1].clone();
        duplicate["guid"] = json!("guid-3");
        value["notes"]
            .as_array_mut()
            .expect("notes")
            .push(duplicate);
    })
}

#[test]
fn rules_registry_is_listed_with_stable_codes() {
    let temp = TempDir::new("qa-rules");
    temp.write_export(&rules_export());
    let (code, parsed) = qa_json(&temp.path().to_string_lossy(), &[]);
    assert_eq!(code, 0);
    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(parsed["command"], "qa");

    let rules = parsed["result"]["rules"].as_array().expect("rules");
    let codes: Vec<&str> = rules
        .iter()
        .map(|rule| rule["code"].as_str().expect("code"))
        .collect();
    assert_eq!(
        codes,
        vec![
            "empty_field_value",
            "leading_whitespace",
            "trailing_whitespace",
            "forbidden_white_span",
            "duplicate_note_content",
            "duplicate_primary_field"
        ]
    );
    for rule in rules {
        assert!(
            rule["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
        assert!(rule["applicable"].as_bool().is_some());
        assert!(rule["findings"].as_u64().is_some());
    }

    let severities: Vec<&str> = rules
        .iter()
        .map(|rule| rule["severity"].as_str().expect("severity"))
        .collect();
    assert_eq!(
        severities,
        vec!["warning", "warning", "warning", "error", "warning", "info"]
    );
}

#[test]
fn qa_reports_error_severity_findings_without_failing() {
    let temp = TempDir::new("qa-error-severity");
    temp.write_export(&rules_export());
    let (code, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "forbidden_white_span"],
    );

    assert_eq!(code, 0, "QA ERROR не делает команду неуспешной");
    assert_eq!(parsed["result"]["findings_total"], 1);
    let finding = &parsed["result"]["findings"][0];
    assert_eq!(finding["severity"], "error");
    assert_eq!(finding["code"], "forbidden_white_span");
    assert_eq!(finding["note_index"], 0);
    assert_eq!(finding["guid"], "guid-1");
    assert_eq!(finding["field"], "Слово");
    assert_eq!(finding["field_ord"], 0);
    assert_eq!(finding["evidence"]["occurrences"], 1);
    assert!(
        finding["evidence"]["first_tag"]
            .as_str()
            .is_some_and(|tag| tag.contains("color: #fff"))
    );
}

#[test]
fn evidence_stays_bounded_and_never_contains_full_field_value() {
    let long = "я".repeat(2000);
    let temp = TempDir::new("qa-bounded-evidence");
    temp.write_export(&export_with(|value| {
        value["notes"][0]["fields"][0] = json!(format!("{long} "));
    }));
    let (_, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "trailing_whitespace"],
    );

    let finding = &parsed["result"]["findings"][0];
    let sample = finding["evidence"]["sample"].as_str().expect("sample");
    assert!(
        sample.chars().count() <= 121,
        "выборка должна быть ограничена, длина {}",
        sample.chars().count()
    );
    assert!(sample.ends_with('…'));
    assert_eq!(finding["evidence"]["value_chars"], 2001);
    assert_eq!(finding["evidence"]["boundary"]["chars"], 1);
}

#[test]
fn max_per_code_bounds_output_but_not_counts() {
    let temp = TempDir::new("qa-max-per-code");
    temp.write_export(&export_with(|value| {
        let template = value["notes"][0].clone();
        for index in 0..30 {
            let mut copy = template.clone();
            copy["guid"] = json!(format!("bulk-{index}"));
            copy["fields"] = json!(["слово", ""]);
            value["notes"].as_array_mut().expect("notes").push(copy);
        }
    }));

    let (_, parsed) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "empty_field_value", "--max-per-code", "5"],
    );
    assert_eq!(parsed["result"]["findings_total"], 31);
    assert_eq!(parsed["result"]["findings_returned"], 5);
    assert_eq!(parsed["result"]["truncated"], true);
    assert_eq!(parsed["result"]["max_per_code"], 5);
    assert_eq!(parsed["result"]["by_code"][0]["count"], 31);
    assert_eq!(
        parsed["result"]["findings"]
            .as_array()
            .expect("findings")
            .len(),
        5
    );

    let (_, complete) = qa_json(
        &temp.path().to_string_lossy(),
        &["--code", "empty_field_value", "--max-per-code", "200"],
    );
    assert_eq!(complete["result"]["findings_returned"], 31);
    assert_eq!(complete["result"]["truncated"], false);
}

#[test]
fn output_is_deterministic_across_runs() {
    let temp = TempDir::new("qa-determinism");
    temp.write_export(&rules_export());
    let (_, first) = qa_json(&temp.path().to_string_lossy(), &[]);
    let (_, second) = qa_json(&temp.path().to_string_lossy(), &[]);
    assert_eq!(first["result"], second["result"]);
}

#[test]
fn unknown_code_is_a_usage_class_error_with_available_codes() {
    let temp = TempDir::new("qa-unknown-code");
    temp.write_export(&rules_export());
    let (code, parsed) = qa_json(&temp.path().to_string_lossy(), &["--code", "нет_такого"]);

    assert_eq!(code, 3);
    assert_eq!(parsed["error"]["code"], "unknown_qa_code");
    let available = parsed["error"]["details"]["available_codes"]
        .as_array()
        .expect("available_codes");
    assert!(available.iter().any(|code| code == "empty_field_value"));
    assert_eq!(parsed["error"]["details"]["unknown_codes"][0], "нет_такого");
}

#[test]
fn human_mode_lists_every_rule_and_keeps_stdout_only() {
    let temp = TempDir::new("qa-human");
    temp.write_export(&rules_export());
    let (code, stdout, stderr) = run_cli(&["qa", &temp.path().to_string_lossy()]);

    assert_eq!(code, 0);
    assert!(stderr.is_empty());
    for needle in [
        "По кодам:",
        "Findings:",
        "Правила (6):",
        "empty_field_value",
        "duplicate_primary_field",
        "усечено",
    ] {
        assert!(
            stdout.contains(needle),
            "нет фрагмента {needle:?}\n{stdout}"
        );
    }
}

#[test]
fn canonical_decks_report_consistent_counts() {
    for (level, notes) in [(1u8, 718usize), (2, 867), (3, 1507), (4, 982), (5, 1004)] {
        let export = words_deck(level);
        let (code, parsed) = qa_json(&export.to_string_lossy(), &["--max-per-code", "200"]);
        assert_eq!(code, 0, "N{level}");
        assert_eq!(parsed["result"]["notes_total"], notes, "N{level}");

        let by_code = parsed["result"]["by_code"].as_array().expect("by_code");
        let total: u64 = by_code
            .iter()
            .map(|entry| entry["count"].as_u64().expect("count"))
            .sum();
        assert_eq!(
            total as usize, parsed["result"]["findings_total"],
            "N{level}"
        );

        // Реестр правил не зависит от содержимого колоды.
        assert_eq!(
            parsed["result"]["rules"].as_array().expect("rules").len(),
            6,
            "N{level}"
        );

        // Правила, которых в канонических колодах нет, обязаны оставаться нулевыми.
        for code in ["forbidden_white_span", "duplicate_note_content"] {
            let count = by_code
                .iter()
                .find(|entry| entry["code"] == code)
                .map_or(0, |entry| entry["count"].as_u64().expect("count"));
            assert_eq!(count, 0, "N{level}: {code} не должен находиться");
        }
    }
}

#[test]
fn empty_field_findings_point_at_a_real_empty_value() {
    let export = words_deck(1);
    let (code, parsed) = qa_json(&export.to_string_lossy(), &["--code", "empty_field_value"]);
    assert_eq!(code, 0);

    let findings = parsed["result"]["findings"].as_array().expect("findings");
    assert!(!findings.is_empty(), "в N1 есть пустые «Ударение»");

    // Проверяем адрес каждого показанного finding прямо по сырому deck.json.
    let text = std::fs::read_to_string(export.join("deck.json")).expect("deck.json");
    let raw: Value = serde_json::from_str(&text).expect("JSON");
    let notes = raw["notes"].as_array().expect("notes");
    for finding in findings {
        let index = finding["note_index"].as_u64().expect("note_index") as usize;
        let ord = finding["field_ord"].as_u64().expect("field_ord") as usize;
        assert_eq!(finding["field"], "Ударение");
        assert_eq!(finding["severity"], "warning");
        assert_eq!(
            notes[index]["fields"][ord], "",
            "finding должен указывать на пустое значение"
        );
        assert_eq!(finding["evidence"]["value_chars"], 0);
    }
}
