use std::path::Path;

use asset_store::AssetStore;
use asset_store::kanji_validator::KanjiImageValidator;

fn main() {
    match AssetStore::verify_publishable_corpus(
        Path::new(".asset-store/kanji"),
        &KanjiImageValidator::validator_identity(),
    ) {
        Ok(()) => println!("проверка готовности корпуса к публикации прошла"),
        Err(error) => {
            eprintln!("проверка готовности корпуса к публикации не пройдена: {error}");
            std::process::exit(1);
        }
    }
}
