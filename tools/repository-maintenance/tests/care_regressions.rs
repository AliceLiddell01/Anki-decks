//! Изолированные проверки настоящих путей диагностики, таймера и блокировок.
use asset_store::temp_workspace::TempWorkspace;
use repository_maintenance::{care, timer};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const HELPER: &str = "care_scenario_helper";

#[test]
fn care_scenario_helper() {
    let Ok(action) = std::env::var("CARE_SCENARIO") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("CARE_ROOT").unwrap());
    let value = match action.as_str() {
        "inspect" => serde_json::to_value(care::inspect(&root)).unwrap(),
        "text" => json!(care::render_text(&care::inspect(&root))),
        "timer" => {
            json!({"inspection": format!("{:?}",timer::inspect()), "result": timer::status().map(|v| json!(v))})
        }
        "enable" => json!(care::enable(&root)),
        "disable" => json!(care::disable(&root)),
        "yes" => json!(care::set_choice("yes")),
        "no" => json!(care::set_choice("no")),
        "record-failure" => json!(care::record_failure(&root, "проверяемая ошибка")),
        "record" => {
            let policy = repository_maintenance::policy::Policy::defaults().unwrap();
            let opts = repository_maintenance::RunOptions {
                apply: true,
                mode: "apply",
                temp_root: PathBuf::from(std::env::var_os("TMPDIR").unwrap()),
                ..Default::default()
            };
            let (result, snapshot) = repository_maintenance::run_and_record(&root, &policy, &opts);
            json!({"result":result.map(|v| json!(v)), "snapshot_error": snapshot})
        }
        _ => panic!("неизвестный сценарий"),
    };
    fs::write(
        std::env::var_os("CARE_OUTPUT").unwrap(),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}

struct Harness {
    owner: TempWorkspace,
    root: PathBuf,
}
impl Harness {
    fn new() -> Self {
        let owner = TempWorkspace::create("care-regressions").unwrap();
        let root = owner.path().join("checkout with ' quote");
        for name in ["home", "config", "cache", "state", "bin", "tmp"] {
            fs::create_dir_all(owner.path().join(name)).unwrap();
        }
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\nmembers=[]\n").unwrap();
        let this = Self { owner, root };
        this.script("cargo", r#"#!/bin/sh
case "$1" in
--version) echo 'cargo 1.99.0';;
metadata) printf '{"workspace_root":"%s","target_directory":"%s/target"}\n' "$CARE_ROOT" "$CARE_ROOT";;
*) exit 99;;
esac
"#);
        // Не допускаем ни PATH хоста, ни fallback: powershell.exe всегда подменён.
        fs::copy(
            env!("CARGO_BIN_EXE_anki-repository-maintenance"),
            this.path("bin/anki-repository-maintenance"),
        )
        .unwrap();
        this.script("powershell.exe", "#!/bin/sh\nexit 19\n");
        this.script("notify-send", "#!/bin/sh\nexit 19\n");
        this.script("systemctl", r#"#!/bin/sh
printf '%s\n' "$*" >> "$CARE_SANDBOX/calls"
if [ -f "$CARE_SANDBOX/unavailable" ]; then exit 5; fi
if [ "$1 $2" = 'is-active systemd-tmpfiles-clean.timer' ]; then
 if [ -f "$CARE_SANDBOX/barrier-gc" ]; then
   : > "$CARE_SANDBOX/entered"
   while [ ! -f "$CARE_SANDBOX/release" ]; do /bin/sleep 0.02; done
 fi
 exit 3
fi
verb="$2"
case "$verb" in
show-environment) exit 0;;
is-enabled)
 if [ -f "$CARE_SANDBOX/query-error" ]; then echo nonsense; exit 5; fi
 if [ -f "$CARE_SANDBOX/enabled" ]; then echo enabled; exit 0; fi
 echo disabled; exit 1;;
