use clap::Parser;

// CLI binary only: the library stays allocator-agnostic for embedders (oj).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    name = "fru",
    version,
    about = "Clean-room StyleX compiler (oracle: @stylexjs/babel-plugin 0.19.0)"
)]
struct Cli {
    #[command(subcommand)]
    command: oj_stylex::cli::Command,
}

fn main() {
    // Snapshot the env before any compile thread starts (oj_env's contract).
    oj_env::init();
    oj_stylex::cli::run(Cli::parse().command);
}
