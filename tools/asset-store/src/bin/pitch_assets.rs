use std::io::{self, Write};
use std::process::ExitCode;

use asset_store::pitch_cli::{PitchCli, execute, install_safe_panic_hook};
use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = PitchCli::parse();
    install_safe_panic_hook(cli.output);
    let output = execute(cli).await;
    if let Err(error) = io::stdout().lock().write_all(output.stdout.as_bytes())
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        let _ = writeln!(io::stderr().lock(), "не удалось записать stdout: {error}");
        return ExitCode::from(5);
    }
    let _ = io::stderr().lock().write_all(output.stderr.as_bytes());
    ExitCode::from(output.exit_code)
}
