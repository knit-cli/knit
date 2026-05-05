use std::path::Path;

pub fn same_path(left: &str, right: &str) -> bool {
    Path::new(left) == Path::new(right)
}
