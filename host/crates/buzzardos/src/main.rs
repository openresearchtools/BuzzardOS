// SPDX-License-Identifier: AGPL-3.0-or-later

mod cli;
mod display;
mod operations;

use anyhow::Result;
use clap::Parser;

fn main() {
    if let Err(error) = run() {
        eprintln!("Buzzard OS: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    operations::run(cli::Cli::parse())
}
