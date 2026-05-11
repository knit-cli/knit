use crate::output as out;
use crate::store::{find_knit_root, load_config};
use std::path::Path;

pub fn print(root: &Path, message: impl AsRef<str>) {
    if enabled(root) {
        println!("{} {}", out::heading("Next:"), message.as_ref());
    }
}

pub fn enabled(root: &Path) -> bool {
    match std::env::var("KNIT_ADVICE") {
        Ok(value) if matches!(value.as_str(), "0" | "false" | "no" | "off") => return false,
        Ok(value) if matches!(value.as_str(), "1" | "true" | "yes" | "on") => return true,
        _ => {}
    }

    load_config(root)
        .map(|config| config.advice)
        .unwrap_or(true)
}

pub fn print_without_workspace(message: impl AsRef<str>) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => return,
    };
    if let Some(root) = find_knit_root(&cwd) {
        print(&root, message);
    }
}
