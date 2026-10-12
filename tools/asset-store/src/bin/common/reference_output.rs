//! Публикация готового сжатого эталона рядом с выходным файлом.
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

pub fn publish(output: &Path, bytes: &[u8]) -> io::Result<()> {
    let temp = output.with_added_extension("tmp");
    // Не перезаписываем чужой временный файл или цель символической ссылки.
    let mut file = File::create_new(&temp)?;
    let outcome = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let outcome = outcome.and_then(|()| fs::rename(&temp, output));
    if outcome.is_err() {
        fs::remove_file(&temp)?;
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use asset_store::temp_workspace::TempWorkspace;

    #[test]
    fn publication_preserves_the_complete_output_name_and_replaces_bytes() {
        let dir = TempWorkspace::create("reference-output-success").unwrap();
        for name in ["maskdb.zlib", "maskdb.backup.zlib", "maskdb", "日本語.bin"] {
            let output = dir.path().join(name);
            fs::write(&output, b"previous").unwrap();
            publish(&output, b"compressed reference").unwrap();
            assert_eq!(fs::read(&output).unwrap(), b"compressed reference");
            assert!(!dir.path().join(format!("{name}.tmp")).exists());
        }
    }

    #[test]
    fn failed_rename_cleans_only_its_temporary_file() {
        let dir = TempWorkspace::create("reference-output-failure").unwrap();
        let output = dir.path().join("maskdb.zlib");
        fs::create_dir(&output).unwrap();
        fs::write(output.join("keep"), b"untouched").unwrap();
        assert!(publish(&output, b"compressed").is_err());
        assert!(!output.with_added_extension("tmp").exists());
        assert_eq!(fs::read(output.join("keep")).unwrap(), b"untouched");
    }

    #[test]
    fn occupied_temp_and_symlink_are_refused_without_changes() {
        let dir = TempWorkspace::create("reference-output-occupied").unwrap();
        let output = dir.path().join("maskdb.zlib");
        let temp = output.with_added_extension("tmp");
        fs::write(&output, b"previous").unwrap();
        fs::write(&temp, b"other writer").unwrap();
        assert_eq!(
            publish(&output, b"new").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&temp).unwrap(), b"other writer");
        fs::remove_file(&temp).unwrap();
        std::os::unix::fs::symlink(&output, &temp).unwrap();
        assert_eq!(
            publish(&output, b"new").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&output).unwrap(), b"previous");
        assert!(fs::symlink_metadata(temp).unwrap().file_type().is_symlink());
    }
}
