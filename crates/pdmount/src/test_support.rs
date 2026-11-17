//! Helpers for unit tests.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes an executable script at `path`. A `sh` child writes it, so this
/// process never holds a write descriptor on a file it runs later: a child
/// that another test thread forks meanwhile would inherit that descriptor
/// until its exec, and running the script would then fail with "Text file
/// busy" (ETXTBSY).
pub fn write_executable(path: &Path, text: &str) {
    let mut child = Command::new("sh")
        .args(["-c", "cat >\"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn sh");
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(text.as_bytes())
        .expect("write script");
    assert!(
        child.wait().expect("wait").success(),
        "cannot write {path:?}"
    );
}
