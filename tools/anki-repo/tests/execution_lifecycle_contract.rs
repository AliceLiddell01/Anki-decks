//! Жизненный цикл выполнения CLI на независимых синтетических Git-репозиториях.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::common::{TempDir, cli_binary, parse_json, run_cli_in};
use serde_json::Value;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Git: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn success(output: (i32, String, String)) -> Value {
    let (code, stdout, stderr) = output;
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    parse_json(&stdout)["result"].clone()
}

struct Fixture {
    temp: TempDir,
    pack: PathBuf,
    head: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new("execution-lifecycle");
        let root = temp.path();
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "fixture@example.invalid"]);
        git(root, &["config", "user.name", "Тест"]);
        git(root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join(".gitignore"), "/.anki-repo/review/\n").unwrap();
        fs::write(root.join("source.txt"), "база\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "база"]);
        let base = git(root, &["rev-parse", "HEAD"]);
        fs::write(root.join("source.txt"), "проверяемый снимок\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "изменение"]);
        let head = git(root, &["rev-parse", "HEAD"]);
        let result = success(run_cli_in(
            Some(root),
            &[
                "--json",
                "code-review",
                "collect",
                "--base",
                &base,
                "--head",
                &head,
            ],
        ));
        let pack = root
            .join(result["artifact_dir"].as_str().unwrap())
            .join("review.json");
        Self { temp, pack, head }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    /// Каталог, в котором `prepare` создаёт каталоги заданий этого пакета.
    ///
    /// Пространство имён берётся из фактического расположения `review.json`,
    /// а не из зашитого имени.
    fn runs_directory(&self) -> PathBuf {
        self.pack
            .parent()
            .expect("у review.json есть каталог")
            .join("runs")
    }

    /// Ждёт появления каталога подготавливаемого задания, отличного от известных.
    fn discover_new_job(&self, known: &[PathBuf]) -> PathBuf {
        let runs = self.runs_directory();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(entries) = fs::read_dir(&runs) {
                let mut found: Vec<PathBuf> = entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| !known.contains(path) && path.join("state.json").is_file())
                    .collect();
                found.sort();
                if let Some(path) = found.into_iter().next() {
                    return path;
                }
            }
            assert!(
                Instant::now() < deadline,
                "каталог подготавливаемого задания не появился"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn prepare(&self, mode: &str) -> PathBuf {
        let result = success(run_cli_in(
            Some(self.root()),
            &[
                "--json",
                "code-review",
                "execution",
                "prepare",
                "--pack",
                self.pack.to_str().unwrap(),
                "--mode",
                mode,
                "--scope",
                "lifecycle",
            ],
        ));
        self.root().join(result["job_directory"].as_str().unwrap())
    }

    fn operation(&self, verb: &str, job: &Path) -> (i32, String, String) {
        run_cli_in(
            Some(self.root()),
            &[
                "--json",
                "code-review",
                "execution",
                verb,
                job.to_str().unwrap(),
            ],
        )
    }

    fn run(&self, job: &Path, arguments: &[&str]) -> (i32, String, String) {
        let mut args = vec![
            "--json",
            "code-review",
            "execution",
            "run",
            job.to_str().unwrap(),
            "--timeout-seconds",
            "10",
            "--",
        ];
        args.extend_from_slice(arguments);
        run_cli_in(Some(self.root()), &args)
    }
}

#[test]
fn cleanup_preserves_direction_report_and_removes_only_runtime() {
    let fixture = Fixture::new();
    let job = fixture.prepare("isolated_checks");
    let report = b"{\"status\":\"done\",\"evidence\":\"handoff\"}\n";
    fs::write(job.join("outputs/direction-report.json"), report).unwrap();
    fs::write(job.join("tmp/runtime.txt"), "временные байты".as_bytes()).unwrap();
    let result = success(fixture.operation("cleanup", &job));
    assert_eq!(result["workspace_removed"], true);
    assert_eq!(result["evidence_retained"], true);
    assert_eq!(
        fs::read(job.join("outputs/direction-report.json")).unwrap(),
        report
    );
    assert!(job.join("logs").is_dir());
    for surface in [
        "worktree",
        "target",
        "tmp",
        "scratch",
        "home",
        "cargo-home",
        "config",
        "hooks",
    ] {
        assert!(!job.join(surface).exists(), "не удалён {surface}");
    }
    success(fixture.operation("inspect", &job));
    // Повторная очистка также сохраняет завершённый отчёт.
    success(fixture.operation("cleanup", &job));
    assert_eq!(
        fs::read(job.join("outputs/direction-report.json")).unwrap(),
        report
    );
}

#[test]
fn cleanup_preflights_all_surfaces_before_removing_worktree() {
    use std::os::unix::fs::symlink;
    for replacement in ["symlink", "file"] {
        let fixture = Fixture::new();
        let outside = TempDir::new("execution-external");
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, "чужие байты".as_bytes()).unwrap();
        let job = fixture.prepare("isolated_checks");
        fs::remove_dir(job.join("scratch")).unwrap();
        if replacement == "symlink" {
            symlink(outside.path(), job.join("scratch")).unwrap();
        } else {
            fs::write(job.join("scratch"), "не каталог".as_bytes()).unwrap();
        }
        let (code, stdout, _) = fixture.operation("cleanup", &job);
        assert_ne!(code, 0, "{stdout}");
        assert_eq!(
            parse_json(&stdout)["error"]["code"],
            "review_artifact_conflict"
        );
        assert_eq!(fs::read(&sentinel).unwrap(), "чужие байты".as_bytes());
        assert!(
            job.join("worktree").is_dir(),
            "отказ очистки должен предшествовать удалению"
        );
        assert!(job.join("target").is_dir());
        assert_eq!(
            success(fixture.operation("inspect", &job))["workspace_removed"],
            false
        );
    }
}

#[test]
fn cleanup_preserves_regular_leftover_atomic_publication_files() {
    let fixture = Fixture::new();
    let job = fixture.prepare("isolated_checks");
    let publish = b"partial metadata publication";
    let review_publish = b"partial review artifact publication";
    fs::write(job.join(".publish-leftover"), publish).unwrap();
    fs::write(job.join(".review-publish-leftover"), review_publish).unwrap();

    let result = success(fixture.operation("cleanup", &job));
    assert_eq!(result["workspace_removed"], true);
    assert_eq!(fs::read(job.join(".publish-leftover")).unwrap(), publish);
    assert_eq!(
        fs::read(job.join(".review-publish-leftover")).unwrap(),
        review_publish
    );
    assert!(!job.join("worktree").exists());
}

#[test]
fn failures_before_spawn_leave_proven_not_started_job_inspectable() {
    for surface in ["logs/stdout.log", "logs/stderr.log", "home"] {
        let fixture = Fixture::new();
        let job = fixture.prepare("isolated_checks");
        let path = job.join(surface);
        if surface == "home" {
            fs::remove_dir(&path).unwrap();
            fs::write(&path, "препятствие окружения".as_bytes()).unwrap();
        } else {
            fs::write(&path, "существующее свидетельство".as_bytes()).unwrap();
        }
        let (code, stdout, _) = fixture.run(
            &job,
            &["/bin/sh", "-c", "printf started > ../outputs/started"],
        );
        assert_ne!(code, 0, "{stdout}");
        assert!(!job.join("outputs/started").exists());
        let inspection = success(fixture.operation("inspect", &job));
        assert_eq!(inspection["lifecycle"], "prepared", "{inspection}");
        assert_eq!(inspection["result"], Value::Null);
        if surface == "home" {
            assert!(!job.join("logs/stdout.log").exists());
            assert!(!job.join("logs/stderr.log").exists());
        } else if surface == "logs/stderr.log" {
            assert!(!job.join("logs/stdout.log").exists());
        }
        fs::remove_file(path).unwrap();
        if surface == "home" {
            fs::create_dir(job.join("home")).unwrap();
        }
        let result = success(fixture.run(
            &job,
            &["/bin/sh", "-c", "printf started > ../outputs/started"],
        ));
        assert_eq!(result["status"], "passed");
        assert_eq!(fs::read(job.join("outputs/started")).unwrap(), b"started");
        let result = confirmed_cleanup(&fixture, &job);
        assert_eq!(result["workspace_removed"], true);
    }
}

#[test]
fn unavailable_command_has_not_started_result_and_can_be_cleaned() {
    let fixture = Fixture::new();
    let job = fixture.prepare("disposable_source_experiment");
    let (code, stdout, _) = fixture.run(&job, &["/nonexistent/execution-fixture-command"]);
    assert_eq!(code, 127, "{stdout}");
    let result = &parse_json(&stdout)["result"];
    assert_eq!(result["status"], "unavailable");
    assert_eq!(result["enforcement"]["process_cleanup"], "not_started");
    assert_eq!(
        success(fixture.operation("inspect", &job))["lifecycle"],
        "completed"
    );
    assert_eq!(
        success(fixture.operation("cleanup", &job))["workspace_removed"],
        true
    );
    assert!(job.join("result.json").is_file());
    assert!(job.join("logs/stdout.log").is_file());
}

#[test]
fn executed_disposable_and_abandoned_jobs_require_descendant_confirmation() {
    let fixture = Fixture::new();
    let job = fixture.prepare("disposable_source_experiment");
    success(fixture.run(
        &job,
        &[
            "/bin/sh",
            "-c",
            "printf experiment > source.txt; printf report > ../outputs/direction-report.json",
        ],
    ));
    assert_eq!(
        fs::read(fixture.root().join("source.txt")).unwrap(),
        "проверяемый снимок\n".as_bytes()
    );
    let result = success(fixture.operation("cleanup", &job));
    assert_eq!(result["workspace_removed"], false);
    assert!(result["limitation"].as_str().unwrap().contains("потомк"));
    assert!(job.join("worktree").is_dir());
    assert_eq!(
        fs::read(job.join("outputs/direction-report.json")).unwrap(),
        b"report"
    );

    // Потеря lock при сохранённом Running не является доказательством остановки потомков.
    let abandoned = fixture.prepare("disposable_source_experiment");
    let mut state: Value =
        serde_json::from_slice(&fs::read(abandoned.join("state.json")).unwrap()).unwrap();
    state["lifecycle"] = "running".into();
    fs::write(
        abandoned.join("state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    assert_eq!(
        success(fixture.operation("inspect", &abandoned))["lifecycle"],
        "interrupted"
    );
    let (code, _, _) = fixture.operation("cleanup", &abandoned);
    assert_ne!(code, 0);
    assert!(abandoned.join("worktree").is_dir());
}

struct ActiveRun {
    child: Option<Child>,
    release: PathBuf,
}

impl ActiveRun {
    fn spawn(fixture: &Fixture, job: &Path, limit: &str) -> Self {
        let child = Command::new(cli_binary()).current_dir(fixture.root()).args([
            "--json", "code-review", "execution", "run", job.to_str().unwrap(),
            "--timeout-seconds", "20", "--max-parallel-jobs", limit, "--", "/bin/sh", "-c",
            "printf ready > ../outputs/ready; while test ! -f ../outputs/release; do /bin/sleep 0.02; done",
        ]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut run = Self {
            child: Some(child),
            release: job.join("outputs/release"),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !job.join("outputs/ready").is_file() {
            if let Some(status) = run.child.as_mut().unwrap().try_wait().unwrap() {
                panic!("процесс не дошёл до барьера: {status}");
            }
            assert!(Instant::now() < deadline, "процесс не дошёл до барьера");
            std::thread::sleep(Duration::from_millis(10));
        }
        run
    }

    fn finish(mut self) {
        fs::write(&self.release, "освободить".as_bytes()).unwrap();
        let output = self.child.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

impl Drop for ActiveRun {
    fn drop(&mut self) {
        // Освобождаем только собственную фикстуру даже при panic проверки.
        let _ = fs::write(&self.release, "освободить".as_bytes());
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
    }
}

#[test]
fn independent_processes_obey_stable_repository_parallel_policy() {
    let fixture = Fixture::new();
    let first = fixture.prepare("isolated_checks");
    let second = fixture.prepare("isolated_checks");
    let third = fixture.prepare("isolated_checks");
    // Барьер одновременно запускает независимые CLI-процессы; они конкурируют
    // за общую политику и разные слоты, затем оба удерживают свои ресурсы.
    let barrier = std::sync::Barrier::new(2);
    let (first_run, second_run) = std::thread::scope(|scope| {
        let first_thread = scope.spawn(|| {
            barrier.wait();
            ActiveRun::spawn(&fixture, &first, "2")
        });
        let second_thread = scope.spawn(|| {
            barrier.wait();
            ActiveRun::spawn(&fixture, &second, "2")
        });
        (first_thread.join().unwrap(), second_thread.join().unwrap())
    });
    let busy = run_cli_in(
        Some(fixture.root()),
        &[
            "--json",
            "code-review",
            "execution",
            "run",
            third.to_str().unwrap(),
            "--timeout-seconds",
            "10",
            "--max-parallel-jobs",
            "2",
            "--",
            "/bin/true",
        ],
    );
    assert_eq!(busy.0, 13, "{}", busy.1);
    let error = &parse_json(&busy.1)["error"];
    assert_eq!(error["code"], "execution_busy");
    assert_eq!(error["details"]["retryable"], true);
    assert_eq!(
        success(fixture.operation("inspect", &third))["lifecycle"],
        "prepared"
    );
    second_run.finish();
    for mismatch in ["1", "3"] {
        let output = run_cli_in(
            Some(fixture.root()),
            &[
                "--json",
                "code-review",
                "execution",
                "run",
                third.to_str().unwrap(),
                "--timeout-seconds",
                "10",
                "--max-parallel-jobs",
                mismatch,
                "--",
                "/bin/true",
            ],
        );
        assert_eq!(output.0, 3, "{}", output.1);
        assert_eq!(parse_json(&output.1)["error"]["code"], "invalid_request");
        assert_eq!(
            success(fixture.operation("inspect", &third))["lifecycle"],
            "prepared"
        );
    }
    first_run.finish();
    let output = run_cli_in(
        Some(fixture.root()),
        &[
            "--json",
            "code-review",
            "execution",
            "run",
            third.to_str().unwrap(),
            "--timeout-seconds",
            "10",
            "--max-parallel-jobs",
            "2",
            "--",
            "/bin/true",
        ],
    );
    success(output);
}

#[test]
fn missing_job_and_missing_manifest_are_read_errors() {
    let fixture = Fixture::new();
    let job = fixture.prepare("isolated_checks");
    let absent = job.with_file_name("00000000000000000000000000000000");
    let (code, stdout, _) = fixture.operation("inspect", &absent);
    assert_eq!(code, 4, "{stdout}");
    assert_eq!(parse_json(&stdout)["error"]["code"], "not_found");
    fs::remove_file(job.join("job.json")).unwrap();
    let (code, stdout, _) = fixture.operation("inspect", &job);
    assert_eq!(code, 3, "{stdout}");
    assert_eq!(parse_json(&stdout)["error"]["code"], "input_unreadable");
}

#[test]
fn private_cargo_home_has_deterministic_offline_behavior() {
    let fixture = Fixture::new();
    // Оба эксперимента пишут только в собственный disposable worktree.
    let local = fixture.prepare("disposable_source_experiment");
    fs::write(
        local.join("worktree/Cargo.toml"),
        "[package]\nname = \"local-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::create_dir(local.join("worktree/src")).unwrap();
    fs::write(
        local.join("worktree/src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .unwrap();
    let result = success(fixture.run(&local, &["cargo", "check", "--offline"]));
    assert_eq!(result["status"], "passed");
    assert!(local.join("target/debug").is_dir());
    assert!(!fixture.root().join("target").exists());

    let registry = fixture.prepare("disposable_source_experiment");
    fs::write(registry.join("worktree/Cargo.toml"), "[package]\nname = \"registry-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n[dependencies]\nserde = \"1\"\n").unwrap();
    fs::create_dir(registry.join("worktree/src")).unwrap();
    fs::write(
        registry.join("worktree/src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .unwrap();
    let (code, stdout, _) = fixture.run(&registry, &["cargo", "check", "--offline"]);
    assert_eq!(code, 9, "{stdout}");
    let result = &parse_json(&stdout)["result"];
    assert_eq!(result["status"], "failed");
    assert!(
        result["stderr"]["text"].as_str().unwrap().contains("serde"),
        "{result}"
    );
    assert!(!fixture.root().join("target").exists());
}

#[test]
fn incompatible_parallel_limit_cannot_expand_live_repository_capacity() {
    let fixture = Fixture::new();
    let first = fixture.prepare("isolated_checks");
    let mismatch = fixture.prepare("isolated_checks");
    let first_run = ActiveRun::spawn(&fixture, &first, "1");
    let output = run_cli_in(
        Some(fixture.root()),
        &[
            "--json",
            "code-review",
            "execution",
            "run",
            mismatch.to_str().unwrap(),
            "--timeout-seconds",
            "10",
            "--max-parallel-jobs",
            "2",
            "--",
            "/bin/true",
        ],
    );
    assert_eq!(output.0, 3, "{}", output.1);
    assert_eq!(parse_json(&output.1)["error"]["code"], "invalid_request");
    assert_eq!(
        success(fixture.operation("inspect", &mismatch))["lifecycle"],
        "prepared"
    );
    first_run.finish();
}

fn confirmed_cleanup(fixture: &Fixture, job: &Path) -> Value {
    success(run_cli_in(
        Some(fixture.root()),
        &[
            "--json",
            "code-review",
            "execution",
            "cleanup",
            job.to_str().unwrap(),
            "--confirm-no-live-descendants",
        ],
    ))
}

#[test]
fn operator_confirmed_cleanup_releases_completed_and_abandoned_worktrees() {
    let fixture = Fixture::new();
    for abandoned in [false, true] {
        let job = fixture.prepare("disposable_source_experiment");
        if abandoned {
            let mut state: Value =
                serde_json::from_slice(&fs::read(job.join("state.json")).unwrap()).unwrap();
            state["lifecycle"] = "running".into();
            fs::write(job.join("state.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        } else {
            success(fixture.run(&job, &["/bin/sh", "-c", "printf changed > source.txt"]));
        }
        fs::write(job.join("outputs/direction-report.json"), b"report bytes\n").unwrap();
        let result = confirmed_cleanup(&fixture, &job);
        assert_eq!(result["workspace_removed"], true);
        assert_eq!(result["evidence_retained"], true);
        assert!(result["limitation"].as_str().unwrap().contains("оператор"));
        assert!(!job.join("worktree").exists());
        assert!(
            !git(fixture.root(), &["worktree", "list", "--porcelain"])
                .contains(job.to_str().unwrap())
        );
        assert_eq!(
            fs::read(job.join("outputs/direction-report.json")).unwrap(),
            b"report bytes\n"
        );
        assert_eq!(confirmed_cleanup(&fixture, &job)["workspace_removed"], true);
    }
}

#[test]
fn active_job_cannot_be_cleaned_even_with_descendant_confirmation() {
    let fixture = Fixture::new();
    let job = fixture.prepare("disposable_source_experiment");
    let active = ActiveRun::spawn(&fixture, &job, "1");
    let (code, stdout, _) = run_cli_in(
        Some(fixture.root()),
        &[
            "--json",
            "code-review",
            "execution",
            "cleanup",
            job.to_str().unwrap(),
            "--confirm-no-live-descendants",
        ],
    );
    assert_eq!(code, 13, "{stdout}");
    assert_eq!(parse_json(&stdout)["error"]["code"], "execution_busy");
    assert!(job.join("worktree").is_dir());
    assert!(job.join("outputs/ready").is_file());
    active.finish();
    confirmed_cleanup(&fixture, &job);
}

#[test]
fn timeout_and_cancelled_disposable_jobs_preserve_evidence_during_confirmed_cleanup() {
    let fixture = Fixture::new();
    let timed = fixture.prepare("disposable_source_experiment");
    let (code, stdout, _) = run_cli_in(
        Some(fixture.root()),
        &[
            "--json",
            "code-review",
            "execution",
            "run",
            timed.to_str().unwrap(),
            "--timeout-seconds",
            "1",
            "--",
            "/bin/sh",
            "-c",
            "printf report > ../outputs/direction-report.json; exec /bin/sleep 30",
        ],
    );
    assert_eq!(code, 11, "{stdout}");
    assert_eq!(parse_json(&stdout)["result"]["status"], "timed_out");
    assert_eq!(
        confirmed_cleanup(&fixture, &timed)["workspace_removed"],
        true
    );
    assert_eq!(
        fs::read(timed.join("outputs/direction-report.json")).unwrap(),
        b"report"
    );
    assert!(timed.join("result.json").is_file());

    let cancelled = fixture.prepare("disposable_source_experiment");
    let mut active = ActiveRun::spawn(&fixture, &cancelled, "1");
    success(fixture.operation("cancel", &cancelled));
    let output = active.child.take().unwrap().wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(12));
    assert_eq!(
        parse_json(&String::from_utf8(output.stdout).unwrap())["result"]["status"],
        "cancelled"
    );
    assert_eq!(
        confirmed_cleanup(&fixture, &cancelled)["workspace_removed"],
        true
    );
    assert!(cancelled.join("result.json").is_file());
    assert!(cancelled.join("outputs/ready").is_file());
}

#[test]
fn git_operation_failure_is_distinct_from_snapshot_mismatch() {
    let fixture = Fixture::new();
    let broken = fixture.prepare("isolated_checks");
    fs::write(broken.join("worktree/.git"), "невалидные Git metadata\n").unwrap();
    let (code, stdout, _) = fixture.run(&broken, &["/bin/true"]);
    assert_eq!(code, 14, "{stdout}");
    assert_eq!(
        parse_json(&stdout)["error"]["code"],
        "process_operation_failed"
    );
    assert_eq!(
        success(fixture.operation("inspect", &broken))["lifecycle"],
        "prepared"
    );

    let mismatch = fixture.prepare("isolated_checks");
    git(
        &mismatch.join("worktree"),
        &["checkout", "--detach", "HEAD^"],
    );
    let (code, stdout, _) = fixture.run(&mismatch, &["/bin/true"]);
    assert_eq!(code, 3, "{stdout}");
    assert_eq!(
        parse_json(&stdout)["error"]["code"],
        "review_artifact_invalid"
    );
    assert_eq!(
        success(fixture.operation("inspect", &mismatch))["lifecycle"],
        "prepared"
    );
}

#[test]
fn cleanup_rejects_foreign_attestation_and_preserves_operator_limitation() {
    let fixture = Fixture::new();
    let first = fixture.prepare("disposable_source_experiment");
    let second = fixture.prepare("disposable_source_experiment");
    success(fixture.run(&first, &["/bin/true"]));
    success(fixture.run(&second, &["/bin/true"]));
    let initial = confirmed_cleanup(&fixture, &first);
    let repeated = success(fixture.operation("cleanup", &first));
    assert_eq!(repeated["limitation"], initial["limitation"]);
    assert!(
        repeated["limitation"]
            .as_str()
            .unwrap()
            .contains("оператор")
    );
    let attestation = fs::read(first.join("cleanup-attestation.json")).unwrap();
    fs::remove_file(first.join("cleanup-attestation.json")).unwrap();
    let (code, stdout, _) = fixture.operation("cleanup", &first);
    assert_eq!(code, 7, "{stdout}");
    fs::write(first.join("cleanup-attestation.json"), attestation).unwrap();
    fs::copy(
        first.join("cleanup-attestation.json"),
        second.join("cleanup-attestation.json"),
    )
    .unwrap();
    let (code, stdout, _) = run_cli_in(
        Some(fixture.root()),
        &[
            "--json",
            "code-review",
            "execution",
            "cleanup",
            second.to_str().unwrap(),
            "--confirm-no-live-descendants",
        ],
    );
    assert_eq!(code, 7, "{stdout}");
    assert_eq!(
        parse_json(&stdout)["error"]["code"],
        "review_artifact_conflict"
    );
    assert!(second.join("worktree").is_dir());
    assert!(second.join("target").is_dir());
    fs::remove_file(second.join("cleanup-attestation.json")).unwrap();
    confirmed_cleanup(&fixture, &second);
}

#[test]
fn cleanup_rejects_unexpected_job_entries_before_any_mutation() {
    let fixture = Fixture::new();
    let job = fixture.prepare("isolated_checks");
    fs::write(job.join("foreign.txt"), b"unchanged").unwrap();
    let (code, stdout, _) = fixture.operation("cleanup", &job);
    assert_eq!(code, 7, "{stdout}");
    assert_eq!(fs::read(job.join("foreign.txt")).unwrap(), b"unchanged");
    assert!(job.join("worktree").is_dir());
    assert!(job.join("target").is_dir());
    assert!(!job.join("cleanup-attestation.json").exists());
}

#[test]
fn runtime_cleanup_does_not_follow_nested_symlink_targets() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let outside = TempDir::new("execution-nested-symlink");
    fs::write(outside.path().join("sentinel"), b"outside preserved").unwrap();
    let job = fixture.prepare("isolated_checks");
    symlink(outside.path(), job.join("tmp/external-link")).unwrap();
    assert_eq!(
        success(fixture.operation("cleanup", &job))["workspace_removed"],
        true
    );
    assert_eq!(
        fs::read(outside.path().join("sentinel")).unwrap(),
        b"outside preserved"
    );
}

/// Административная запись Git, чей `gitdir` указывает на `<worktree>/.git`.
fn admin_record(root: &Path, worktree: &Path) -> PathBuf {
    let expected = worktree.join(".git");
    let mut found = Vec::new();
    for entry in fs::read_dir(root.join(".git/worktrees")).unwrap() {
        let entry = entry.unwrap();
        let gitdir = fs::read_to_string(entry.path().join("gitdir")).unwrap_or_default();
        if Path::new(gitdir.trim()) == expected {
            found.push(entry.path());
        }
    }
    assert_eq!(found.len(), 1, "запись Git для {worktree:?}");
    found.pop().unwrap()
}

/// Путь к настоящему `git` из PATH тестового процесса.
fn real_git() -> PathBuf {
    for directory in std::env::split_paths(&std::env::var_os("PATH").expect("PATH задан")) {
        let candidate = directory.join("git");
        let executable = fs::metadata(&candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
        if executable {
            return candidate;
        }
    }
    panic!("настоящий git не найден в PATH");
}

/// Детерминированный барьер на существенном шаге подготовки.
///
/// `trusted_git()` резолвит `git` через PATH, поэтому тест подкладывает в PATH
/// дочернего CLI shim-обёртку `git`: на `worktree add` она сообщает о входе в шаг
/// и ждёт разрешения, все остальные вызовы `git` пропускает прозрачно. Так
/// interleaving задаётся событием, а не задержкой «на угад», и production-код
/// не получает тестовых хуков. Обёртка живёт только в тестовом каталоге.
struct PreparationBarrier {
    _temp: TempDir,
    enter: PathBuf,
    release: PathBuf,
    aborted: PathBuf,
    path: OsString,
    /// Процесс подготовки принадлежит барьеру: при падении теста он и его shim
    /// завершаются, а не ждут разрешения вечно.
    child: Option<Child>,
}

impl PreparationBarrier {
    fn new() -> Self {
        let temp = TempDir::new("execution-preparation-barrier");
        let shim_directory = temp.path().join("path");
        fs::create_dir(&shim_directory).unwrap();
        let enter = temp.path().join("entered");
        let release = temp.path().join("release");
        let aborted = temp.path().join("aborted");
        let script = format!(
            "#!/bin/sh\n\
             real='{}'\n\
             enter='{}'\n\
             release='{}'\n\
             aborted='{}'\n\
             parent=$PPID\n\
             previous=\n\
             target=0\n\
             for argument in \"$@\"; do\n\
             \tif [ \"$previous\" = worktree ] && [ \"$argument\" = add ]; then target=1; fi\n\
             \tprevious=$argument\n\
             done\n\
             if [ \"$target\" = 1 ]; then\n\
             \t: > \"$enter\"\n\
             \twhile [ ! -e \"$release\" ]; do\n\
             \t\tif ! kill -0 \"$parent\" 2>/dev/null; then : > \"$aborted\"; exit 1; fi\n\
             \t\t/bin/sleep 0.05\n\
             \tdone\n\
             fi\n\
             exec \"$real\" \"$@\"\n",
            real_git().display(),
            enter.display(),
            release.display(),
            aborted.display(),
        );
        let shim = shim_directory.join("git");
        fs::write(&shim, script).unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = vec![shim_directory];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH задан"),
        ));
        let path = std::env::join_paths(paths).unwrap();
        Self {
            _temp: temp,
            enter,
            release,
            aborted,
            path,
            child: None,
        }
    }

    /// Запускает независимый CLI-процесс `prepare` с барьером в PATH.
    fn spawn(&mut self, fixture: &Fixture, scope: &str) {
        assert!(self.child.is_none(), "подготовка уже запущена");
        self.child = Some(
            Command::new(cli_binary())
                .current_dir(fixture.root())
                .env("PATH", &self.path)
                .args([
                    "--json",
                    "code-review",
                    "execution",
                    "prepare",
                    "--pack",
                    fixture.pack.to_str().unwrap(),
                    "--mode",
                    "isolated_checks",
                    "--scope",
                    scope,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }

    /// Ждёт входа в `git worktree add`; ожидание события, а не гонки во времени.
    fn wait_until_entered(&mut self) {
        let enter = self.enter.clone();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !enter.is_file() {
            let status = self
                .child
                .as_mut()
                .expect("подготовка запущена")
                .try_wait()
                .unwrap();
            if let Some(status) = status {
                panic!("подготовка завершилась до барьера: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "подготовка не дошла до шага `git worktree add`"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn release(&self) {
        fs::write(&self.release, "продолжить".as_bytes()).unwrap();
    }

    /// Забирает завершение процесса подготовки: барьер больше его не удерживает.
    fn wait_with_output(&mut self) -> std::process::Output {
        self.child
            .take()
            .expect("подготовка запущена")
            .wait_with_output()
            .unwrap()
    }

    /// Обрывает подготовку и собирает процесс: shim замечает исчезновение
    /// родителя и завершается сам, поэтому зависших процессов не остаётся.
    fn kill_and_reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Ждёт, пока оборванная обёртка сообщит об исчезновении процесса подготовки.
    fn wait_until_aborted(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.aborted.is_file() {
            assert!(
                Instant::now() < deadline,
                "обёртка не заметила обрыв подготовки"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for PreparationBarrier {
    fn drop(&mut self) {
        // При panic в тесте сначала собираем процесс подготовки, а уже потом
        // освобождаем барьер: удаление release-файла вместе с TempDir не должно
        // оставлять ни подготовку, ни shim ждать разрешения вечно.
        self.kill_and_reap();
        let _ = fs::write(&self.release, "освободить".as_bytes());
    }
}

/// Доводит независимую подготовку до барьера и обрывает её: каталог задания и
/// runtime-поверхности уже созданы, рабочее дерево ещё нет.
fn abandon_preparation(
    fixture: &Fixture,
    barrier: &mut PreparationBarrier,
    scope: &str,
) -> PathBuf {
    barrier.spawn(fixture, scope);
    barrier.wait_until_entered();
    let job = fixture.discover_new_job(&[]);
    barrier.kill_and_reap();
    barrier.wait_until_aborted();
    assert!(
        !job.join("worktree").exists(),
        "оборванная подготовка не создаёт рабочее дерево"
    );
    job
}

#[test]
fn concurrent_cleanup_cannot_remove_resources_while_preparation_creates_worktree() {
    let fixture = Fixture::new();
    // Чужие ресурсы: посторонний worktree и посторонний файл в том же репозитории.
    let foreign_worktree = fixture.root().join("foreign-worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            foreign_worktree.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let foreign_note = fixture.root().join(".anki-repo/foreign-note.txt");
    fs::write(&foreign_note, "чужие байты").unwrap();
    // Соседнее задание доказывает единичность владения: его ресурсы не трогают.
    let neighbor = fixture.prepare("isolated_checks");
    let neighbor_head = git(&neighbor.join("worktree"), &["rev-parse", "HEAD"]);

    let mut barrier = PreparationBarrier::new();
    barrier.spawn(&fixture, "concurrent-preparation");
    barrier.wait_until_entered();
    let job = fixture.discover_new_job(std::slice::from_ref(&neighbor));

    // Подготовка идёт: задание не выдаётся за готовое.
    let inspection = success(fixture.operation("inspect", &job));
    assert_eq!(inspection["lifecycle"], "preparing", "{inspection}");
    assert_eq!(inspection["result"], Value::Null);
    assert_eq!(inspection["workspace_removed"], false);
    assert!(
        inspection["limitations"][0]
            .as_str()
            .unwrap()
            .contains("другим процессом"),
        "{inspection}"
    );
    assert!(
        !job.join("worktree").exists(),
        "worktree ещё не создан: подготовка остановлена на барьере"
    );

    // `run` не принимает частичную подготовку за готовую и ничего не исполняет.
    let (code, stdout, _) = fixture.run(
        &job,
        &["/bin/sh", "-c", "printf started > ../outputs/started"],
    );
    assert_ne!(code, 0, "{stdout}");
    assert!(
        matches!(
            parse_json(&stdout)["error"]["code"].as_str().unwrap(),
            "execution_busy" | "review_artifact_conflict"
        ),
        "{stdout}"
    );
    assert!(!job.join("outputs/started").exists());

    // Конкурентный cleanup получает явный наблюдаемый отказ и не удаляет ресурсы.
    let (code, stdout, _) = fixture.operation("cleanup", &job);
    assert_eq!(code, 13, "{stdout}");
    let error = &parse_json(&stdout)["error"];
    assert_eq!(error["code"], "execution_busy");
    assert_eq!(error["details"]["retryable"], true);
    assert!(!job.join("cleanup-attestation.json").exists());
    for surface in ["target", "tmp", "scratch", "outputs", "logs", "hooks"] {
        assert!(job.join(surface).is_dir(), "снят ресурс {surface}");
    }
    let state: Value = serde_json::from_slice(&fs::read(job.join("state.json")).unwrap()).unwrap();
    assert_eq!(state["lifecycle"], "preparing");
    assert_eq!(state["workspace_removed"], false);
    assert!(foreign_worktree.is_dir());
    assert!(neighbor.join("worktree").is_dir());

    // Освобождаем барьер: подготовка доводится до конца.
    barrier.release();
    let output = barrier.wait_with_output();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let prepared = parse_json(&String::from_utf8(output.stdout).unwrap())["result"].clone();
    assert_eq!(
        prepared["job_id"],
        job.file_name().unwrap().to_str().unwrap()
    );

    // Готовое задание честно подготовлено, целевой HEAD и чужие ресурсы сохранены.
    let inspection = success(fixture.operation("inspect", &job));
    assert_eq!(inspection["lifecycle"], "prepared", "{inspection}");
    assert!(inspection["limitations"].as_array().unwrap().is_empty());
    assert_eq!(
        git(&job.join("worktree"), &["rev-parse", "HEAD"]),
        fixture.head
    );
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(worktrees.contains(foreign_worktree.to_str().unwrap()));
    assert!(worktrees.contains(job.join("worktree").to_str().unwrap()));
    assert_eq!(fs::read(&foreign_note).unwrap(), "чужие байты".as_bytes());
    assert_eq!(
        git(&neighbor.join("worktree"), &["rev-parse", "HEAD"]),
        neighbor_head
    );
    assert_eq!(
        success(fixture.operation("inspect", &neighbor))["lifecycle"],
        "prepared"
    );

    // Полностью подготовленное задание работает по прежнему контракту.
    let result = success(fixture.run(
        &job,
        &["/bin/sh", "-c", "printf prepared > ../outputs/run.txt"],
    ));
    assert_eq!(result["status"], "passed");
    assert_eq!(fs::read(job.join("outputs/run.txt")).unwrap(), b"prepared");
    assert_eq!(confirmed_cleanup(&fixture, &job)["workspace_removed"], true);
    assert!(!job.join("worktree").exists());
    assert!(foreign_worktree.is_dir());
    assert_eq!(fs::read(&foreign_note).unwrap(), "чужие байты".as_bytes());
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
}

#[test]
fn preparation_killed_before_publication_stays_unlaunchable_and_cleans_without_confirmation() {
    let fixture = Fixture::new();
    let mut barrier = PreparationBarrier::new();
    barrier.spawn(&fixture, "abandoned-preparation");
    barrier.wait_until_entered();
    let job = fixture.discover_new_job(&[]);

    // Имитируем убийство CLI в середине подготовки: состояние не должно стать `prepared`.
    barrier.kill_and_reap();
    barrier.wait_until_aborted();
    assert!(
        !job.join("worktree").exists(),
        "оборванная подготовка не создаёт рабочее дерево"
    );

    let inspection = success(fixture.operation("inspect", &job));
    assert_eq!(inspection["lifecycle"], "preparing", "{inspection}");
    assert_eq!(inspection["result"], Value::Null);
    assert_eq!(inspection["workspace_removed"], false);
    assert!(
        inspection["limitations"][0]
            .as_str()
            .unwrap()
            .contains("не удерживает"),
        "{inspection}"
    );

    // Незапускаемое задание не запускается и не создаёт следов исполнения.
    let (code, stdout, _) = fixture.run(
        &job,
        &["/bin/sh", "-c", "printf started > ../outputs/started"],
    );
    assert_ne!(code, 0, "{stdout}");
    assert_eq!(
        parse_json(&stdout)["error"]["code"],
        "review_artifact_conflict"
    );
    assert!(!job.join("outputs/started").exists());

    // Отмена частичного задания фиксируется маркером и не запускает код.
    let cancelled = success(fixture.operation("cancel", &job));
    assert_eq!(cancelled["lifecycle"], "preparing", "{cancelled}");
    assert!(job.join("cancel.json").is_file());

    // Частичный job очищается без ложного утверждения об отсутствии потомков.
    let result = success(fixture.operation("cleanup", &job));
    assert_eq!(result["workspace_removed"], true);
    assert_eq!(result["evidence_retained"], true);
    assert_eq!(result["limitation"], Value::Null);
    assert!(!job.join("cleanup-attestation.json").exists());
    assert!(!job.join("target").exists());
    assert!(job.join("job.json").is_file());
    assert!(job.join("source-review.json").is_file());

    let inspection = success(fixture.operation("inspect", &job));
    assert_eq!(inspection["lifecycle"], "preparing", "{inspection}");
    assert_eq!(inspection["workspace_removed"], true);
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
}

/// Авария внутри `git worktree add`: каталог рабочего дерева уже создан, но Git
/// о нём ещё не знает. Штатная очистка обязана довести восстановление до конца.
#[test]
fn cleanup_recovers_worktree_directory_left_unregistered_by_interrupted_git() {
    let fixture = Fixture::new();
    let foreign_worktree = fixture.root().join("foreign-worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            foreign_worktree.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let mut barrier = PreparationBarrier::new();
    let job = abandon_preparation(&fixture, &mut barrier, "interrupted-worktree-add");

    // Каталог принадлежит заданию, но в списке worktrees его нет: именно это
    // состояние оставляет kill между созданием каталога и регистрацией.
    let partial = job.join("worktree");
    fs::create_dir(&partial).unwrap();
    fs::write(partial.join("partial.txt"), "частичные байты".as_bytes()).unwrap();
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(
        !worktrees.contains(partial.to_str().unwrap()),
        "{worktrees}"
    );

    let inspection = success(fixture.operation("inspect", &job));
    assert_eq!(inspection["lifecycle"], "preparing", "{inspection}");
    assert!(
        inspection["limitations"][0]
            .as_str()
            .unwrap()
            .contains("не удерживает"),
        "{inspection}"
    );

    // Очистка не требует ложного подтверждения об отсутствии потомков и
    // завершается консистентно: код этого задания не запускался.
    let result = success(fixture.operation("cleanup", &job));
    assert_eq!(result["workspace_removed"], true, "{result}");
    assert_eq!(result["evidence_retained"], true, "{result}");
    assert_eq!(result["limitation"], Value::Null, "{result}");
    assert!(!result.to_string().contains("/proc/"), "{result}");
    assert!(!job.join("cleanup-attestation.json").exists());
    assert!(
        !partial.exists(),
        "каталог незарегистрированного worktree не удалён"
    );
    // Логи и outputs сохраняются как свидетельства, остальные поверхности удалены.
    for surface in [
        "target",
        "tmp",
        "scratch",
        "home",
        "cargo-home",
        "config",
        "hooks",
    ] {
        assert!(!job.join(surface).exists(), "не удалён {surface}");
    }
    assert!(job.join("logs").is_dir());
    assert!(job.join("outputs").is_dir());
    assert!(job.join("job.json").is_file());
    assert!(job.join("source-review.json").is_file());

    // Чужие worktrees, артефакты и Git metadata целы.
    assert!(foreign_worktree.is_dir());
    assert_eq!(git(&foreign_worktree, &["rev-parse", "HEAD"]), fixture.head);
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(
        worktrees.contains(foreign_worktree.to_str().unwrap()),
        "{worktrees}"
    );
    assert!(
        !worktrees.contains(partial.to_str().unwrap()),
        "{worktrees}"
    );

    // Повторный inspect консистентен, повторная очистка идемпотентна.
    let inspection = success(fixture.operation("inspect", &job));
    assert_eq!(inspection["lifecycle"], "preparing", "{inspection}");
    assert_eq!(inspection["workspace_removed"], true, "{inspection}");
    assert_eq!(
        success(fixture.operation("cleanup", &job))["workspace_removed"],
        true
    );
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
}

/// Авария после регистрации: административная запись Git есть, а связь рабочего
/// дерева потеряна. Очистка обязана убрать и каталог, и застарелую запись.
#[test]
fn cleanup_prunes_stale_git_record_of_broken_owned_worktree() {
    let fixture = Fixture::new();
    let foreign_worktree = fixture.root().join("foreign-worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            foreign_worktree.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let mut barrier = PreparationBarrier::new();
    let job = abandon_preparation(&fixture, &mut barrier, "broken-worktree-record");

    let owned = job.join("worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            owned.to_str().unwrap(),
            &fixture.head,
        ],
    );
    fs::remove_file(owned.join(".git")).unwrap();
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(worktrees.contains(owned.to_str().unwrap()), "{worktrees}");

    let result = success(fixture.operation("cleanup", &job));
    assert_eq!(result["workspace_removed"], true, "{result}");
    assert_eq!(result["limitation"], Value::Null, "{result}");
    assert!(!owned.exists(), "каталог рабочего дерева не удалён");
    // Логи и outputs сохраняются как свидетельства, остальные поверхности удалены.
    for surface in [
        "target",
        "tmp",
        "scratch",
        "home",
        "cargo-home",
        "config",
        "hooks",
    ] {
        assert!(!job.join(surface).exists(), "не удалён {surface}");
    }
    assert!(job.join("logs").is_dir());
    assert!(job.join("outputs").is_dir());

    // Застарелая запись удалена, чужие worktrees и целевой HEAD сохранены.
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains(owned.to_str().unwrap()), "{worktrees}");
    assert!(
        worktrees.contains(foreign_worktree.to_str().unwrap()),
        "{worktrees}"
    );
    assert!(foreign_worktree.is_dir());
    assert_eq!(git(&foreign_worktree, &["rev-parse", "HEAD"]), fixture.head);
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
    assert_eq!(
        success(fixture.operation("inspect", &job))["workspace_removed"],
        true
    );
}

/// Собственная застарелая запись Git удаляется, а чужая prunable-запись остаётся:
/// очистка задания не имеет права чистить административные записи репозитория
/// целиком.
#[test]
fn cleanup_removes_only_own_stale_worktree_record() {
    let fixture = Fixture::new();
    let mut barrier = PreparationBarrier::new();
    let job = abandon_preparation(&fixture, &mut barrier, "own-stale-record");

    // Чужая запись, ставшая prunable: каталог временно убран, запись осталась.
    let foreign = fixture.root().join("foreign-worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            foreign.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let foreign_record = admin_record(fixture.root(), &foreign);
    let foreign_gitdir = fs::read_to_string(foreign_record.join("gitdir")).unwrap();
    fs::remove_dir_all(&foreign).unwrap();

    // Собственная запись с потерянной связью: регистрация есть, `.git` нет.
    let owned = job.join("worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            owned.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let owned_record = admin_record(fixture.root(), &owned);
    fs::remove_file(owned.join(".git")).unwrap();

    let result = success(fixture.operation("cleanup", &job));
    assert_eq!(result["workspace_removed"], true, "{result}");
    assert_eq!(result["limitation"], Value::Null, "{result}");
    assert!(!owned.exists(), "каталог рабочего дерева не удалён");
    assert!(
        !owned_record.exists(),
        "собственная застарелая запись не удалена"
    );

    // Чужая prunable-запись обязана пережить очистку нашего задания: каталог,
    // возвращённый на место, снова распознаётся Git.
    assert!(
        foreign_record.join("gitdir").is_file(),
        "чужая административная запись Git удалена"
    );
    assert_eq!(
        fs::read_to_string(foreign_record.join("gitdir")).unwrap(),
        foreign_gitdir
    );
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(worktrees.contains(foreign.to_str().unwrap()), "{worktrees}");
    assert!(
        worktrees.contains("prunable"),
        "чужая запись потеряла признак prunable: {worktrees}"
    );
    fs::create_dir_all(&foreign).unwrap();
    fs::write(
        foreign.join(".git"),
        format!("gitdir: {}\n", foreign_record.display()),
    )
    .unwrap();
    assert_eq!(
        git(&foreign, &["rev-parse", "--is-inside-work-tree"]),
        "true",
        "вернувшийся чужой worktree не распознан"
    );
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
}

/// Собственное рабочее дерево, помеченное `locked`, тоже очищается: запись
/// подтверждена по `gitdir` и по закреплённому каталогу, поэтому Git снимает
/// блокировку двойным `--force` только у своего дерева.
#[test]
fn cleanup_removes_own_locked_worktree_record() {
    let fixture = Fixture::new();
    let foreign_worktree = fixture.root().join("foreign-worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            foreign_worktree.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let mut barrier = PreparationBarrier::new();
    let job = abandon_preparation(&fixture, &mut barrier, "locked-own-worktree");

    let owned = job.join("worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            owned.to_str().unwrap(),
            &fixture.head,
        ],
    );
    git(
        fixture.root(),
        &[
            "worktree",
            "lock",
            "--reason",
            "проверка",
            owned.to_str().unwrap(),
        ],
    );
    let owned_record = admin_record(fixture.root(), &owned);
    assert!(owned_record.join("locked").is_file());

    let result = success(fixture.operation("cleanup", &job));
    assert_eq!(result["workspace_removed"], true, "{result}");
    assert_eq!(result["evidence_retained"], true, "{result}");
    assert_eq!(result["limitation"], Value::Null, "{result}");
    assert!(
        !owned.exists(),
        "каталог собственного locked worktree не удалён"
    );
    assert!(
        !owned_record.exists(),
        "запись собственного locked worktree осталась"
    );
    for surface in [
        "target",
        "tmp",
        "scratch",
        "home",
        "cargo-home",
        "config",
        "hooks",
    ] {
        assert!(!job.join(surface).exists(), "не удалён {surface}");
    }
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(!worktrees.contains(owned.to_str().unwrap()), "{worktrees}");
    assert!(
        worktrees.contains(foreign_worktree.to_str().unwrap()),
        "{worktrees}"
    );
    assert!(foreign_worktree.is_dir());
    assert_eq!(git(&foreign_worktree, &["rev-parse", "HEAD"]), fixture.head);
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
    assert_eq!(
        success(fixture.operation("inspect", &job))["workspace_removed"],
        true
    );
}

/// Собственная административная запись Git убирается и тогда, когда каталога
/// рабочего дерева уже нет: прерванная очистка не оставляет prunable-запись,
/// о которой инструмент рапортует как об очищенной.
#[test]
fn cleanup_removes_own_record_when_worktree_directory_is_absent() {
    let fixture = Fixture::new();
    let foreign_worktree = fixture.root().join("foreign-worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            foreign_worktree.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let mut barrier = PreparationBarrier::new();
    let job = abandon_preparation(&fixture, &mut barrier, "absent-worktree-record");

    let owned = job.join("worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            owned.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let owned_record = admin_record(fixture.root(), &owned);
    // Наблюдаемое состояние прерванной очистки: каталог уже удалён, запись осталась.
    fs::remove_dir_all(&owned).unwrap();
    assert!(!owned.exists());
    assert!(owned_record.join("gitdir").is_file());

    let result = success(fixture.operation("cleanup", &job));
    assert_eq!(result["workspace_removed"], true, "{result}");
    assert_eq!(result["limitation"], Value::Null, "{result}");
    assert!(!result.to_string().contains("/proc/"), "{result}");
    assert!(
        !owned_record.exists(),
        "собственная застарелая запись осталась: {result}"
    );

    // Чужие записи, ресурсы и целевой HEAD не тронуты.
    assert!(foreign_worktree.is_dir());
    assert_eq!(git(&foreign_worktree, &["rev-parse", "HEAD"]), fixture.head);
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(
        worktrees.contains(foreign_worktree.to_str().unwrap()),
        "{worktrees}"
    );

    // Повторная очистка идемпотентна и тоже не оставляет своей записи.
    let repeated = success(fixture.operation("cleanup", &job));
    assert_eq!(repeated["workspace_removed"], true, "{repeated}");
    assert!(
        !owned_record.exists(),
        "собственная запись вернулась после повторной очистки: {repeated}"
    );
}

/// Повторная очистка обязана убрать собственную административную запись задания,
/// оставшуюся от неудавшегося первого прохода: успех не рапортуется, пока запись
/// на месте.
fn assert_repeated_cleanup_removes_record(fixture: &Fixture, job: &Path, record: &Path) {
    let repeated = success(fixture.operation("cleanup", job));
    assert_eq!(repeated["workspace_removed"], true, "{repeated}");
    assert_eq!(repeated["limitation"], Value::Null, "{repeated}");
    assert!(
        !record.exists(),
        "собственная запись осталась после повторной очистки: {repeated}"
    );
}

/// Первый проход очистки может удалить каталог, но не суметь удалить запись
/// (например, административный каталог закрыт от записи). Повторная очистка
/// обязана довести дело до конца, а не рапортовать успех, оставляя запись.
#[test]
fn repeated_cleanup_removes_record_left_by_failed_first_pass() {
    let fixture = Fixture::new();
    let foreign_worktree = fixture.root().join("foreign-worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            foreign_worktree.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let mut barrier = PreparationBarrier::new();
    let job = abandon_preparation(&fixture, &mut barrier, "record-left-by-failed-pass");

    let owned = job.join("worktree");
    git(
        fixture.root(),
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            owned.to_str().unwrap(),
            &fixture.head,
        ],
    );
    let owned_record = admin_record(fixture.root(), &owned);
    // Связь `.git` потеряна: удаление каталога проходит, а удаление записи — нет.
    fs::remove_file(owned.join(".git")).unwrap();
    let original = fs::metadata(&owned_record).unwrap().permissions();
    let mut readonly = original.clone();
    readonly.set_mode(0o500);
    fs::set_permissions(&owned_record, readonly).unwrap();

    // Отказ удаления записи воспроизводится запретом прав на её каталог, но под
    // root mode-биты не действуют: сначала проба действенности запрета — та же
    // проба, что в `note_lifecycle_contract`.
    let probe = owned_record.join("проба-прав");
    let rejecting = fs::write(&probe, b"x").is_err();
    if !rejecting {
        let _ = fs::remove_file(&probe);
    }

    if rejecting {
        // Запрет действует: проверяем сам сценарий — первый проход удаляет
        // каталог, но не рапортует успех, пока запись не удалена.
        let first = success(fixture.operation("cleanup", &job));
        assert_eq!(first["workspace_removed"], false, "{first}");
        assert!(first["limitation"].is_string(), "{first}");
        assert!(
            !owned.exists(),
            "каталог рабочего дерева не удалён: {first}"
        );
        assert!(
            owned_record.join("gitdir").is_file(),
            "запись исчезла раньше времени: {first}"
        );
        fs::set_permissions(&owned_record, original).unwrap();
    } else {
        // Разовый отказ невоспроизводим: первый проход не проверяем, но
        // состояние после него («каталога нет, запись осталась») собираем
        // напрямую — содержательная проверка повторной очистки сохраняется и не
        // подменяется безусловным пропуском.
        eprintln!(
            "запрет прав 0o500 не действует (например, права root): отказ первого прохода невоспроизводим, состояние после него собрано напрямую"
        );
        fs::set_permissions(&owned_record, original).unwrap();
        fs::remove_dir_all(&owned).unwrap();
        assert!(owned_record.join("gitdir").is_file());
    }

    // Повторная очистка обязана убрать оставшуюся запись в обоих случаях.
    assert_repeated_cleanup_removes_record(&fixture, &job, &owned_record);

    // Чужие записи, ресурсы и целевой HEAD не тронуты.
    assert!(foreign_worktree.is_dir());
    assert_eq!(git(&foreign_worktree, &["rev-parse", "HEAD"]), fixture.head);
    assert_eq!(git(fixture.root(), &["rev-parse", "HEAD"]), fixture.head);
    let worktrees = git(fixture.root(), &["worktree", "list", "--porcelain"]);
    assert!(
        worktrees.contains(foreign_worktree.to_str().unwrap()),
        "{worktrees}"
    );
}
