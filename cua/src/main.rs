// SPDX-License-Identifier: AGPL-3.0-or-later

mod contract;
mod core;
mod cursor;
mod platform;

use crate::core::protocol::{Content, ToolResult};
use base64::Engine as _;
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
           cua screenshot [JSON] --screenshot-out-file PATH\n\
           cua batch JSON_ARRAY\n\
           cua browser WILDBUZZARD_ARGUMENTS...\n\
           cua2 TOOL [JSON]  # independent seat/workspace 2\n\
           cua --index N TOOL [JSON]  # any positive numbered seat/workspace",
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
            format!(
                "image output directory does not exist: {}",
                parent.display()
            ),
            1,
        );
    }
    std::fs::write(path, bytes)
        .unwrap_or_else(|error| die(format!("cannot write {}: {error}", path.display()), 1));
}

fn print_result(result: ToolResult, screenshot_output: Option<PathBuf>) {
    let failed = result.is_error == Some(true);
    let mut structured = result.structured_content;
    if let Some(Value::Object(object)) = structured.as_mut() {
        if let (Ok(index), Ok(seat), Ok(workspace), Ok(output)) = (
            std::env::var(crate::core::seat_context::CUA_INDEX_ENV),
            std::env::var(crate::core::seat_context::CUA_SEAT_ENV),
            std::env::var(crate::core::seat_context::CUA_WORKSPACE_ENV),
            std::env::var(crate::core::seat_context::CUA_OUTPUT_ENV),
        ) {
            object.insert(
                "cua_index".into(),
                index.parse::<u32>().map(Value::from).unwrap_or(Value::Null),
            );
            object.insert("cua_seat".into(), Value::String(seat));
            object.insert("cua_workspace".into(), Value::String(workspace));
            object.insert("cua_output".into(), Value::String(output));
        }
    }
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

fn registry() -> Arc<crate::core::tool::ToolRegistry> {
    Arc::new(crate::platform::register_tools())
}

fn canonical_tool(name: &str) -> &str {
    match name {
        "focus" => "bring_to_front",
        "screenshot" => "get_desktop_state",
        other => other,
    }
}

fn alias_definition(mut definition: Value, alias: &str) -> Value {
    if canonical_tool(alias) != alias {
        if let Value::Object(object) = &mut definition {
            object.insert("name".into(), Value::String(alias.to_owned()));
            object.insert(
                "aliasFor".into(),
                Value::String(canonical_tool(alias).to_owned()),
            );
        }
    }
    definition
}

fn main() {
    let argv0 = std::env::args_os().next().unwrap_or_else(|| "cua".into());
    let mut arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|argument| argument == "--internal-wayland-clipboard-owner-v1")
    {
        arguments.remove(0);
        if let Err(error) = crate::platform::clipboard::run_internal_wayland_owner(&arguments) {
            println!("ERROR:{error}");
            std::io::stdout().flush().ok();
            process::exit(1);
        }
        return;
    }
    let mut invocation_index =
        crate::core::seat_context::invocation_index(&argv0).unwrap_or_else(|error| die(error, 64));
    let explicit_index = if arguments
        .first()
        .is_some_and(|argument| argument == "--index")
    {
        if arguments.len() < 2 {
            die("--index requires a positive integer", 64);
        }
        arguments.remove(0);
        Some(arguments.remove(0))
    } else if let Some(value) = arguments
        .first()
        .and_then(|argument| argument.strip_prefix("--index="))
        .map(str::to_owned)
    {
        arguments.remove(0);
        Some(value)
    } else {
        None
    };
    if let Some(value) = explicit_index {
        let requested = value
            .parse::<u32>()
            .ok()
            .filter(|index| *index > 0)
            .unwrap_or_else(|| die("--index requires a positive integer", 64));
        if invocation_index != 1 && invocation_index != requested {
            die("numbered cuaN executable conflicts with --index", 64);
        }
        invocation_index = requested;
    }
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
    match arguments[0].as_str() {
        "list-tools" | "tools" => {
            let registry = registry();
            if arguments.iter().any(|argument| argument == "--json") {
                let mut listed = registry.tools_list();
                if let Some(tools) = listed.get_mut("tools").and_then(Value::as_array_mut) {
                    for alias in ["focus", "screenshot"] {
                        if let Some(definition) = registry.get_def(canonical_tool(alias)) {
                            tools.push(alias_definition(definition.to_list_entry(), alias));
                        }
                    }
                }
                println!("{}", serde_json::to_string_pretty(&listed).unwrap());
            } else {
                for name in registry.tool_names() {
                    println!("{name}");
                }
                println!("focus");
                println!("screenshot");
            }
        }
        "describe" => {
            let registry = registry();
            let name = arguments
                .get(1)
                .unwrap_or_else(|| die("describe requires TOOL", 64));
            let definition = registry
                .get_def(canonical_tool(name))
                .unwrap_or_else(|| die(format!("unknown tool: {name}"), 2));
            println!(
                "{}",
                serde_json::to_string_pretty(&alias_definition(definition.to_list_entry(), name))
                    .unwrap()
            );
        }
        "batch" => {
            arguments.remove(0);
            if arguments.len() != 1 {
                die("batch requires exactly one JSON array", 64);
            }
            let steps = serde_json::from_str::<Vec<Value>>(&arguments[0])
                .unwrap_or_else(|error| die(format!("invalid batch JSON: {error}"), 64));
            if steps.is_empty() || steps.len() > 64 {
                die("batch requires between 1 and 64 steps", 64);
            }
            let _seat_context = crate::core::seat_context::prepare(invocation_index)
                .unwrap_or_else(|error| {
                    die(
                        format!("cannot prepare numbered CUA workspace: {error:#}"),
                        1,
                    )
                });
            std::env::set_var("BUZZARDOS_CUA_BATCH", "1");
            let registry = registry();
            let mut results = Vec::with_capacity(steps.len());
            let mut failed = false;
            for (index, step) in steps.into_iter().enumerate() {
                let Value::Object(mut object) = step else {
                    die(format!("batch step {} must be an object", index + 1), 64);
                };
                let name = object
                    .remove("tool")
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| {
                        die(format!("batch step {} requires string tool", index + 1), 64)
                    });
                let input = match object.remove("args") {
                    Some(args) if object.is_empty() => args,
                    Some(_) => die(
                        format!(
                            "batch step {} cannot mix args with inline fields",
                            index + 1
                        ),
                        64,
                    ),
                    None => Value::Object(object),
                };
                let canonical = canonical_tool(&name);
                let result = runtime.block_on(registry.invoke_direct(canonical, input));
                let is_error = result.is_error == Some(true);
                results.push(serde_json::json!({
                    "index": index,
                    "tool": name,
                    "result": result,
                }));
                if is_error {
                    failed = true;
                    break;
                }
            }
            println!("{}", serde_json::to_string_pretty(&results).unwrap());
            if failed {
                process::exit(1);
            }
        }
        _ => {
            let explicit_call = arguments[0] == "call";
            let tool_index = usize::from(explicit_call);
            let mut tool = arguments
                .get(tool_index)
                .cloned()
                .unwrap_or_else(|| die("call requires TOOL", 64));
            tool = canonical_tool(&tool).to_owned();
            arguments.drain(0..=tool_index);
            let screenshot_output =
                take_option(&mut arguments, "--screenshot-out-file").map(PathBuf::from);
            if arguments.len() > 1 {
                die("tool calls accept at most one JSON argument", 64);
            }
            let input = parse_arguments(arguments.first());
            let _seat_context = crate::core::seat_context::prepare(invocation_index)
                .unwrap_or_else(|error| {
                    die(
                        format!("cannot prepare numbered CUA workspace: {error:#}"),
                        1,
                    )
                });
            let registry = registry();
            let result = runtime.block_on(registry.invoke_direct(&tool, input));
            print_result(result, screenshot_output);
        }
    }
}
