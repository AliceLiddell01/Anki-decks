//! Межпроцессная проверка настоящего пути атомарной публикации.
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::common::TempDir;
use anki_repo::error::ErrorCode;
use anki_repo::write::{ExportLock, replace_atomically};

const SIZE: usize = 1024 * 1024;

#[test]
fn writer_process() {
    let Some(path) = std::env::var_os("ANKI_LOCK_TEST_PATH") else {
        return;
    };
    let byte: u8 = std::env::var("ANKI_LOCK_TEST_BYTE")
        .unwrap()
        .parse()
        .unwrap();
    // Родитель уже держит production ExportLock: оба writer читают один snapshot.
    let expected = fs::read(&path).unwrap();
    println!("WRITER_READY");
    std::io::stdout().flush().unwrap();
    match replace_atomically(std::path::Path::new(&path), &expected, &vec![byte; SIZE]) {
        Ok(published) => {
            assert_eq!(published.bytes, SIZE);
            println!("WRITER_PUBLISHED");
        }
        Err(error) => {
            assert_eq!(error.code, ErrorCode::SourceChanged);
            assert_eq!(error.details["reason"], "source_modified");
            assert_eq!(error.exit_code(), 7);
            println!("WRITER_CONFLICT");
        }
    }
}

#[test]
fn independent_writers_wait_then_publish_exactly_one_complete_candidate() {
    let dir = TempDir::new("write-process-lock");
    let path = dir.path().join("deck.json");
    fs::write(&path, vec![b'S'; SIZE]).unwrap();
    let holder = ExportLock::acquire(dir.path()).unwrap();
    let mut children = Vec::new();
    for byte in *b"AB" {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "write_lock_contract::writer_process",
                "--nocapture",
            ])
            .env("ANKI_LOCK_TEST_PATH", &path)
            .env("ANKI_LOCK_TEST_BYTE", byte.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "writer завершился до READY"
            );
            if line.contains("WRITER_READY") {
                break;
            }
            line.clear();
        }
        assert!(child.try_wait().unwrap().is_none());
        children.push((child, output));
    }
    assert_eq!(fs::read(&path).unwrap(), vec![b'S'; SIZE]);
    let contender = fs::File::open(dir.path()).unwrap();
    std::assert_matches!(contender.try_lock(), Err(std::fs::TryLockError::WouldBlock));

    let done = Arc::new(AtomicBool::new(false));
    let reader_done = done.clone();
    let reader_path = path.clone();
    let reader = std::thread::spawn(move || {
        let mut reads = 0;
        loop {
            let bytes = fs::read(&reader_path).unwrap();
            assert_eq!(bytes.len(), SIZE, "частичная публикация");
            assert!(matches!(bytes[0], b'S' | b'A' | b'B'));
            assert!(
                bytes.iter().all(|byte| *byte == bytes[0]),
                "смешанные байты writers"
            );
            reads += 1;
            if reader_done.load(Ordering::Acquire) {
                return reads;
            }
            std::thread::yield_now();
        }
    });
    drop(holder);
    let mut published = 0;
    let mut conflicted = 0;
    for (child, mut reader) in children {
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let mut output = String::new();
        reader.read_to_string(&mut output).unwrap();
        published += usize::from(output.contains("WRITER_PUBLISHED"));
        conflicted += usize::from(output.contains("WRITER_CONFLICT"));
    }
    done.store(true, Ordering::Release);
    assert!(reader.join().unwrap() > 0);
    assert_eq!((published, conflicted), (1, 1));
    let bytes = fs::read(path).unwrap();
    assert!(bytes == vec![b'A'; SIZE] || bytes == vec![b'B'; SIZE]);
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "нет временных остатков"
    );
    contender
        .try_lock()
        .expect("Drop освободил блокировку каталога");
}
