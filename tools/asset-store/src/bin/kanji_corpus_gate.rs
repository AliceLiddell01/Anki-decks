use std::path::Path;

use asset_store::AssetStore;

fn main() {
    match AssetStore::verify_publishable_corpus(Path::new(".asset-store/kanji")) {
        Ok(()) => println!("проверка готовности корпуса к публикации прошла"),
        Err(error) => {
            eprintln!("проверка готовности корпуса к публикации не пройдена: {error}");
            std::process::exit(1);
        }
    }
}
