// Cargo only rebuilds a crate when a file it depends on changes, and it learns
// about those files from the compiler's dep-info output. `include_gresource_from_dir!`
// reads and compresses everything under `data/resources` at compile time, but as a
// proc macro it never reports those reads, so editing a stylesheet or a `.ui` file
// would silently leave the previously built bundle in the binary.
//
// Declaring the directory here makes cargo re-run this crate whenever anything
// inside it changes, which is what keeps a CSS-only edit from being a no-op.

fn main() {
    println!("cargo::rerun-if-changed=data/resources");

    for entry in walk("data/resources") {
        println!("cargo::rerun-if-changed={entry}");
    }
}

/// Every file below `dir`, recursively, as slash-separated paths.
fn walk(dir: &str) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(path) = path.to_str() else {
            continue;
        };
        if path.starts_with('.') {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            found.extend(walk(path));
        } else {
            found.push(path.to_owned());
        }
    }
    found
}