is-active)
 if [ -f "$CARE_SANDBOX/active" ]; then echo active; exit 0; fi
 echo inactive; exit 3;;
show)
 name="$3"
 fragment="$XDG_CONFIG_HOME/systemd/user/$name"
 if [ -f "$CARE_SANDBOX/foreign" ]; then fragment=/foreign/unit; fi
 if [ -f "$XDG_CONFIG_HOME/systemd/user/$name" ]; then
   echo LoadState=loaded
   echo "FragmentPath=$fragment"
 else echo LoadState=not-found; echo FragmentPath=; fi
 if [ "$name" = anki-repository-maintenance.service ] && [ -f "$CARE_SANDBOX/service-active" ]; then echo ActiveState=active; else echo ActiveState=inactive; fi
 if [ -f "$CARE_SANDBOX/dropin" ]; then echo DropInPaths=/foreign/override; else echo DropInPaths=; fi
 if [ -f "$CARE_SANDBOX/stale-loaded" ]; then echo NeedDaemonReload=yes; else echo NeedDaemonReload=no; fi
 if [ -f "$CARE_SANDBOX/service-failure" ]; then echo Result=exit-code; else echo Result=success; fi
 execpath="$HOME/.local/bin/anki-repository-maintenance"
 if [ -f "$CARE_SANDBOX/wrong-exec" ]; then execpath=/foreign/binary; fi
 echo "ExecStart={ path=$execpath ; argv[]=$execpath clean --apply --installed-root ; ignore_errors=no ; }"
 ;;
enable|disable)
 if [ -f "$CARE_SANDBOX/barrier-$verb" ]; then
   : > "$CARE_SANDBOX/entered"
   while [ ! -f "$CARE_SANDBOX/release" ]; do /bin/sleep 0.02; done
 fi
 if [ -f "$CARE_SANDBOX/fail-$verb" ]; then exit 9; fi
 if [ "$verb" = enable ] && [ -f "$CARE_SANDBOX/enable-noop" ]; then exit 0; fi
 if [ "$verb" = enable ]; then : > "$CARE_SANDBOX/enabled"; : > "$CARE_SANDBOX/active";
 elif [ ! -f "$CARE_SANDBOX/stays-active" ]; then /bin/rm -f "$CARE_SANDBOX/enabled" "$CARE_SANDBOX/active"; fi
 ;;
