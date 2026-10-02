use std::path::Path;

use asset_store::{AssetStore, PitchAccentDomainPolicy, PitchAccentImageValidator};

fn main() {
    match AssetStore::verify_publishable_corpus_with_policy(
        Path::new(".asset-store/pitch-accent"),
        &PitchAccentDomainPolicy,
        &PitchAccentImageValidator::validator_identity(),
    ) {
        Ok(()) => println!("проверка pitch-accent корпуса к публикации прошла"),
        Err(error) => {
            eprintln!("проверка pitch-accent корпуса к публикации не пройдена: {error}");
            std::process::exit(1);
        }
    }
}
