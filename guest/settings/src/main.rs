// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() -> glib::ExitCode {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--version" || argument == "-V")
    {
        println!("Buzzard OS Settings {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }
    buzzardos_settings::run()
}
