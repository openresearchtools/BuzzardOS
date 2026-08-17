// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() {
    // Dependency panics must not serialize clipboard bytes or untrusted MIME
    // metadata into the session log.
    std::panic::set_hook(Box::new(|_| {
        eprintln!("clipboard-agent fatal: internal_panic");
    }));
    let arguments: Vec<_> = std::env::args_os().collect();
    if buzzardos_clipboard_agent::is_internal_worker_invocation(&arguments) {
        std::process::exit(buzzardos_clipboard_agent::internal_worker_entrypoint());
    }
    if arguments.len() != 1 {
        eprintln!("clipboard-agent startup failed: unexpected_arguments");
        std::process::exit(2);
    }
    if let Err(error) = buzzardos_clipboard_agent::run() {
        eprintln!("clipboard-agent startup failed: {}", error.category());
        std::process::exit(1);
    }
}
