//! Explicit reference runner; built only as a Cargo example.
#[allow(dead_code)]
#[path = "mod.rs"]
mod support;

fn main() -> std::process::ExitCode {
    match support::reference::run(std::env::args_os().skip(1)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}
