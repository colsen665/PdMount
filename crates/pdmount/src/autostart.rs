//! `pdmount autostart` (DESIGN.md §14.2, §14.4, D059): an XDG autostart entry
//! that mounts the profile at desktop login.

use anyhow::Context;
use pdmount_core::ProfilePaths;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn autostart_dir(paths: &ProfilePaths) -> PathBuf {
    paths
        .config_dir
        .parent()
        .unwrap_or(&paths.config_dir)
        .join("autostart")
}

/// `$XDG_CONFIG_HOME/autostart/pdmount-<profile>.desktop`.
pub fn entry_path(paths: &ProfilePaths) -> PathBuf {
    autostart_dir(paths).join(format!("pdmount-{}.desktop", paths.profile))
}

/// `$XDG_CONFIG_HOME/autostart/pdmount-<profile>-attach.desktop`: the entry
/// `service install` writes so that each desktop login hands its session bus
/// to the service's daemon (D102). It mounts nothing, so it is not autostart.
pub fn attach_entry_path(paths: &ProfilePaths) -> PathBuf {
    autostart_dir(paths).join(format!("pdmount-{}-attach.desktop", paths.profile))
}

/// The `Exec` arguments after the binary and profile that mark an attach entry.
const ATTACH_ARGS: [&str; 2] = ["session", "attach"];

/// Whether the file is an attach entry. The profile `p-attach`'s autostart
/// entry and profile `p`'s attach entry have the same name; the content
/// tells them apart.
fn is_attach_entry(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| {
        text.lines()
            .any(|line| line.starts_with("Exec=") && line.ends_with(" session attach"))
    })
}

/// Whether the profile's autostart entry exists (an attach entry of another
/// profile with the same file name does not count).
pub fn enabled(paths: &ProfilePaths) -> bool {
    let entry = entry_path(paths);
    entry.exists() && !is_attach_entry(&entry)
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
    desktop_entry(&format!("PdMount ({profile})"), &args)
}

/// The attach entry: no secret flag, since `session attach` reads no secret.
fn attach_entry_text(profile: &str, binary: &str) -> String {
    let mut args = vec![binary, "--profile", profile];
    args.extend(ATTACH_ARGS);
    desktop_entry(&format!("PdMount desktop bus ({profile})"), &args)
}

fn desktop_entry(name: &str, args: &[&str]) -> String {
    let exec: Vec<String> = args.iter().map(|arg| quote_arg(arg)).collect();
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={}\n\
         NoDisplay=true\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         Exec={}\n",
        escape_value(name),
        escape_value(&exec.join(" "))
    )
}

/// `service install`: writes the attach entry for `binary`; refuses to
/// replace another profile's autostart entry of the same name.
pub fn write_attach_entry(paths: &ProfilePaths, binary: &str) -> anyhow::Result<PathBuf> {
    let entry = attach_entry_path(paths);
    if entry.exists() && !is_attach_entry(&entry) {
        anyhow::bail!(
            "{} exists and is not PdMount's attach entry (the autostart entry of profile {}-attach?); \
             remove it first",
            entry.display(),
            paths.profile
        );
    }
    enable_at(&entry, &attach_entry_text(paths.profile.as_str(), binary))
        .with_context(|| format!("cannot write {}", entry.display()))?;
    Ok(entry)
}

/// `service uninstall`: removes the attach entry, never another file of
/// that name; whether there was one.
pub fn remove_attach_entry(paths: &ProfilePaths) -> anyhow::Result<Option<PathBuf>> {
    let entry = attach_entry_path(paths);
    if !is_attach_entry(&entry) {
        return Ok(None);
    }
    let removed =
        disable_at(&entry).with_context(|| format!("cannot remove {}", entry.display()))?;
    Ok(removed.then_some(entry))
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
    if entry.exists() && is_attach_entry(&entry) {
        anyhow::bail!(
            "{} is the attach entry of another profile's service; this profile's name ends in \
             -attach, which cannot have an autostart entry next to it",
            entry.display()
        );
    }
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
    let removed = enabled(paths)
        && disable_at(&entry).with_context(|| format!("cannot remove {}", entry.display()))?;
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
    if enabled(paths) {
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
    fn the_attach_entry_runs_session_attach_without_a_secret_flag() {
        let text = attach_entry_text("work", "/opt/my tools/pdmount");
        assert_eq!(
            text,
            "[Desktop Entry]\nType=Application\nName=PdMount desktop bus (work)\nNoDisplay=true\n\
             Terminal=false\nX-GNOME-Autostart-enabled=true\n\
             Exec=\"/opt/my tools/pdmount\" --profile work session attach\n"
        );
    }

    fn paths_in(root: &Path, profile: &str) -> ProfilePaths {
        ProfilePaths::new(
            profile.parse().expect("profile"),
            &pdmount_core::paths::BaseDirs {
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
                state: root.join("state"),
                runtime: None,
            },
        )
    }

    /// Profile `p`'s attach entry has the name of profile `p-attach`'s
    /// autostart entry: neither is taken for the other.
    #[test]
    fn an_attach_entry_is_never_counted_as_autostart() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = paths_in(dir.path(), "p");
        let other = paths_in(dir.path(), "p-attach");
        assert_eq!(attach_entry_path(&p), entry_path(&other));
        let entry = write_attach_entry(&p, "/usr/bin/pdmount").expect("write");
        assert!(entry.ends_with("autostart/pdmount-p-attach.desktop"));
        assert!(!enabled(&p), "the attach entry is not p's autostart");
        assert!(!enabled(&other), "nor p-attach's");
        assert!(state_text(&other).starts_with("not enabled"));
        // Profile p-attach's real autostart entry is not overwritten or removed.
        std::fs::write(&entry, entry_text("p-attach", "/usr/bin/pdmount", None)).expect("write");
        assert!(enabled(&other));
        assert!(write_attach_entry(&p, "/usr/bin/pdmount").is_err());
        assert_eq!(remove_attach_entry(&p).expect("remove"), None);
        assert!(entry.exists());
        std::fs::remove_file(&entry).expect("rm");
        write_attach_entry(&p, "/usr/bin/pdmount").expect("write");
        assert_eq!(
            remove_attach_entry(&p).expect("remove"),
            Some(entry.clone())
        );
        assert!(!entry.exists());
        assert_eq!(remove_attach_entry(&p).expect("nothing"), None);
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
