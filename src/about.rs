//! The About page: project links and the current version's entry from `CHANGELOG.md`.

use crate::updater::REPO;

const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// Link `index` of the About page (see `About.open-link` in the UI).
pub fn link(index: i32) -> Option<String> {
    let base = format!("https://github.com/{REPO}");
    Some(match index {
        0 => base,
        1 => format!("{base}/issues"),
        2 => format!("{base}/releases"),
        3 => format!("{base}/blob/main/LICENSE"),
        4 => format!("{base}#readme"),
        _ => return None,
    })
}

/// The changelog section of `version` as plain text (headings and bullets simplified).
pub fn changes(version: &str) -> String {
    changes_in(CHANGELOG, version)
}

/// "0.2.0" from a heading like "## [0.2.0] – 2026-10-29".
fn heading_version(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("## ")?.trim_start_matches('[').trim_start_matches('v');
    rest.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-')).next()
}

fn changes_in(changelog: &str, version: &str) -> String {
    let is_heading = |l: &str| l.starts_with("## ");
    let mut lines = changelog.lines().skip_while(|l| heading_version(l) != Some(version));
    if lines.next().is_none() {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    for line in lines.take_while(|l| !is_heading(l)) {
        let line = line.trim_end();
        let text = if let Some(h) = line.strip_prefix("### ") {
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            h.to_string()
        } else if let Some(item) = line.strip_prefix("- ") {
            format!("•  {}", item.replace("**", "").replace('`', ""))
        } else if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        } else {
            line.replace("**", "").replace('`', "")
        };
        out.push(text);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_one_version() {
        let log = "# Changelog\n\n## [0.2.0] – 2026-10-29\n\n### Added\n- **Notes** on the graph\n\n### Fixed\n- `x`\n\n## [0.1.0]\n- old\n";
        assert_eq!(changes_in(log, "0.2.0"), "Added\n•  Notes on the graph\n\nFixed\n•  x");
        assert_eq!(changes_in(log, "0.1.0"), "•  old");
        assert_eq!(changes_in(log, "0.2"), "");
        assert_eq!(changes_in(log, "9.9.9"), "");
    }

    #[test]
    fn current_version_has_notes() {
        assert!(!changes(crate::updater::CURRENT).is_empty(), "CHANGELOG.md has no entry for this version");
        assert!(link(0).is_some_and(|l| l.starts_with("https://github.com/")) && link(9).is_none());
    }
}
