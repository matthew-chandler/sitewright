use clap::Parser;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    sw_core::crypto::install_default_provider();
    let cli = sitewright::cli::Cli::parse();
    match sitewright::cli::run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
