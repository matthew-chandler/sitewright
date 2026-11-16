use clap::Parser;

fn main() -> std::process::ExitCode {
    // A backend worker is this same program, started by the daemon with one fixed
    // argument. It must be recognised before anything else happens: the worker jails
    // itself, and the jail covers only the thread that installs it, so no runtime
    // threads may exist yet.
    if std::env::args_os().nth(1).as_deref() == Some(sw_backend::WORKER_ARG.as_ref()) {
        sw_backend::worker_main()
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: could not start the async runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    runtime.block_on(async {
        sw_core::crypto::install_default_provider();
        let cli = sitewright::cli::Cli::parse();
        match sitewright::cli::run(cli).await {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error:#}");
                std::process::ExitCode::FAILURE
            }
        }
    })
}