stop) /bin/rm -f "$CARE_SANDBOX/service-active";;
daemon-reload|status) exit 0;;
*) exit 99;;
esac
"#);
        this
    }
    fn path(&self, name: &str) -> PathBuf {
        self.owner.path().join(name)
    }
    fn script(&self, name: &str, content: &str) {
        let path = self.path("bin").join(name);
        fs::write(&path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn flag(&self, name: &str, yes: bool) {
        if yes {
            fs::write(self.path(name), "").unwrap();
        } else {
            let _ = fs::remove_file(self.path(name));
        }
    }
    fn command(&self, action: &str, output: &Path) -> Command {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", HELPER, "--nocapture"])
            .env_clear()
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_CACHE_HOME", self.path("cache"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("TMPDIR", self.path("tmp"))
            .env("PATH", self.path("bin"))
            .env("CARE_SANDBOX", self.owner.path())
            .env("CARE_SCENARIO", action)
            .env("CARE_ROOT", &self.root)
            .env("CARE_OUTPUT", output);
        c
    }
    fn invoke(&self, action: &str) -> Value {
        let output = self.path("output.json");
        let result = self.command(action, &output).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice(&fs::read(output).unwrap()).unwrap()
    }
    fn enable(&self) {
        assert_eq!(self.invoke("enable"), json!({"Ok":null}));
    }
    fn status(&self) -> Value {
        self.invoke("inspect")
    }
}
fn diagnostic<'a>(v: &'a Value, code: &str) -> &'a Value {
    let d = v["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == code)
        .unwrap_or_else(|| panic!("нет {code}: {v}"));
    let severity = match code {
        "setup_required" | "gc_deferred" | "system_tmp_inventory_incomplete" => "info",
        "preference_unavailable"
        | "systemd_service_failure"
        | "policy_unavailable"
        | "gc_operation_error" => "error",
        "last_gc_problem" => "info", // Здесь вызывается для известного partial, а не для failed.
        _ => "warning",
    };
    assert_eq!(d["severity"], severity, "{d}");
    assert!(d["context"]["workspace_root"].is_string(), "{d}");
    match code {
        "policy_unavailable" | "last_attempt_unavailable" | "gc_deferred" => {
            assert!(d["action"].is_null())
        }
        "setup_required" | "preference_mismatch" => {
            let action = d["action"].as_str().unwrap();
            assert!(action.starts_with("anki-repo maintenance --workspace-root"));
            assert!(action.ends_with(if v["preference"] == "no" {
                "disable"
            } else {
                "enable"
            }));
        }
        "measurement_unavailable"
        | "gc_operation_error"
        | "system_tmp_inventory_incomplete"
        | "last_gc_problem" => assert!(
            d["action"]
                .as_str()
                .unwrap()
                .starts_with("anki-repository-maintenance scan --workspace-root")
        ),
        _ => assert!(d["action"].is_string(), "{d}"),
    }
    d
}
fn wait_file(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "барьер не достигнут"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn timer_fact_matrix_and_both_status_interfaces() {
    let h = Harness::new();
    h.enable();
    for enabled in [false, true] {
        for active in [false, true] {
            h.flag("enabled", enabled);
            h.flag("active", active);
            let s = h.status();
            assert_eq!(s["timer_enabled"], enabled);
            assert_eq!(s["timer_active"], active);
            assert_eq!(
                s["automatic"],
                if enabled || active {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            let old = h.invoke("timer");
            assert_eq!(old["result"]["Ok"]["state"], s["automatic"]);
            let mismatch = s["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == "preference_mismatch");
            assert_eq!(mismatch, !(enabled && active));
        }
    }
    h.invoke("no");
    h.flag("active", true);
    h.flag("enabled", false);
    assert_eq!(
        diagnostic(&h.status(), "preference_mismatch")["severity"],
        "warning"
    );
    h.flag("stays-active", true);
    assert!(h.invoke("disable")["Err"].is_string());
    assert!(
        h.path("config/systemd/user/anki-repository-maintenance.timer")
            .exists()
    );
    h.flag("query-error", true);
    assert_eq!(h.status()["automatic"], "unknown");
    assert!(h.invoke("timer")["result"]["Err"].is_string());
    h.flag("unavailable", true);
    assert_eq!(h.status()["automatic"], "unavailable");
    assert!(h.invoke("disable")["Err"].is_string());
}

#[test]
fn disable_cannot_save_no_when_timer_or_service_remains_active() {
    let h = Harness::new();
    h.enable();
    h.flag("enabled", false);
    h.flag("stays-active", true);
    h.flag("service-active", true);
    assert!(h.invoke("disable")["Err"].is_string());
    assert_eq!(h.status()["preference"], "yes");
    h.flag("stays-active", false);
    assert_eq!(h.invoke("disable"), json!({"Ok":null}));
    let s = h.status();
    assert_eq!(s["preference"], "no");
    assert_eq!(s["automatic"], "disabled");
    assert_eq!(s["service_active"], false);
}

#[test]
fn binding_rejects_foreign_exec_units_symlinks_and_missing_config() {
    for change in [
        "foreign",
        "wrong-exec",
        "missing-config",
        "changed-unit",
        "symlink",
        "missing-binary",
        "dropin",
        "stale-loaded",
    ] {
        let h = Harness::new();
        h.enable();
        let service = h.path("config/systemd/user/anki-repository-maintenance.service");
        match change {
            "missing-config" => {
                fs::remove_file(h.path("config/anki-decks/repository-maintenance-install.toml"))
                    .unwrap();
            }
            "missing-binary" => {
                fs::remove_file(h.path("home/.local/bin/anki-repository-maintenance")).unwrap();
            }
            "changed-unit" => {
                let text = fs::read_to_string(&service).unwrap().replace(
                    "clean --apply --installed-root",
                    "clean --apply --workspace-root /foreign",
                );
                fs::write(&service, text).unwrap();
            }
            "symlink" => {
                let target = h.path("foreign.service");
                fs::rename(&service, &target).unwrap();
                std::os::unix::fs::symlink(target, &service).unwrap();
            }
            _ => h.flag(change, true),
        }
        assert_eq!(h.status()["automatic"], "unknown", "{change}");
        let before = fs::read_to_string(h.path("calls")).unwrap();
        assert!(h.invoke("disable")["Err"].is_string(), "{change}");
        if change != "missing-binary" {
            assert!(h.invoke("enable")["Err"].is_string(), "{change}");
        }
        let calls = fs::read_to_string(h.path("calls")).unwrap();
        assert!(
            !calls[before.len()..].contains("--user disable"),
            "{change}"
        );
        assert!(!calls[before.len()..].contains("--user enable"), "{change}");
        assert!(fs::symlink_metadata(&service).is_ok());
        if change == "missing-binary" {
            h.enable();
        }
    }
}

#[test]
fn enable_disable_are_serialized_with_deterministic_barriers() {
    for first in ["enable", "disable"] {
        let h = Harness::new();
        h.enable();
        h.flag(&format!("barrier-{first}"), true);
        let out = h.path("first.json");
        let mut child = h.command(first, &out).spawn().unwrap();
        wait_file(&h.path("entered"));
        let second = if first == "enable" {
            "disable"
        } else {
            "enable"
        };
        assert!(
            h.invoke(second)["Err"]
                .as_str()
                .unwrap()
                .contains("блокировка")
        );
        h.flag("release", true);
        assert!(child.wait().unwrap().success());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(out).unwrap()).unwrap(),
            json!({"Ok":null})
        );
        assert_eq!(
            h.status()["preference"],
            if first == "enable" { "yes" } else { "no" }
        );
        assert_eq!(
            h.status()["automatic"],
            if first == "enable" {
                "enabled"
            } else {
                "disabled"
            }
        );
        if second == "enable" {
            h.enable();
        } else {
            assert_eq!(h.invoke("disable"), json!({"Ok":null}));
        }
    }
}

#[test]
fn diagnostic_real_conditions_and_retry_after_installation_failure() {
    let mut h = Harness::new();
    let unset = h.status();
    assert_eq!(diagnostic(&unset, "setup_required")["severity"], "info");
    let action = diagnostic(&unset, "setup_required")["action"]
        .as_str()
        .unwrap();
    assert!(action.contains("--workspace-root"));
    assert!(action.contains("'\\''"));
    h.flag("fail-enable", true);
    assert!(h.invoke("enable")["Err"].is_string());
    let failure = h.status();
    assert_eq!(failure["preference"], "yes");
    assert_eq!(
        diagnostic(&failure, "preference_mismatch")["severity"],
        "warning"
    );
    h.flag("fail-enable", false);
    h.enable();
    let good = h.status();
    assert!(
        !good["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "preference_mismatch")
    );
    h.flag("service-failure", true);
    assert_eq!(
        diagnostic(&h.status(), "systemd_service_failure")["severity"],
        "error"
    );
    let original = h.root.clone();
    h.root = h.owner.path().join("second-checkout");
    fs::create_dir_all(&h.root).unwrap();
    fs::write(h.root.join("Cargo.toml"), "[workspace]\n").unwrap();
    let second = h.status();
    assert_eq!(second["automatic"], "other_workspace");
    diagnostic(&second, "timer_other_workspace");
    assert!(h.invoke("disable")["Err"].is_string());
    assert!(h.invoke("enable")["Err"].is_string());
    h.root = original;
    h.flag("unavailable", true);
    diagnostic(&h.status(), "timer_unavailable");
    h.flag("unavailable", false);
    fs::write(h.path("config/anki-decks/gc-preference.json"), "invalid").unwrap();
    diagnostic(&h.status(), "preference_unavailable");
    fs::create_dir_all(h.path("state/anki-decks")).unwrap();
    let snapshot = h.path("state/anki-decks/gc-last-attempt.json");
    fs::write(&snapshot, "invalid").unwrap();
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600)).unwrap();
    diagnostic(&h.status(), "last_attempt_unavailable");
    h.script("cargo", "#!/bin/sh\nexit 18\n");
    diagnostic(&h.status(), "measurement_unavailable");
}

#[test]
fn status_is_read_only_including_mtime_and_permissions() {
    let h = Harness::new();
    h.enable();
    let path = h.path("config/anki-decks/gc-preference.json");
    let before = fs::metadata(&path).unwrap();
    let bytes = fs::read(&path).unwrap();
    let first = h.status();
    let second = h.status();
    assert_eq!(first["preference"], second["preference"]);
    let after = fs::metadata(&path).unwrap();
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_eq!(before.mode(), after.mode());
    assert_eq!(bytes, fs::read(&path).unwrap());
    assert!(!h.path("state/anki-decks/gc-last-attempt.json").exists());
    let empty = Harness::new();
    empty.status();
    assert!(!empty.path("config/anki-decks").exists());
    assert!(!empty.path("cache/anki-decks").exists());
}

#[test]
fn gc_snapshot_is_atomic_and_rejected_contender_preserves_previous_attempt() {
    let mut h = Harness::new();
    assert!(h.invoke("record")["result"]["Ok"].is_object());
    let snapshot = h.path("state/anki-decks/gc-last-attempt.json");
    let newer = h.owner.path().join("new-checkout");
    fs::create_dir_all(&newer).unwrap();
    fs::write(newer.join("Cargo.toml"), "[workspace]\n").unwrap();
    h.root = newer.clone();

    // Новый CLI удерживает lock вплоть до snapshot. Конкурентная попытка не
    // выполняет GC и не перезаписывает результат уже выполняющегося владельца.
    h.flag("barrier-gc", true);
    let before = fs::read(&snapshot).unwrap();
    let output = h.path("atomic.json");
    let mut first = h.command("record", &output).spawn().unwrap();
    wait_file(&h.path("entered"));
    let blocked = h.invoke("record");
    assert!(
        blocked["result"]["Err"]
            .as_str()
            .unwrap()
            .contains("блокировк")
    );
    assert_eq!(before, fs::read(&snapshot).unwrap());
    h.flag("release", true);
    assert!(first.wait().unwrap().success());
    let stored: Value = serde_json::from_slice(&fs::read(&snapshot).unwrap()).unwrap();
    assert_eq!(stored["workspace_root"], json!(newer));
    let status = h.status();
    assert!(
        status["last_attempt"].is_object(),
        "точный сохранённый путь сопоставляется с checkout"
    );
    assert!(
        !status["workspace_root"]
            .as_str()
            .unwrap()
            .contains(&format!(
                "anki-decks-{}",
                fs::metadata("/proc/self").unwrap().uid()
            ))
    );
}

#[test]
fn foreign_tmp_eacces_is_partial_without_fatal_gc_error() {
    let h = Harness::new();
    let foreign = h.path("tmp/foreign-private");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("data"), [1; 4096]).unwrap();
    fs::set_permissions(&foreign, fs::Permissions::from_mode(0o000)).unwrap();
    let result = h.invoke("record");
    fs::set_permissions(&foreign, fs::Permissions::from_mode(0o700)).unwrap();
    let report = &result["result"]["Ok"];
    assert_eq!(report["fatal_errors"], 0);
    if fs::metadata("/proc/self").unwrap().uid() != 0 {
        assert!(report["errors"].as_u64().unwrap() > 0);
        let status = h.status();
        let last = &status["last_attempt"];
        assert_eq!(last["outcome"], "partial");
        assert_eq!(last["fatal_errors"], 0);
        assert_eq!(last["managed_inventory_complete"], true);
        diagnostic(&status, "last_gc_problem");
    }
    assert!(foreign.join("data").exists(), "чужие данные не удаляются");
}

