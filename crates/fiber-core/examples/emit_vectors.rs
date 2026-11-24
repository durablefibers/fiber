//! Emit golden vectors for the TypeScript port: `cargo run -p fiber-core --example
//! emit_vectors > conformance/vectors/core.json`. Inputs are fixed or come from a seeded
//! generator, so the output only changes when the behaviour does.

use chrono::{TimeZone, Utc};
use fiber_core::{dag, path_filter, roles::ProjectRole, schedule, step_if};
use fiber_proto::{limits, validate};
use serde_json::{Value, json};

/// SplitMix64: enough randomness for coverage, no dependency, same on every platform.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn word(&mut self, alphabet: &[&str], max: usize) -> String {
        let n = self.below(max + 1);
        (0..n)
            .map(|_| alphabet[self.below(alphabet.len())])
            .collect()
    }
}

fn yaml_case(src: &str) -> Value {
    match dag::parse_pipeline_yaml(src) {
        Err(e) => json!({"in": src, "stage": "parse", "error": e.to_string()}),
        Ok(def) => match dag::compile_definition(&def) {
            Err(e) => json!({"in": src, "stage": "compile", "def": def, "error": e.to_string()}),
            Ok(d) => json!({"in": src, "stage": "ok", "def": def, "dag": d}),
        },
    }
}

