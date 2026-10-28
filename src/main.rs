// drover runs coding agents in git worktrees, on this machine or over ssh. See DESIGN.md.

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T, E = Error> = std::result::Result<T, E>;

macro_rules! err {
    ($($t:tt)*) => { $crate::Error::from(format!($($t)*)) };
}

macro_rules! bail {
    ($($t:tt)*) => { return Err(err!($($t)*)) };
}

mod cli;
mod config;
mod control;
mod harness;
mod herdr;
mod http;
mod migrate;
mod run;
mod serve;
mod socket;
mod store;
mod toon;

#[tokio::main]
async fn main() {
    use clap::Parser;
    std::process::exit(cli::main(cli::Cli::parse()).await);
}