#[test]
fn recommendations_execute_from_another_checkout_and_preserve_quoted_root() {
    let h = Harness::new();
    h.script("cargo", "#!/bin/sh\nexit 18\n");
    let status = h.status();
    let action = diagnostic(&status, "measurement_unavailable")["action"]
        .as_str()
        .unwrap();
    h.script(
        "cargo",
        r#"#!/bin/sh
printf '{"workspace_root":"%s","target_directory":"%s/target"}\n' "$CARE_ROOT" "$CARE_ROOT"
"#,
    );
    let second = h.path("different-checkout");
    fs::create_dir_all(&second).unwrap();
    fs::write(second.join("Cargo.toml"), "[workspace]\n").unwrap();
    let env = h.command("inspect", &h.path("unused"));
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(action)
        .current_dir(second)
        .env_clear();
    for (key, value) in env.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    let result = command.output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(report["workspace_root"], status["workspace_root"]);
}

#[test]
fn missing_gc_binary_keeps_consent_and_can_be_retried() {
    let h = Harness::new();
    let binary = h.path("bin/anki-repository-maintenance");
    fs::remove_file(&binary).unwrap();
    assert!(
        h.invoke("enable")["Err"]
            .as_str()
            .unwrap()
            .contains("не найден бинарник")
    );
    assert_eq!(h.status()["preference"], "yes");
    fs::copy(env!("CARGO_BIN_EXE_anki-repository-maintenance"), binary).unwrap();
    h.enable();
}