fn main() {
    let mut r = Rng(42);
    let mut yamls: Vec<String> = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir("examples")
        .expect("run from the repository root")
        .filter_map(|e| Some(e.ok()?.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yml"))
        .collect();
    files.sort();
    files.push("fiber.yml".into());
    for f in files {
        yamls.push(std::fs::read_to_string(f).expect("example is readable"));
    }
    let frag = "name: p\nsteps:\n  a:\n    run: x\n";
    for line in [
        "need: [a]",
        "needs: [b]",
        "needs: [a]",
        "needs:",
        "needs: a",
        "env:",
        "env: {A: 1}",
        "env: {A: 0777}",
        "env: {A: yes}",
        "env: {A: 'y', B: ~}",
        "env: {A: true}",
        "env: {A: 1.5}",
        "env: {A: 0x1F}",
        "env: {A: +1}",
        "env: {A: 1e3}",
        "env: {A: .5}",
        "env: {A: 1_000}",
        "env: {A: -07}",
        "env: {A: .inf}",
        "env: {1A: x}",
        "env: {FIBER_X: y}",
        "env: {__proto__: y}",
        "env: {A-B: x}",
        "env: {constructor: y, toString: z}",
        "continue-on-error: true",
        "imgae: x",
        "continue_on_error: 1",
        "continue_on_error:",
        "retries: -1",
        "retries: 2",
        "retries: 1.0",
        "retries:",
        "retries: 4294967296",
        "timeout_minutes: 0",
        "timeout_minutes: 5",
        "working_directory: ../x",
        "working_directory: ' sub '",
        "shell: bash -e",
        "shell: bash",
        "artifacts: [./a]",
        "artifacts: [' a ']",
        "image: '-v'",
        "image: '  '",
        "image: ' node:22 '",
        "matrix: {os: [a, b]}",
        "matrix: {os: []}",
        "matrix: {}",
        "matrix:",
        "matrix: {b: [1]}",
        "matrix: {b: ['1','2'], a: ['x y','é/z']}",
        "matrix: {x-y: [a]}",
        "matrix: {os: [a, a]}",
        "if: matrix.os == 'a'",
        "if: failure()",
        "if: ''",
        "secrets: []",
        "secrets:",
        "secrets: [A]",
        "id: z",
        "<<: {run: y}",
        "x-foo: 1",
        "run: ''",
        "name: 12",
        "name:",
        "labels: [os=linux]",
        "zzzzzz: 1",
        "é: 1",
    ] {
        yamls.push(format!("{frag}    {line}\n"));
    }
    for top in [
        "timeout_minutes: 0",
        "x-anchor: &a [1]",
        "_c: 1",
        "stpes: {}",
        "on: {push: {}}",
        "on:",
        "on: {push: }",
        "on: {pull_request: {types: [opened]}}",
        "on: {pushh: {}}",
        "on: {cron: '0 0 * * * *', interval_minutes: 5}",
        "on: {schedule: x}",
        "on: []",
        "workspace: {repo: 'ext::sh'}",
        "workspace: {repo: ' /tmp/x ', ref: main}",
        "workspace: {}",
        "workspace: {repo: x, ref: 'a..b'}",
        "concurrency: {group: g, cancel_in_progress: true}",
        "concurrency: {}",
        "concurrency: {grp: g}",
        "env: {Z: '1', A: '2'}",
        "name: dup",
        "1: x",
    ] {
        yamls.push(format!("{frag}{top}\n"));
    }
    for s in [
        "",
        "~",
        "[]",
        "a: b",
        "name: p",
        "name: p\nsteps: 3",
        "name: p\nsteps: []",
        "name: p\nsteps:",
        "name: p\nsteps: {}",
        "name: p\nsteps: {1: {run: x}}",
        "name: p\nsteps: {'': {run: x}}",
        "name: p\nsteps: {a: }",
        "name: p\nsteps: [{id: a, run: x}]",
        "name: p\nsteps: [{id: a, name: A, run: x}, {id: a, name: B, run: y}]",
        "name: p\nsteps: [{id: a, name: A, run: x, bogus: 1}]",
        "name: p\nsteps: [{name: A, bogus: 1}]",
        "name: p\nsteps: {a: {run: x, needs: [b]}, b: {run: y, needs: [a]}}",
        "name: p\nsteps: {a: {run: x}, a: {run: y}}",
        "name: 'p\nsteps: {}",
        "name: p\nx-l: &l [os=linux]\nsteps: {a: {run: x, labels: *l}}",
        "a: 1\n---\nb: 2",
        "name: p\nsteps: {m: {run: x, matrix: {v: ['1','2']}}, d: {run: y, needs: [m, m]}, e: {run: z, needs: [d, m]}}",
        "name: p\nsteps: {10: {run: x}}",
        "name: p\nsteps: {b: {run: x}, a: {run: y, needs: [b]}, c: {run: z}}",
    ] {
        yamls.push(s.to_string());
    }
    // Random DAGs: ordering, levels, matrix rewriting.
    for _ in 0..200 {
        let n = 1 + r.below(8);
        let mut s = String::from("name: r\nsteps:\n");
        for i in 0..n {
            s.push_str(&format!("  s{i}:\n    run: x\n"));
            let deps: Vec<String> = (0..n)
                .filter(|j| *j != i && r.below(5) == 0)
                .map(|j| format!("s{j}"))
                .collect();
            s.push_str(&format!("    needs: [{}]\n", deps.join(", ")));
            if r.below(4) == 0 {
                s.push_str("    matrix: {os: [a, b], v: ['1', '2']}\n");
            }
        }
        yamls.push(s);
    }
    let many: String = (0..501).map(|i| format!("  s{i}: {{run: x}}\n")).collect();
    yamls.push(format!("name: p\nsteps:\n{many}"));
    let yaml: Vec<Value> = yamls.iter().map(|y| yaml_case(y)).collect();

    let mut ifs: Vec<String> = [
        "",
        " ",
        "success()",
        "always()",
        "never()",
        "failure()",
        "cancelled( )",
        "matrix.os == 'linux'",
        "matrix.os=='linux'",
        "env.FOO == \"bar\"",
        "os == linux",
        "matrix.os != 'x'",
        "a && b",
        "a || b",
        "${{ x }}",
        "github.ref == 'x'",
        "== 'x'",
        "os ==",
        "os == 'x",
        "os == a b",
        "os == 'a' == 'b'",
        "matrix. == 'x'",
        "a b == 'x'",
        "OS == 'linux'",
        "matrix.OS == linux",
        "FIBER_MATRIX_os == linux",
        "env.foo == bar",
        "os == ''",
        "x() == 'y'",
        "x()",
        "é() ",
        "os == '",
        "\u{feff}always()",
        "\u{85}always()\u{85}",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let alpha = [
        "os", "matrix.", "env.", "FOO", " ", "=", "==", "'", "\"", "linux", "(", ")", ".", "bar",
        "é",
    ];
    for _ in 0..600 {
        ifs.push(r.word(&alpha, 7));
    }
    let env: Vec<(String, String)> = [
        ("FOO", "bar"),
        ("os", "linux"),
        ("MATRIX_OS", "linux"),
        ("FIBER_MATRIX_OS", "linux"),
        ("Zed", ""),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let ifc: Vec<Value> = ifs.iter().map(|e| {
        let ev = |ok| step_if::eval_if(Some(e), &step_if::IfContext { needs_succeeded: ok, env: env.clone() });
        json!({"in": e, "t": ev(true), "f": ev(false), "shape": step_if::validate(Some(e)).err(),
               "key": step_if::validate_key(Some(e), &env).err(), "nokey": step_if::validate_key(Some(e), &[]).err()})
    }).collect();

    // Only the FIBER_MATRIX_ alias, so that lookup arm is load-bearing.
    let alias_env = vec![("FIBER_MATRIX_ARCH".to_string(), "arm".to_string())];
    let alias: Vec<Value> = ["arch == arm", "matrix.ARCH == 'arm'", "env.arc == arm"]
        .iter()
        .map(|e| {
            let ctx = step_if::IfContext { needs_succeeded: true, env: alias_env.clone() };
            json!({"in": e, "t": step_if::eval_if(Some(e), &ctx), "key": step_if::validate_key(Some(e), &alias_env).err()})
        })
        .collect();

    let mut crons: Vec<String> = [
        "0 */5 * * * *",
        "0 0 0 31 2 *",
        "0 0 0 29 2 *",
        "@hourly",
        " @daily ",
        "@weekly",
        "@monthly",
        "@yearly",
        "@never",
        "* * * * *",
        "0 0 9 * * Mon-Fri",
        "0 0 9 * * 1",
        "0 0 9 * * 7",
        "0 0 9 * * 0",
        "0 0 0 13 * 6",
        "0 0 0 ? * ?",
        "? * * * * *",
        "0 0 0 1 Jan *",
        "0 0 0 1 JANUARY-mar/2 *",
        "0 0 0 1 foo *",
        "0 0 0 1,15 * *",
        "0 0 0 1 , 15 * *",
        "0 0 0 1 1 * 2030",
        "0 0 0 1 1 * 2101",
        "0 5/15 * * * *",
        "0 10-20/3 * * * *",
        "0 20-10 * * * *",
        "0 */0 * * * *",
        "0 */60 * * * *",
        "60 * * * * *",
        "******",
        "0 0 0 * * Sunday",
        "0 0 0 * * tues,THURS",
        "0 0 0 * * Mon/2",
        "",
        "0 0 0 L * *",
        "0 0 12 * * * *",
        "0 0 0 1 1 * 1999",
        "\t0 0 0 * * *\n",
        "0 0 0 * * *,",
        "1,2,3 4 5 6 7 *",
        "0 0 0 31 * *",
        "0 0 0 1 sept *",
        "0 0 0 1 june *",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let fld = [
        "*", "*/7", "1", "5", "2-4", "1,3", "10/5", "30", "0", "Mon", "Jan-Mar", "?", "12", "7",
    ];
    for _ in 0..400 {
        let n = if r.below(8) == 0 { 7 } else { 6 };
        crons.push(
            (0..n)
                .map(|_| fld[r.below(fld.len())])
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    let times: Vec<i64> = [
        0,
        1_700_000_000_500,
        1_709_164_799_000,
        1_735_689_599_999,
        1_795_500_000_000,
        4_102_444_799_000,
    ]
    .to_vec();
    let cron: Vec<Value> = crons
        .iter()
        .map(|c| {
            let next: Vec<Value> = times
                .iter()
                .map(|t| {
                    let at = Utc.timestamp_millis_opt(*t).unwrap();
                    json!(schedule::next_from_cron(c, at).map(|d| d.timestamp_millis()))
                })
                .collect();
            let ok = std::str::FromStr::from_str(c)
                .map(|_: cron::Schedule| ())
                .is_ok();
            json!({"in": c, "ok": ok, "next": next})
        })
        .collect();

    let mut globs: Vec<String> = [
        "**",
        "**/*.rs",
        "src/**",
        "src/**/x",
        "*.md",
        "docs/*",
        "a?c",
        "[a-c]x",
        "[!a]b",
        "[^a]b",
        "{a,b}/c",
        "{a,{b,c}}d",
        "a{,b}",
        "{}",
        "{,}",
        "\\*",
        "a\\",
        "[",
        "[z-a]",
        "{a",
        "a}",
        "a/**/b",
        "a/**",
        "**a",
        "a**",
        "/**/",
        "**/**",
        "a/**/**/b",
        "[]a]",
        "[a-]",
        "[-a]",
        "é",
        "?",
        "??",
        "[é]",
        "{src/**,docs}/x",
        "{**/a,b}",
        "a,b",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let ga = [
        "a", "b", "/", "*", "**", "?", "[ab]", "[!a]", "{a,b}", "{a/**,b}", ",", "\\", "é", ".rs",
        "{", "}",
    ];
    for _ in 0..600 {
        globs.push(r.word(&ga, 6));
    }
    let mut paths: Vec<String> = [
        "",
        "a",
        "b",
        "ab",
        "a/b",
        "a/x/b",
        "src/x",
        "src/a/b/x",
        "x.rs",
        "d/x.rs",
        "README.md",
        "docs/a/b",
        "a/c",
        "abc",
        "é",
        "bd",
        "ad",
        "/a",
        "a/",
        "a,b",
        "*",
        "axc",
        "/",
        "src/",
        "bx",
        "-",
        "]",
        "a/b.rs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let pa = ["a", "b", "/", ".rs", "é", ","];
    for _ in 0..30 {
        paths.push(r.word(&pa, 5));
    }
    let glob: Vec<Value> = globs.iter().map(|g| {
        let m: String = paths.iter().map(|p| {
            if path_filter::paths_allow(std::slice::from_ref(p), std::slice::from_ref(g), &[]) { '1' } else { '0' }
        }).collect();
        json!({"in": g, "m": m, "error": path_filter::validate_globs("f", std::slice::from_ref(g)).err()})
    }).collect();

    let sha = "a".repeat(40);
    let dig = format!("x@sha256:{}", "b".repeat(64));
    let mut texts: Vec<String> = [
        "",
        " ",
        "node:22",
        "ghcr.io/a/b:1.2",
        "localhost:5000/x",
        "-v",
        "a b",
        "a//b",
        "a/",
        "a:",
        "a:b:c",
        "x@sha256:abc",
        dig.as_str(),
        "https://github.com/a/b.git",
        "ext::sh -c x",
        "ext::x",
        "fd::3",
        "git@github.com:a/b.git",
        "ssh://-oProxy@h/x",
        "-x@h:y",
        "user@-h:p",
        "https://[2001:db8::1]/r",
        "FILE:///x",
        "ftp://x",
        "http://",
        "/abs/path",
        "../rel",
        sha.as_str(),
        "main",
        "refs/pull/1/head",
        "a..b",
        "a~1",
        "a^",
        "a:b",
        "a?b",
        "x\ty",
        "x\u{85}y",
        "x\u{feff}y",
        "x\u{1}",
        "x\u{9f}",
        "sub/dir",
        "a\\..\\b",
        "C:/x",
        "\\x",
        "..",
        "a/..",
        ".",
        "./a",
        "a/./b",
        "a//b/",
        "a/.",
        "bash",
        "/bin/bash",
        "bash -e",
        "sh.exe",
        "é",
        "\u{85} sub \u{85}",
        "\u{feff}sub",
        "a/b/..c",
        "...",
        "a\u{2028}b",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    texts.push("a".repeat(33));
    texts.push("é".repeat(128));
    texts.push("a".repeat(1025));
    let va = [
        "a", "/", ".", "..", ":", "@", "-", " ", "\\", "git", "://", "http", "é", "1", "~",
        "\u{85}",
    ];
    for _ in 0..1500 {
        texts.push(r.word(&va, 7));
    }
    let valid: Vec<Value> = texts
        .iter()
        .map(|s| {
            let b = [
                validate::image_reference_ok(s),
                validate::repo_url_ok(s),
                validate::commit_sha_ok(s),
                validate::git_ref_ok(s),
                validate::working_directory_ok(s),
                validate::shell_ok(s),
                validate::artifact_path_ok(s),
            ];
            json!({"in": s, "m": b.iter().map(|x| if *x { '1' } else { '0' }).collect::<String>()})
        })
        .collect();

    let trunc: Vec<Value> = [
        ("a", 65536),
        ("a", 65537),
        ("é", 32768),
        ("é", 40000),
        ("€", 30000),
        ("😀", 20000),
        ("a😀", 13200),
    ]
    .iter()
    .map(|(c, n)| {
        let out = limits::truncate_log_line(&c.repeat(*n)).into_owned();
        json!({"unit": c, "n": n, "bytes": out.len(), "chars": out.chars().count()})
    })
    .collect();
    let roles: Vec<Value> = [
        "reader",
        " Writer ",
        "ADMIN",
        "owner",
        "root",
        "",
        "constructor",
        "\u{85}owner",
    ]
    .iter()
    .map(|s| json!({"in": s, "out": ProjectRole::parse(s).map(|r| r.as_str())}))
    .collect();

    let out = json!({"times": times, "paths": paths, "ifEnv": env, "yaml": yaml, "if": ifc, "alias": alias, "aliasEnv": alias_env, "cron": cron,
        "glob": glob, "validate": valid, "truncate": trunc, "roles": roles});
    println!("{}", serde_json::to_string(&out).unwrap());
}
