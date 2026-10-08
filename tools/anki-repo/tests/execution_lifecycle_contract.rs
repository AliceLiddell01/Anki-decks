//! Жизненный цикл выполнения CLI на независимых синтетических Git-репозиториях.

use std::fs;
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
        Self { temp, pack }
    }

    fn root(&self) -> &Path {
        self.temp.path()
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
fn failures_before_spawn_leave_proven_not_started_job_inspectable() {
    for surface in ["logs/stdout.log", "home"] {
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
        fs::remove_file(path).unwrap();
        if surface == "home" {
            fs::create_dir(job.join("home")).unwrap();
        }
        let result = success(fixture.operation("cleanup", &job));
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
    let first_run = ActiveRun::spawn(&fixture, &first, "2");
    let second_run = ActiveRun::spawn(&fixture, &second, "2");
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
    assert_eq!(code, 2, "{stdout}");
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
        result["stderr"]["text"]
            .as_str()
            .unwrap()
            .contains("no matching package named")
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