#[test]
fn successful_systemctl_exit_without_activation_is_not_successful_install() {
    let h = Harness::new();
    h.flag("enable-noop", true);
    assert!(
        h.invoke("enable")["Err"]
            .as_str()
            .unwrap()
            .contains("не подтверждён")
    );
    assert_eq!(h.status()["preference"], "yes");
    assert_eq!(h.status()["timer_active"], false);
    h.flag("enable-noop", false);
    h.enable();
}

#[test]
fn gc_snapshot_and_exit_survive_notification_timeout_and_lock_is_already_released() {
    use std::process::Stdio;
    let h = Harness::new();
    fs::create_dir_all(h.root.join("target")).unwrap();
    fs::write(h.root.join("target/data"), [9; 4096]).unwrap();
    let mut policy = repository_maintenance::policy::Policy::defaults().unwrap();
    policy.target_warning_bytes = Some(1);
    policy.target_hard_limit_bytes = Some(2);
    let policy_file = h.path("policy.toml");
    fs::write(&policy_file, toml::to_string(&policy).unwrap()).unwrap();
    h.script(
        "powershell.exe",
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$CARE_SANDBOX/notify-pid\"\nexec /bin/sleep 60\n",
    );
    let env = h.command("inspect", &h.path("unused"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_anki-repository-maintenance"));
    command
        .args(["clean", "--apply", "--json", "--workspace-root"])
        .arg(&h.root)
        .arg("--policy")
        .arg(policy_file)
        .arg("--temp-root")
        .arg(h.path("tmp"))
        .env_clear()
        .env("WSL_DISTRO_NAME", "synthetic-wsl")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env.get_envs() {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
    let started = Instant::now();
    let child = command.spawn().unwrap();
    wait_file(&h.path("notify-pid"));
    let snapshot = h.path("state/anki-decks/gc-last-attempt.json");
    let before = fs::read(&snapshot).unwrap();
    h.enable(); // Уведомление ещё спит, но общая блокировка уже освобождена.
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(started.elapsed() < Duration::from_secs(9));
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(report["fatal_errors"].as_u64().unwrap() > 0);
    assert_eq!(before, fs::read(snapshot).unwrap());
    assert_eq!(
        serde_json::from_slice::<Value>(&before).unwrap()["outcome"],
        "failed"
    );
    assert!(h.root.join("target/data").exists());
    let pid = fs::read_to_string(h.path("notify-pid")).unwrap();
    assert!(!Path::new("/proc").join(pid).exists());
}

#[test]
fn busy_lock_defers_without_alert_but_invalid_lock_directory_alerts() {
    let h = Harness::new();
    h.script(
        "powershell.exe",
        "#!/bin/sh\nprintf '%s' delivered > \"$CARE_SANDBOX/alert\"\nexit 0\n",
    );
    let directory = h.path("cache/anki-decks/maintenance-lock");
    fs::create_dir_all(directory.parent().unwrap()).unwrap();
    let lock = repository_maintenance::guard::MaintenanceLock::acquire_at(&directory).unwrap();
    let invoke = || {
        let env = h.command("inspect", &h.path("unused"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_anki-repository-maintenance"));
        command
            .args(["clean", "--apply", "--json", "--workspace-root"])
            .arg(&h.root)
            .arg("--temp-root")
            .arg(h.path("tmp"))
            .env_clear()
            .env("WSL_DISTRO_NAME", "synthetic-wsl");
        for (key, value) in env.get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command.output().unwrap()
    };
    let deferred = invoke();
    assert_eq!(deferred.status.code(), Some(2));
    let json: Value = serde_json::from_slice(&deferred.stdout).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .starts_with("очистка отложена:")
    );
    assert!(!h.path("alert").exists());
    assert!(!h.path("state/anki-decks/gc-last-attempt.json").exists());
    drop(lock);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    let failed = invoke();
    assert_eq!(failed.status.code(), Some(2));
    let json: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert!(
        !json["error"]
            .as_str()
            .unwrap()
            .starts_with("очистка отложена:")
    );
    assert!(
        h.path("alert").exists(),
        "ошибка безопасности блокировки остаётся существенным сбоем"
    );
    assert!(!h.path("state/anki-decks/gc-last-attempt.json").exists());
}

#[test]
fn non_utf8_checkout_is_diagnosed_without_panic_or_lossy_snapshot() {
    use std::os::unix::ffi::OsStringExt;
    let mut h = Harness::new();
    assert_eq!(h.invoke("record-failure"), json!({"Ok":null}));
    let snapshot = h.path("state/anki-decks/gc-last-attempt.json");
    let before = fs::read(&snapshot).unwrap();
    let root = h
        .path("")
        .join(std::ffi::OsString::from_vec(b"checkout-\xff".to_vec()));
    fs::rename(&h.root, &root).unwrap();
    h.root = root;
    let status = h.status();
    assert!(
        status["workspace_root"]
            .as_str()
            .unwrap()
            .contains('\u{fffd}')
    );
    let setup = status["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "setup_required")
        .unwrap();
    assert!(setup["action"].is_null());
    for d in status["diagnostics"].as_array().unwrap() {
        assert!(
            d["context"]["workspace_root"]
                .as_str()
                .unwrap()
                .contains('\u{fffd}')
        );
        assert!(
            !d["action"]
                .as_str()
                .is_some_and(|a| a.starts_with("anki-repo") && a.contains("--workspace-root"))
        );
    }
    assert!(
        h.invoke("record-failure")["Err"]
            .as_str()
            .unwrap()
            .contains("UTF-8")
    );
    assert_eq!(
        fs::read(snapshot).unwrap(),
        before,
        "ошибка сериализации сохраняет прежний точный snapshot"
    );
}
