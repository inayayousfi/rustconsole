#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    match rustconsole_test_game::parse_options(std::env::args().skip(1)).and_then(windows::run) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustconsole-test-game: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() -> std::process::ExitCode {
    eprintln!("rustconsole-test-game is available only on Windows");
    std::process::ExitCode::FAILURE
}
