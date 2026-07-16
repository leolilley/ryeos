use clap::Parser;
use ryeos_cli::{dispatcher, error, tty};

fn init_tracing(debug: bool) {
    if debug {
        tracing_subscriber::fmt()
            .with_env_filter("ryeos_cli=debug")
            .with_target(false)
            .init();
    }
}

#[tokio::main]
async fn main() {
    let cli = dispatcher::Cli::parse();
    init_tracing(cli.debug);
    let console = tty::Console::detect(dispatcher::forces_plain_output(&cli.rest));

    match dispatcher::run(cli, &console).await {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            if matches!(&e, error::CliError::Io(error) if error.kind() == std::io::ErrorKind::BrokenPipe)
            {
                std::process::exit(0);
            }
            let code = e.exit_code();
            if !matches!(e, error::CliError::Reported { .. }) {
                let _ = console.error(&e.diagnostic());
            }
            std::process::exit(code);
        }
    }
}
