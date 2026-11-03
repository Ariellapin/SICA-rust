//! What the two panic hooks (`backend::incident`, `frontend::fe_incident`)
//! share: finding where in *this* workspace a panic came from.
//!
//! A panic's own location is often inside a dependency — the first one the
//! frontend's hook caught was an egui assertion, located at
//! `C:/Users/…/.cargo/registry/src/…/ecolor-0.28.1/src/color32.rs`. That
//! path is machine-specific and is not where the fix goes; the first frame
//! in our own `crates/` is.

/// The first backtrace frame in this workspace's `crates/`, as
/// `crates/<crate>/src/<file>:<line>` — skipping the standard library, the
/// cargo registry, and `hook_file` (the hook's own source, `file!()` of the
/// caller, whose frame is always on the stack).
pub fn own_frame(backtrace: &str, hook_file: &str) -> Option<String> {
    let hook = hook_file.replace('\\', "/");
    let hook = hook.trim_start_matches("./");
    backtrace.lines().find_map(|l| {
        let path = l.trim().strip_prefix("at ")?.replace('\\', "/");
        if path.contains("/registry/") || path.contains("/rustc/") {
            return None;
        }
        let at = if path.starts_with("crates/") {
            0
        } else if path.starts_with("./crates/") {
            2
        } else {
            path.find("/crates/")? + 1
        };
        let rel = &path[at..];
        if !hook.is_empty() && rel.starts_with(hook) {
            return None;
        }
        // `file:line:col` → `file:line`.
        let mut parts = rel.rsplitn(3, ':').collect::<Vec<_>>();
        parts.reverse();
        Some(match parts.as_slice() {
            [file, line, _col] => format!("{file}:{line}"),
            _ => rel.to_string(),
        })
    })
}

/// [`own_frame`] without its line: the part a ticket's module carries, so
/// the same panic after an unrelated edit above it is the same ticket.
pub fn own_file(backtrace: &str, hook_file: &str) -> Option<String> {
    own_frame(backtrace, hook_file).map(|f| f.split(':').next().unwrap_or(&f).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BT: &str = "   2: frontend::fe_incident::install_panic_hook::{{closure}}\n\
                      \x20            at .\\crates\\frontend\\src\\fe_incident.rs:95:25\n\
                      \x20 29: ecolor::color32::Color32::linear_multiply\n\
                      \x20            at C:\\Users\\U\\.cargo\\registry\\src\\index.crates.io-1949\\ecolor-0.28.1\\src\\color32.rs:220:9\n\
                      \x20 31: frontend::ui::issues::sidebar_chip\n\
                      \x20            at .\\crates\\frontend\\src\\ui\\issues.rs:260:53\n";

    #[test]
    fn skips_the_hook_and_the_registry() {
        let hook = "crates\\frontend\\src\\fe_incident.rs";
        assert_eq!(own_frame(BT, hook).as_deref(), Some("crates/frontend/src/ui/issues.rs:260"));
        assert_eq!(own_file(BT, hook).as_deref(), Some("crates/frontend/src/ui/issues.rs"));
    }

    #[test]
    fn reads_absolute_paths_and_gives_up_on_std_only() {
        let absolute = "  at C:\\work\\sica-rust\\crates\\backend\\src\\chat.rs:10:5\n";
        assert_eq!(own_frame(absolute, "crates/backend/src/incident.rs").as_deref(), Some("crates/backend/src/chat.rs:10"));
        assert_eq!(own_frame("  at /rustc/abc/library/std/src/panicking.rs:1:1\n", ""), None);
    }
}
