use clap::Parser;

mod arg_bind;
mod dispatcher;
mod error;
mod exit;
mod help;
mod project_root;
mod transport;
mod verbs;

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

    match dispatcher::run(cli).await {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            let code = e.exit_code();
            eprintln!("rye: {e}");
            std::process::exit(code);
        }
    }
}
