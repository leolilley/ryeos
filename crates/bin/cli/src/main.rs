use clap::Parser;

mod arg_bind;
mod dispatcher;
mod effective_metadata;
mod error;
mod exec_stream;
mod exit;
mod help;
mod lifecycle_commands;
mod node_descriptors;
mod offline_dispatch;
mod project_resolve;
#[cfg(test)]
mod test_env;
mod transport;
mod tty;

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
            eprintln!("ryeos: {e}");
            std::process::exit(code);
        }
    }
}
