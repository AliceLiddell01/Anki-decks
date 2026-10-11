use clap::{Parser, Subcommand};
use review_observatory::{AppState, router};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    name = "review-observatory",
    about = "Локальное read-only наблюдение review artifacts; запуск только явной командой serve"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Запустить HTTP на отдельном loopback порту; Node.js не нужен для просмотра.
    Serve {
        #[arg(long, default_value = ".")]
        repository: PathBuf,
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,
        #[arg(long,default_value_t=3085,value_parser=clap::value_parser!(u16).range(1..))]
        port: u16,
    },
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Review Observatory: {error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), String> {
    let Command::Serve {
        repository,
        host,
        port,
    } = Cli::parse().command;
    if !host.is_loopback() {
        return Err("--host должен быть loopback IP; внешние интерфейсы не поддерживаются".into());
    }
    let repository = repository
        .canonicalize()
        .map_err(|_| "--repository недоступен")?;
    if !repository.join("Cargo.toml").is_file()
        || !repository.join("tools/anki-repo/Cargo.toml").is_file()
    {
        return Err("--repository должен указывать на корень workspace Anki-decks".into());
    }
    let safe = review_observatory::files::SafeRoot::new(&repository)?;
    safe.read(std::path::Path::new("tools/review-observatory/web/dist/index.html"),4*1024*1024).map_err(|_|"Frontend не собран или недоступен. Выполните `npm ci --prefix tools/review-observatory/web` и `npm run build --prefix tools/review-observatory/web`, затем повторите serve.")?;
    let address = SocketAddr::new(host, port);
    let listener=tokio::net::TcpListener::bind(address).await.map_err(|error|format!("Не удалось занять {address}: {error}. Укажите свободный --port; автоматического переключения порта нет."))?;
    let state = AppState::new(&repository, address)?;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let observer = tokio::spawn(state.clone().observe(receiver));
    let stopping = state.clone();
    eprintln!("Review Observatory доступен на http://{address}. Для остановки нажмите Ctrl+C.");
    let result = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            wait_signal().await;
            stopping.shutdown();
            let _ = shutdown.send(true);
        })
        .await;
    observer.abort();
    result.map_err(|error| format!("HTTP-сервер завершился с ошибкой: {error}"))
}
async fn wait_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {_=tokio::signal::ctrl_c()=>(),_=terminate.recv()=>()}
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
