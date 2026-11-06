//! Development-only tools for measuring a real stream without a person or a display.
//! This application is never packaged or shipped.

mod relay;
mod stream;

use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "rustconsole-lab",
    about = "Rust Console development test tools"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Receive and decode a stream without a window and record measurements.
    Stream {
        #[arg(long)]
        address: SocketAddr,
        #[arg(long)]
        seconds: u64,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 100)]
        maximum_megabits: u64,
        #[arg(long, default_value_t = 100)]
        maximum_delay_milliseconds: u64,
        #[arg(long, default_value_t = 120)]
        frames_per_second: u16,
    },
    /// Forward UDP between a player and a host through a scripted bad network.
    Relay {
        #[arg(long)]
        listen: SocketAddr,
        #[arg(long)]
        target: SocketAddr,
        /// Lines of `seconds rate_megabits delay_milliseconds jitter_milliseconds loss_percent`.
        #[arg(long)]
        scenario: PathBuf,
        #[arg(long)]
        seconds: u64,
    },
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Stream {
            address,
            seconds,
            output,
            maximum_megabits,
            maximum_delay_milliseconds,
            frames_per_second,
        } => stream::run(stream::Options {
            address,
            seconds,
            output,
            maximum_bitrate_bits_per_second: maximum_megabits * 1_000_000,
            maximum_delay_micros: maximum_delay_milliseconds * 1_000,
            frames_per_second,
        }),
        Command::Relay {
            listen,
            target,
            scenario,
            seconds,
        } => relay::run(listen, target, &scenario, seconds),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustconsole-lab: {error}");
            ExitCode::FAILURE
        }
    }
}
