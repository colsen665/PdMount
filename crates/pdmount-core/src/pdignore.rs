//! `.pdignore` rules (DESIGN.md §7.7, D018): entries that stay local-only.
//!
//! Gitignore syntax: `#` comments, `!` negation, a trailing `/` for
//! directories, a `/` at the start or in the middle to anchor at the mount
//! root, `*` and `?` within one name, `**` across directories; later rules
//! override earlier ones. A rule only keeps new nodes from being created
//! remotely; what already exists remotely keeps syncing.
//!
//! Adapted from pdcli `pdignore.rs` (MIT, © Thribhu K,
//! github.com/tirbofish/proton-sdk-rs2): the matcher only; PdMount's default
//! list is empty, and rules come from `<mount>/.pdignore` plus
//! `~/.config/pdmount/<profile>.pdignore`.

use globset::{GlobBuilder, GlobMatcher};

/// Name of the rules file at the mount root.
pub const ROOT_FILE: &str = ".pdignore";

struct Rule {
    negated: bool,
    dir_only: bool,
    matchers: Vec<GlobMatcher>,
}

/// Compiled rules of one or more `.pdignore` texts.
#[derive(Default)]
pub struct IgnoreMatcher {
    rules: Vec<Rule>,
}

impl IgnoreMatcher {
    /// Rules of the given texts, in order.
    pub fn new<'a>(texts: impl IntoIterator<Item = &'a str>) -> Self {
        let mut matcher = Self::default();
        for text in texts {
            matcher.rules.extend(text.lines().filter_map(parse_rule));
        }
        matcher
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether `relative_path` (from the mount root, without a leading
    /// `/`) is ignored.
    pub fn is_ignored(&self, relative_path: &str, is_dir: bool) -> bool {
        let path = relative_path.trim_matches('/');
        let mut ignored = false;
        for rule in &self.rules {
            if rule.matches(path, is_dir) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

impl Rule {
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir && !self.matches_parent_dir(path) {
            return false;
        }
        self.matchers.iter().any(|matcher| matcher.is_match(path))
    }

    fn matches_parent_dir(&self, path: &str) -> bool {
        let mut parent = path;
        while let Some((next, _)) = parent.rsplit_once('/') {
            if next.is_empty() {
                break;
            }
            parent = next;
            if self.matchers.iter().any(|matcher| matcher.is_match(parent)) {
                return true;
            }
        }
        false
    }
}

fn parse_rule(line: &str) -> Option<Rule> {
    let mut pattern = line.trim_end();
    if pattern.is_empty() || pattern.starts_with('#') {
        return None;
    }
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest;
    } else if let Some(rest) = pattern.strip_prefix("\\!") {
        pattern = rest;
    }
    if let Some(rest) = pattern.strip_prefix("\\#") {
        pattern = rest;
    }
    let dir_only = pattern.ends_with('/');
    let pattern = pattern.trim_end_matches('/');
    // As in gitignore, a slash at the start or in the middle anchors the
    // pattern at the root.
    let anchored = pattern.contains('/');
    let pattern = pattern.trim_start_matches('/');
    if pattern.is_empty() {
        return None;
    }
    let mut patterns = vec![pattern.to_owned()];
    if !anchored {
        patterns.push(format!("**/{pattern}"));
    }
    if dir_only {
        let children: Vec<String> = patterns.iter().map(|p| format!("{p}/**")).collect();
        patterns.extend(children);
    }
    let matchers: Vec<GlobMatcher> = patterns
        .iter()
        // `*` and `?` stop at `/`; only `**` crosses directories.
        .filter_map(|p| GlobBuilder::new(p).literal_separator(true).build().ok())
        .map(|glob| glob.compile_matcher())
        .collect();
    (!matchers.is_empty()).then_some(Rule {
        negated,
        dir_only,
        matchers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_common_gitignore_patterns() {
        let matcher = IgnoreMatcher::new(["*.log\nbuild/\n/docs/*.tmp\n"]);
        assert!(matcher.is_ignored("app.log", false));
        assert!(matcher.is_ignored("nested/app.log", false));
        assert!(matcher.is_ignored("build", true));
        assert!(
            !matcher.is_ignored("build", false),
            "build/ only matches directories"
        );
        assert!(matcher.is_ignored("build/output.o", false));
        assert!(matcher.is_ignored("src/build/output.o", false));
        assert!(matcher.is_ignored("docs/file.tmp", false));
        assert!(!matcher.is_ignored("src/docs/file.tmp", false));
        assert!(!matcher.is_ignored("src/main.rs", false));
    }

    #[test]
    fn wildcards_stay_within_a_name_and_a_middle_slash_anchors() {
        let matcher = IgnoreMatcher::new(["/docs/*.tmp\n/*.o\nbuild/out\nlogs/**/*.gz\n"]);
        assert!(matcher.is_ignored("docs/a.tmp", false));
        assert!(!matcher.is_ignored("docs/sub/a.tmp", false));
        assert!(matcher.is_ignored("main.o", false));
        assert!(!matcher.is_ignored("src/main.o", false));
        assert!(matcher.is_ignored("build/out", false));
        assert!(!matcher.is_ignored("src/build/out", false));
        assert!(matcher.is_ignored("logs/a/b/old.gz", false));
        assert!(matcher.is_ignored("logs/old.gz", false));
    }

    #[test]
    fn later_rules_and_later_texts_override() {
        let matcher = IgnoreMatcher::new(["*.tmp\n", "!important.tmp\n"]);
        assert!(matcher.is_ignored("scratch.tmp", false));
        assert!(!matcher.is_ignored("important.tmp", false));
    }

    #[test]
    fn the_default_is_empty() {
        let matcher = IgnoreMatcher::new(["# only a comment\n\n"]);
        assert!(matcher.is_empty());
        assert!(!matcher.is_ignored(".git/config", false));
    }
}
