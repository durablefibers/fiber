//! Glob matching for GitHub-style `paths` / `paths-ignore` filters.

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Compile patterns; invalid globs are skipped.
pub fn compile_globs(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if let Ok(g) = Glob::new(p) {
            builder.add(g);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap())
}

pub fn any_match(set: &GlobSet, path: &str) -> bool {
    !set.is_empty() && set.is_match(path)
}

/// Whether a webhook with `changed` files should fire given path filters.
///
/// - No filters → always true (caller still applies branch rules).
/// - Empty `changed` with filters → false (can't prove a match).
/// - Otherwise: at least one file matches `paths` (or paths empty) and is not
///   matched by `paths_ignore`.
pub fn paths_allow(changed: &[String], paths: &[String], paths_ignore: &[String]) -> bool {
    if paths.is_empty() && paths_ignore.is_empty() {
        return true;
    }
    if changed.is_empty() {
        return false;
    }
    let path_set = compile_globs(paths);
    let ignore_set = compile_globs(paths_ignore);
    changed.iter().any(|f| {
        let ignored = !ignore_set.is_empty() && ignore_set.is_match(f);
        if ignored {
            return false;
        }
        if paths.is_empty() {
            return true;
        }
        path_set.is_match(f)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_filters_allows() {
        assert!(paths_allow(&["a.rs".into()], &[], &[]));
    }

    #[test]
    fn paths_require_match() {
        let paths = vec!["src/**".into()];
        assert!(paths_allow(&["src/main.rs".into()], &paths, &[]));
        assert!(!paths_allow(&["README.md".into()], &paths, &[]));
    }

    #[test]
    fn paths_ignore_blocks_only_when_all_ignored() {
        let ignore = vec!["**/*.md".into()];
        assert!(!paths_allow(&["README.md".into()], &[], &ignore));
        assert!(paths_allow(
            &["README.md".into(), "src/lib.rs".into()],
            &[],
            &ignore
        ));
    }

    #[test]
    fn empty_changed_with_filters_denies() {
        assert!(!paths_allow(&[], &["src/**".into()], &[]));
    }
}
