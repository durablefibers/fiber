//! Step `if:` expression evaluation (GitHub Actions-inspired subset).

/// Context available when deciding whether to queue or skip a step.
#[derive(Debug, Clone, Default)]
pub struct IfContext {
    /// All `needs` completed with Succeeded (caller typically only queues then).
    pub needs_succeeded: bool,
    /// Matrix / step env bindings (`os` → `linux`, also `MATRIX_OS`).
    pub env: Vec<(String, String)>,
}

/// Everything the `if:` grammar accepts, for error messages and for the docs to quote.
pub const SUPPORTED_IF: &str =
    "`success()`, `always()`, `never()`, or `<matrix|env>.<key> == '<value>'`";

/// Evaluate a step condition. Empty / missing → `success()`.
///
/// Supported:
/// - `always()` — always run (when dependencies allow queueing)
/// - `never()` — always skip
/// - `success()` — run when needs succeeded (default)
/// - `matrix.<key> == 'value'` / `env.<KEY> == 'value'` — equality against matrix/env
///
/// Anything else is rejected by [`validate`] when the pipeline compiles, so an expression
/// that reaches here has already been understood. The `false` fallbacks below are what a
/// definition stored before that validation existed still evaluates to.
pub fn eval_if(expr: Option<&str>, ctx: &IfContext) -> bool {
    let expr = expr
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("success()");
    match expr {
        "always()" => true,
        "never()" => false,
        "success()" => ctx.needs_succeeded,
        _ => eval_equality(expr, ctx).unwrap_or(false),
    }
}

/// Reject an `if:` this crate cannot evaluate, naming what went wrong.
///
/// Without this, a typo or a GitHub Actions expression (`!=`, `&&`, `github.event_name`)
/// falls through [`eval_if`]'s equality arm, yields `None`, and becomes `false` — so the
/// step is skipped on every run of every pipeline for as long as nobody re-reads the YAML.
/// A condition that cannot be evaluated is a mistake in the definition, and the compiler
/// is the only place that can say so while the author is still looking.
///
/// This validates the *shape*, not the outcome: `matrix.os == 'plan9'` is well-formed and
/// simply never true. `!=`, `&&` and `||` are refused rather than implemented — a boolean
/// grammar is a feature with its own precedence and truthiness questions (roadmap R-1),
/// and silently accepting half of one is how this finding happened.
pub fn validate(expr: Option<&str>) -> Result<(), String> {
    let Some(expr) = expr.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if expr.contains("${{") {
        return Err(format!(
            "if: `{expr}` — drop the `${{{{ }}}}` wrapper; a Fiber condition is a bare \
             expression ({SUPPORTED_IF})"
        ));
    }
    for op in ["!=", "&&", "||", ">=", "<=", "=~"] {
        if expr.contains(op) {
            return Err(format!(
                "if: `{expr}` — the `{op}` operator is not supported; use {SUPPORTED_IF}"
            ));
        }
    }
    if matches!(expr, "always()" | "never()" | "success()") {
        return Ok(());
    }
    // A call this crate does not implement. Catching it here (rather than letting the
    // equality arm fail) is what turns `failure()` into a sentence instead of a skip.
    if expr.ends_with(')')
        && let Some(open) = expr.find('(')
    {
        let name = &expr[..open];
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!(
                "if: `{expr}` — unknown function `{name}()`; supported: {SUPPORTED_IF}"
            ));
        }
    }
    let Some((left, right)) = split_eq(expr) else {
        return Err(format!(
            "if: `{expr}` — not a condition this server understands; use {SUPPORTED_IF}"
        ));
    };
    if right.contains("==") {
        return Err(format!(
            "if: `{expr}` — more than one `==`; a condition compares exactly two values"
        ));
    }
    let left = left.trim();
    if left.is_empty() {
        return Err(format!("if: `{expr}` — nothing on the left of `==`"));
    }
    if let Some((context, key)) = left.split_once('.') {
        if !matches!(context, "matrix" | "env") {
            return Err(format!(
                "if: `{expr}` — unknown context `{context}`; only `matrix.` and `env.` are \
                 available to a step condition"
            ));
        }
        if !is_key(key) {
            return Err(format!(
                "if: `{expr}` — `{context}.{key}` is not a usable {context} name"
            ));
        }
    } else if !is_key(left) {
        return Err(format!(
            "if: `{expr}` — `{left}` is not a matrix or env name; write \
             `matrix.{left} == '...'` if that is what you meant"
        ));
    }
    let right = right.trim();
    if right.is_empty() {
        return Err(format!("if: `{expr}` — nothing on the right of `==`"));
    }
    let quoted = |q: char| right.starts_with(q) && right.ends_with(q) && right.len() >= 2;
    if !quoted('\'') && !quoted('"') {
        if right.contains('\'') || right.contains('"') {
            return Err(format!(
                "if: `{expr}` — the value `{right}` has an unbalanced quote"
            ));
        }
        if right.contains(char::is_whitespace) {
            return Err(format!(
                "if: `{expr}` — quote the value: `{left} == '{right}'`"
            ));
        }
    }
    Ok(())
}

