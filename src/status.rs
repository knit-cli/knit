pub fn status_label(short_status: &str) -> &'static str {
    if short_status.trim().is_empty() {
        return "clean";
    }

    let staged = has_staged_changes(short_status);
    let modified = short_status.lines().any(|line| {
        let bytes = line.as_bytes();
        bytes.len() > 1 && bytes[1] != b' '
    });

    match (staged, modified) {
        (true, true) => "staged+modified",
        (true, false) => "staged",
        (false, _) => "modified",
    }
}

pub fn has_staged_changes(short_status: &str) -> bool {
    short_status.lines().any(|line| {
        let bytes = line.as_bytes();
        !line.starts_with("??") && !bytes.is_empty() && bytes[0] != b' '
    })
}
