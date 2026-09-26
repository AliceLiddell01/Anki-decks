//! Тонкий CLI/process adapter.
//!
//! Вся доменная работа происходит в библиотеке; здесь только разбор
//! аргументов, печать подготовленного вывода и отображение результата на
//! process exit code.

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

use anki_repo::cli::Cli;
use anki_repo::run::{execute, render_error};

fn main() -> ExitCode {
    let cli = Cli::parse();

    match execute(&cli) {
        Ok(rendered) => {
            print!("{}", rendered.stdout);
            let _ = std::io::stdout().flush();
            ExitCode::from(rendered.exit)
        }
        Err(error) => {
            let command = cli.command_name();
            let (stdout, stderr) = render_error(command, cli.json, &error);
            print!("{stdout}");
            eprint!("{stderr}");
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            ExitCode::from(error.exit_code())
        }
    }
}
