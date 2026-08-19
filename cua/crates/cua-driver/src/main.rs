// SPDX-License-Identifier: AGPL-3.0-or-later

use base64::Engine as _;
use cua_driver_core::protocol::{Content, ToolResult};
use serde_json::{Map, Value};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{self, Command};
use std::sync::Arc;

fn usage() {
    println!(
        "Buzzard CUA {}\n\n\
         Usage:\n\
           cua list-tools [--json]\n\
           cua describe TOOL\n\
           cua call TOOL [JSON] [--screenshot-out-file PATH]\n\
           cua TOOL [JSON] [--screenshot-out-file PATH]\n\
           cua browser WILDBUZZARD_ARGUMENTS...",
        env!("CARGO_PKG_VERSION")
    );
}

fn die(message: impl std::fmt::Display, code: i32) -> ! {
    eprintln!("cua: {message}");
    process::exit(code);
}

fn browser(arguments: &[String]) -> ! {
    let error = Command::new("/usr/bin/wildbuzzard").args(arguments).exec();
    die(format!("cannot execute /usr/bin/wildbuzzard: {error}"), 127)
}

fn take_option(arguments: &mut Vec<String>, name: &str) -> Option<String> {
    if let Some(index) = arguments.iter().position(|argument| argument == name) {
        if index + 1 >= arguments.len() {
            die(format!("{name} requires a value"), 64);
        }
        let value = arguments.remove(index + 1);
        arguments.remove(index);
        return Some(value);
    }
    let prefix = format!("{name}=");
    arguments
        .iter()
        .position(|argument| argument.starts_with(&prefix))
        .map(|index| arguments.remove(index)[prefix.len()..].to_owned())
}

fn parse_arguments(value: Option<&String>) -> Value {
    match value {
        None => Value::Object(Map::new()),
        Some(raw) => serde_json::from_str(raw)
            .unwrap_or_else(|error| die(format!("invalid tool JSON: {error}"), 64)),
    }
}

fn write_image(path: &PathBuf, data: &str) {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap_or_else(|error| die(format!("invalid image result: {error}"), 1));
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    if !parent.is_dir() {
        die(
            format!("image output directory does not exist: {}", parent.display()),
            1,
        );
    }
    std::fs::write(path, bytes)
        .unwrap_or_else(|error| die(format!("cannot write {}: {error}", path.display()), 1));
}

fn print_result(result: ToolResult, screenshot_output: Option<PathBuf>) {
    let failed = result.is_error == Some(true);
    let mut structured = result.structured_content;
    let mut text = Vec::new();
    for item in result.content {
        match item {
            Content::Text { text: value, .. } => text.push(value),
            Content::Image {
                data, mime_type, ..
            } => {
                if let Some(path) = screenshot_output.as_ref() {
                    write_image(path, &data);
                } else if let Some(Value::Object(object)) = structured.as_mut() {
                    object.insert("screenshot_png_b64".into(), Value::String(data));
                    object.insert("screenshot_mime_type".into(), Value::String(mime_type));
                }
            }
        }
    }
    if let Some(value) = structured {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        );
    } else {
        for line in text {
            println!("{line}");
        }
    }
    std::io::stdout().flush().ok();
    if failed {
        process::exit(1);
    }
}

fn registry() -> Arc<cua_driver_core::tool::ToolRegistry> {
    let registry = Arc::new(platform_linux::register_tools());
    registry.init_self_weak();
    registry
}

fn main() {
    let mut arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.is_empty() || matches!(arguments[0].as_str(), "help" | "-h" | "--help") {
        usage();
        return;
    }
    if matches!(arguments[0].as_str(), "version" | "-V" | "--version") {
        println!("Buzzard CUA {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if arguments[0] == "browser" {
        browser(&arguments[1..]);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| die(format!("cannot initialize runtime: {error}"), 1));
    let registry = registry();
    match arguments[0].as_str() {
        "list-tools" | "tools" => {
            if arguments.iter().any(|argument| argument == "--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&registry.tools_list()).unwrap()
                );
            } else {
                for name in registry.tool_names() {
                    println!("{name}");
                }
            }
        }
        "describe" => {
            let name = arguments
                .get(1)
                .unwrap_or_else(|| die("describe requires TOOL", 64));
            let definition = registry
                .get_def(name)
                .unwrap_or_else(|| die(format!("unknown tool: {name}"), 2));
            println!(
                "{}",
                serde_json::to_string_pretty(&definition.to_list_entry()).unwrap()
            );
        }
        _ => {
            let explicit_call = arguments[0] == "call";
            let tool_index = usize::from(explicit_call);
            let tool = arguments
                .get(tool_index)
                .cloned()
                .unwrap_or_else(|| die("call requires TOOL", 64));
            arguments.drain(0..=tool_index);
            let screenshot_output =
                take_option(&mut arguments, "--screenshot-out-file").map(PathBuf::from);
            if arguments.len() > 1 {
                die("tool calls accept at most one JSON argument", 64);
            }
            let input = parse_arguments(arguments.first());
            let result = runtime.block_on(registry.invoke_direct(&tool, input));
            print_result(result, screenshot_output);
        }
    }
}
