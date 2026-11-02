//! Code shared by the desktop app and the caching server.

pub mod api;
pub mod demo;
pub mod devlog;
pub mod export;
pub mod i18n;
pub mod report_pdf;
pub mod stats;
pub mod store;
pub mod sync;

/// "1 note", "3 notes" – translated through the catalogue's `{n} <noun>` plural entries.
pub fn count(n: usize, noun: &str) -> String {
    i18n::plural(n, &format!("{{n}} {noun}"), &format!("{{n}} {noun}s"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts() {
        assert_eq!(super::count(1, "note"), "1 note");
        assert_eq!(super::count(0, "reading"), "0 readings");
        assert_eq!(super::count(12, "day"), "12 days");
    }
}
