//! Glob matching for GitHub-style `paths` / `paths-ignore` filters.

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Reject a pattern `globset` cannot compile, naming the field it came from.
///
/// [`compile_globs`] drops what it cannot parse, because it runs on a stored definition
/// where there is nobody to tell. That makes `paths: ["src/[**"]` an *empty* filter set,
/// and an empty set matches nothing — so the trigger never fires and the pipeline looks
/// like it is being ignored. The write path calls this instead, so the typo is a 400 with
/// the pattern in it rather than a build that never happens.
pub fn validate_globs(field: &str, patterns: &[String]) -> Result<(), String> {
    for p in patterns {
        let trimmed = p.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "{field}: an empty pattern matches nothing; remove it"
            ));
        }
        Glob::new(trimmed).map_err(|e| format!("{field}: invalid glob `{trimmed}`: {e}"))?;
    }
    Ok(())
}

/// Compile patterns; invalid globs are skipped.
///
/// Lenient on purpose: a definition stored before [`validate_globs`] existed still has to
/// evaluate rather than panic. New and edited pipelines cannot reach here with a pattern
/// this would drop.
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
    builder
        .build()
        .unwrap_or_else(|_| GlobSetBuilder::new().build().unwrap())
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
    fn an_invalid_glob_is_a_named_error_not_an_empty_filter() {
        // Unclosed character class: the pattern the finding used.
        let err = validate_globs("on.push.paths", &["src/[**".into()]).expect_err("rejected");
        assert!(err.contains("src/[**"), "{err}");
        assert!(err.contains("on.push.paths"), "{err}");
        // And what it used to do instead: silently become a set that matches nothing.
        assert!(compile_globs(&["src/[**".into()]).is_empty());
        assert!(!paths_allow(
            &["src/main.rs".into()],
            &["src/[**".into()],
            &[]
        ));
    }

    #[test]
    fn valid_globs_and_empty_patterns() {
        assert!(validate_globs("on.push.paths", &["src/**".into(), "*.md".into()]).is_ok());
        assert!(validate_globs("on.push.paths", &[]).is_ok());
        assert!(validate_globs("on.push.paths", &["  ".into()]).is_err());
    }

    #[test]
    fn empty_changed_with_filters_denies() {
        assert!(!paths_allow(&[], &["src/**".into()], &[]));
    }
}
