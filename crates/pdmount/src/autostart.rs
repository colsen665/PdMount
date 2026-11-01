//! `pdmount autostart` (DESIGN.md §14.2, §14.4, D059): an XDG autostart entry
//! that mounts the profile at desktop login.

use anyhow::Context;
use pdmount_core::ProfilePaths;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// `$XDG_CONFIG_HOME/autostart/pdmount-<profile>.desktop`.
pub fn entry_path(paths: &ProfilePaths) -> PathBuf {
    paths
        .config_dir
        .parent()
        .unwrap_or(&paths.config_dir)
        .join("autostart")
        .join(format!("pdmount-{}.desktop", paths.profile))
}

/// One argument of an `Exec` key, by the Desktop Entry spec's quoting rules.
fn quote_arg(arg: &str) -> String {
    let reserved = |c: char| " \t\n\"'\\><~|&;$*?#()`".contains(c);
    let doubled = arg.replace('%', "%%");
    if !doubled.is_empty() && !doubled.contains(reserved) {
        return doubled;
    }
    let mut quoted = String::with_capacity(doubled.len() + 2);
    quoted.push('"');
    for c in doubled.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// The escapes of a string value, which readers undo before the quoting.
fn escape_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
}

fn entry_text(profile: &str, binary: &str, secrets_flag: Option<&str>) -> String {
    let mut args = vec![binary, "--profile", profile];
    args.extend(secrets_flag);
    args.push("mount");
    let exec: Vec<String> = args.into_iter().map(quote_arg).collect();
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=PdMount ({})\n\
         NoDisplay=true\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         Exec={}\n",
        escape_value(profile),
        escape_value(&exec.join(" "))
    )
}

fn enable_at(entry: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = entry.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(entry, text)
}

/// Whether an entry was there.
fn disable_at(entry: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(entry) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn enable(paths: &ProfilePaths) -> anyhow::Result<ExitCode> {
    let binary = std::env::current_exe().context("cannot find the pdmount executable")?;
    let binary = binary
        .to_str()
        .context("the path of the pdmount executable is not UTF-8")?;
    let entry = entry_path(paths);
    let text = entry_text(
        paths.profile.as_str(),
        binary,
        crate::credentials::mode().flag(),
    );
    enable_at(&entry, &text).with_context(|| format!("cannot write {}", entry.display()))?;
    println!(
        "Profile {} mounts at desktop login ({}).",
        paths.profile,
        entry.display()
    );
    Ok(ExitCode::SUCCESS)
}

pub fn disable(paths: &ProfilePaths) -> anyhow::Result<ExitCode> {
    let entry = entry_path(paths);
    let removed =
        disable_at(&entry).with_context(|| format!("cannot remove {}", entry.display()))?;
    if removed {
        println!("Removed {}.", entry.display());
    } else {
        println!("Autostart was not enabled for profile {}.", paths.profile);
    }
    Ok(ExitCode::SUCCESS)
}

pub fn show(paths: &ProfilePaths) -> ExitCode {
    println!("{}", state_text(paths));
    ExitCode::SUCCESS
}

/// One line for `autostart` and `doctor`.
pub fn state_text(paths: &ProfilePaths) -> String {
    let entry = entry_path(paths);
    if entry.exists() {
        format!(
            "enabled for profile {} ({})",
            paths.profile,
            entry.display()
        )
    } else {
        format!("not enabled for profile {}", paths.profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_has_the_fields_and_flags() {
        let text = entry_text("work", "/usr/bin/pdmount", Some("--allow-file-secrets"));
        assert_eq!(
            text,
            "[Desktop Entry]\nType=Application\nName=PdMount (work)\nNoDisplay=true\nTerminal=false\n\
             X-GNOME-Autostart-enabled=true\n\
             Exec=/usr/bin/pdmount --profile work --allow-file-secrets mount\n"
        );
        assert!(!entry_text("work", "/p", None).contains("secrets"));
    }

    #[test]
    fn exec_quoting_follows_the_spec() {
        assert_eq!(quote_arg("/usr/bin/pdmount"), "/usr/bin/pdmount");
        assert_eq!(quote_arg("/opt/my tools/pd"), "\"/opt/my tools/pd\"");
        assert_eq!(quote_arg("/opt/100%/pd"), "/opt/100%%/pd");
        assert_eq!(quote_arg("/a b/100%"), "\"/a b/100%%\"");
        assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_arg("a`b$c"), "\"a\\`b\\$c\"");
        assert_eq!(quote_arg("a\\b"), "\"a\\\\b\"");
        assert_eq!(quote_arg("a'b"), "\"a'b\"");
    }

    /// The string escapes apply on top of the quoting: a literal backslash in
    /// a quoted argument is four in the file (the spec's own example).
    #[test]
    fn a_backslash_is_escaped_twice_in_the_file() {
        let text = entry_text("p", "/a\\b", None);
        assert!(
            text.contains("Exec=\"/a\\\\\\\\b\" --profile p mount\n"),
            "{text}"
        );
        let text = entry_text("p", "/a\"b", None);
        assert!(
            text.contains("Exec=\"/a\\\\\"b\" --profile p mount\n"),
            "{text}"
        );
    }

    #[test]
    fn enable_and_disable_a_temporary_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let entry = dir.path().join("autostart/pdmount-p.desktop");
        assert!(!disable_at(&entry).expect("nothing there"));
        enable_at(&entry, "text").expect("enable");
        assert_eq!(std::fs::read_to_string(&entry).expect("read"), "text");
        enable_at(&entry, "other").expect("replace");
        assert!(disable_at(&entry).expect("disable"));
        assert!(!entry.exists());
    }
}