/// A matrix axis or env name: what the YAML can actually bind.
fn is_key(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn eval_equality(expr: &str, ctx: &IfContext) -> Option<bool> {
    // `matrix.os == 'linux'` or `env.FOO == "bar"`
    let (left, right) = split_eq(expr)?;
    let right = unquote(right.trim())?;
    let val = lookup_env(&ctx.env, referenced_key(left)?)?;
    Some(*val == right)
}

/// The matrix/env name an equality reads, with any `matrix.` / `env.` prefix stripped.
/// `None` for anything that is not an equality — `always()` reads nothing.
fn referenced_key(left: &str) -> Option<&str> {
    let left = left.trim();
    if left.is_empty() {
        return None;
    }
    Some(
        left.strip_prefix("matrix.")
            .or_else(|| left.strip_prefix("env."))
            .unwrap_or(left),
    )
}

/// Reject a condition that reads a name this step does not have.
///
/// [`validate`] checks the shape; this checks the one thing left that makes a condition
/// silently false forever. `matrix.osx == 'linux'` on a step whose axis is `os` is
/// well-formed, compiles, and skips the step on every run — the original finding, one
/// typo further in. It is statically decidable: `eval_if` only ever sees
/// `CompiledStep.env`, which is the pipeline env, the step env and the cell's matrix
/// bindings, all known when the pipeline compiles. Save-time only, like the rest.
pub fn validate_key(expr: Option<&str>, env: &[(String, String)]) -> Result<(), String> {
    let Some(expr) = expr.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if matches!(expr, "always()" | "never()" | "success()") {
        return Ok(());
    }
    let Some((left, _)) = split_eq(expr) else {
        return Ok(());
    };
    let Some(key) = referenced_key(left) else {
        return Ok(());
    };
    if lookup_env(env, key).is_some() {
        return Ok(());
    }
    Err(format!(
        "if: `{expr}` — `{key}` is not a matrix axis or an env name on this step{}",
        available(env)
    ))
}

/// `; available: a, b, c` — the names an author can actually compare against.
///
/// The generated `MATRIX_*` / `FIBER_MATRIX_*` aliases are left out: they all resolve,
/// but listing three spellings of one axis makes the message harder to read, not easier.
fn available(env: &[(String, String)]) -> String {
    let mut names: Vec<&str> = env
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| !k.starts_with("MATRIX_") && !k.starts_with("FIBER_MATRIX_"))
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return "; this step has no matrix axes and no env".into();
    }
    format!("; available: {}", names.join(", "))
}

fn split_eq(expr: &str) -> Option<(&str, &str)> {
    let idx = expr.find("==")?;
    Some((&expr[..idx], &expr[idx + 2..]))
}

fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    if ((s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')))
        && s.len() >= 2
    {
        return Some(s[1..s.len() - 1].to_string());
    }
    Some(s.to_string())
}

/// The value `key` resolves to, under exactly the rules `eval_if` applies.
///
/// Shared with [`validate_key`] on purpose: a compile-time check that disagreed with the
/// runtime lookup would reject a working pipeline or pass a broken one.
fn lookup_env<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a String> {
    let upper = format!("MATRIX_{}", key.to_ascii_uppercase());
    for (k, v) in env {
        if k == key || k.eq_ignore_ascii_case(&upper) || k == &format!("MATRIX_{key}") {
            return Some(v);
        }
        // FIBER_MATRIX_os style
        if k.eq_ignore_ascii_case(&format!("FIBER_MATRIX_{key}")) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_success() {
        assert!(eval_if(
            None,
            &IfContext {
                needs_succeeded: true,
                env: vec![]
            }
        ));
        assert!(!eval_if(
            None,
            &IfContext {
                needs_succeeded: false,
                env: vec![]
            }
        ));
    }

    #[test]
    fn never_always() {
        assert!(!eval_if(
            Some("never()"),
            &IfContext {
                needs_succeeded: true,
                env: vec![]
            }
        ));
        assert!(eval_if(
            Some("always()"),
            &IfContext {
                needs_succeeded: false,
                env: vec![]
            }
        ));
    }

    #[test]
    fn validate_accepts_what_eval_understands() {
        for ok in [
            None,
            Some(""),
            Some("   "),
            Some("always()"),
            Some("never()"),
            Some("success()"),
            Some("matrix.os == 'linux'"),
            Some("env.CHANNEL == \"nightly\""),
            Some("os == 'linux'"),
            Some("matrix.rust-version == '1.98'"),
        ] {
            assert!(validate(ok).is_ok(), "{ok:?} should validate");
        }
    }

    /// Every one of these used to compile and then skip the step on every run.
    #[test]
    fn validate_rejects_what_eval_would_silently_call_false() {
        let cases = [
            ("matrix.os != 'windows'", "!="),
            ("success() && matrix.os == 'linux'", "&&"),
            ("matrix.os == 'linux' || matrix.os == 'mac'", "||"),
            ("failure()", "failure()"),
            ("cancelled()", "cancelled()"),
            ("${{ success() }}", "${{"),
            ("github.event_name == 'push'", "github"),
            ("succes()", "succes()"),
            ("matrix.os", "understands"),
            ("== 'linux'", "left"),
            ("matrix.os ==", "right"),
            ("matrix.os == 'a' == 'b'", "more than one"),
            ("matrix.os == 'lin ux", "unbalanced"),
            ("matrix.os == lin ux", "quote the value"),
        ];
        for (expr, needle) in cases {
            let err = validate(Some(expr)).expect_err(&format!("{expr} must be rejected"));
            assert!(
                err.contains(needle),
                "error for `{expr}` should mention `{needle}`, got: {err}"
            );
        }
    }

    #[test]
    fn matrix_eq() {
        let ctx = IfContext {
            needs_succeeded: true,
            env: vec![
                ("os".into(), "linux".into()),
                ("MATRIX_OS".into(), "linux".into()),
            ],
        };
        assert!(eval_if(Some("matrix.os == 'linux'"), &ctx));
        assert!(!eval_if(Some("matrix.os == 'windows'"), &ctx));
    }
}
