use std::env;
use std::process::ExitCode;

mod cli;
mod dashboard;
mod mcp;

fn main() -> ExitCode {
    match cli::run(env::args().skip(1)) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("aifuel: {error}");
            ExitCode::from(2)
        }
    }
}
