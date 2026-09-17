//! Static audit: every file that names a Rust version must name the same one.
//!
//! `rust-toolchain.toml` wins at build time regardless, so a mismatch does not fail — it
//! silently makes every other file a lie and costs a full toolchain download per step.
//! Dogfooding found `fiber.yml` pinned to an image two files had moved past, paying that
//! download on `fmt`, `clippy`, `test` and `build` of every push and pull request.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    fn repo_file(rel: &str) -> String {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..");
        std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"))
    }

    /// The `x.y` of the first `channel = "…"`, `rust:…`, or `toolchain: "…"` it finds.
    fn versions_in(text: &str, needle: &str) -> Vec<String> {
        text.lines()
            .filter(|l| l.contains(needle) && !l.trim_start().starts_with('#'))
            .filter_map(|l| {
                let after = l.split(needle).nth(1)?;
                let digits: String = after
                    .trim_start_matches(['"', ':', ' ', '='])
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                let mut parts = digits.split('.');
                Some(format!("{}.{}", parts.next()?, parts.next()?))
            })
            .collect()
    }

    #[test]
    fn every_file_that_names_a_rust_version_names_the_same_one() {
        let pinned = versions_in(&repo_file("rust-toolchain.toml"), "channel")
            .first()
            .cloned()
            .expect("rust-toolchain.toml declares a channel");

        let mut found: Vec<(&str, String)> = vec![("rust-toolchain.toml", pinned.clone())];
        for (file, needle) in [
            ("fiber.yml", "rust:"),
            ("deploy/Dockerfile", "rust:"),
            (".github/workflows/ci.yml", "toolchain:"),
        ] {
            let text = repo_file(file);
            let versions = versions_in(&text, needle);
            assert!(
                !versions.is_empty(),
                "{file} names no Rust version via `{needle}` — did the syntax change?"
            );
            for v in versions {
                found.push((file, v));
            }
        }

        let disagreeing: Vec<_> = found.iter().filter(|(_, v)| *v != pinned).collect();
        assert!(
            disagreeing.is_empty(),
            "rust-toolchain.toml pins {pinned}, but these disagree: {disagreeing:?}. \
             The pin wins at build time, so the others only cost a toolchain download per step."
        );
    }
}
