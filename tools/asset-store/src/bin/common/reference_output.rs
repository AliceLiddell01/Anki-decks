//! Публикация готового сжатого эталона рядом с выходным файлом.
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temporary_path(output: &Path, sequence: u64) -> PathBuf {
    output.with_added_extension(format!("tmp-{}-{sequence}", std::process::id()))
}

fn create_temporary_from(output: &Path, mut sequence: u64) -> io::Result<(PathBuf, File)> {
    loop {
        let temp = temporary_path(output, sequence);
        match File::create_new(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                sequence = sequence.wrapping_add(1);
            }
            Err(error) => return Err(error),
        }
    }
}

fn create_temporary(output: &Path) -> io::Result<(PathBuf, File)> {
    create_temporary_from(output, TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed))
}

pub fn publish(output: &Path, bytes: &[u8]) -> io::Result<()> {
    // Уникальное имя переживает остатки прежнего запуска; create_new защищает
    // от перезаписи чужого файла и разрешает параллельным процессам работать.
    let (temp, mut file) = create_temporary(output)?;
    let outcome = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let outcome = outcome.and_then(|()| fs::rename(&temp, output));
    if outcome.is_err()
        && let Err(cleanup_error) = fs::remove_file(&temp)
    {
        eprintln!(
            "не удалось удалить временный файл {} после сбоя публикации: {cleanup_error}",
            temp.display()
        );
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;

    fn temporary_files_for(output: &Path) -> Vec<PathBuf> {
        let parent = output.parent().unwrap_or(Path::new("."));
        let prefix = format!("{}.tmp-", output.file_name().unwrap().to_string_lossy());
        fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .collect()
    }

    #[test]
    fn publication_preserves_the_complete_output_name_and_replaces_bytes() {
        let dir = TempWorkspace::create("reference-output-success").unwrap();
        for name in ["maskdb.zlib", "maskdb.backup.zlib", "maskdb", "日本語.bin"] {
            let output = dir.path().join(name);
            fs::write(&output, b"previous").unwrap();
            publish(&output, b"compressed reference").unwrap();
            assert_eq!(fs::read(&output).unwrap(), b"compressed reference");
            assert!(temporary_files_for(&output).is_empty());
        }
    }

    #[test]
    fn failed_rename_cleans_only_its_temporary_file() {
        let dir = TempWorkspace::create("reference-output-failure").unwrap();
        let output = dir.path().join("maskdb.zlib");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("keep"), b"untouched").unwrap();
        assert!(publish(&output, b"compressed").is_err());
        assert!(temporary_files_for(&output).is_empty());
        assert_eq!(fs::read(output.join("keep")).unwrap(), b"untouched");
    }

    #[test]
    fn stale_legacy_temp_does_not_block_publication_or_get_overwritten() {
        let dir = TempWorkspace::create("reference-output-stale-temp").unwrap();
        let output = dir.path().join("maskdb.zlib");
        let stale = output.with_added_extension("tmp");
        fs::write(&stale, b"interrupted writer").unwrap();

        publish(&output, b"new reference").unwrap();

        assert_eq!(fs::read(&output).unwrap(), b"new reference");
        assert_eq!(fs::read(stale).unwrap(), b"interrupted writer");
        assert!(temporary_files_for(&output).is_empty());
    }

    #[test]
    fn occupied_temporary_names_are_skipped_without_changes() {
        let dir = TempWorkspace::create("reference-output-occupied").unwrap();
        let output = dir.path().join("maskdb.zlib");
        let first = temporary_path(&output, 0);
        let second = temporary_path(&output, 1);
        let third = temporary_path(&output, 2);
        fs::write(&output, b"previous").unwrap();
        fs::write(&first, b"other writer").unwrap();
        std::os::unix::fs::symlink(&output, &second).unwrap();

        let (created, file) = create_temporary_from(&output, 0).unwrap();
        assert_eq!(created, third);
        drop(file);
        fs::remove_file(&created).unwrap();

        assert_eq!(fs::read(&output).unwrap(), b"previous");
        assert_eq!(fs::read(&first).unwrap(), b"other writer");
        assert!(
            fs::symlink_metadata(&second)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let remaining = temporary_files_for(&output);
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&first));
        assert!(remaining.contains(&second));
        assert!(!third.exists());
    }
}
