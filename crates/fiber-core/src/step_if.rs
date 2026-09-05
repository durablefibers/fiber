//! Step `if:` expression evaluation (GitHub Actions-inspired subset).

/// Context available when deciding whether to queue or skip a step.
#[derive(Debug, Clone, Default)]
pub struct IfContext {
    /// All `needs` completed with Succeeded (caller typically only queues then).
    pub needs_succeeded: bool,
    /// Matrix / step env bindings (`os` → `linux`, also `MATRIX_OS`).
    pub env: Vec<(String, String)>,
}

/// Evaluate a step condition. Empty / missing → `success()`.
///
/// Supported:
/// - `always()` — always run (when dependencies allow queueing)
/// - `never()` — always skip
/// - `success()` — run when needs succeeded (default)
/// - `matrix.<key> == 'value'` / `env.<KEY> == 'value'` — equality against matrix/env
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

fn eval_equality(expr: &str, ctx: &IfContext) -> Option<bool> {
    // `matrix.os == 'linux'` or `env.FOO == "bar"`
    let (left, right) = split_eq(expr)?;
    let left = left.trim();
    let right = unquote(right.trim())?;
    let key = left
        .strip_prefix("matrix.")
        .or_else(|| left.strip_prefix("env."))
        .unwrap_or(left);
    let val = lookup_env(ctx, key)?;
    Some(val == right)
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

fn lookup_env(ctx: &IfContext, key: &str) -> Option<String> {
    let upper = format!("MATRIX_{}", key.to_ascii_uppercase());
    for (k, v) in &ctx.env {
        if k == key || k.eq_ignore_ascii_case(&upper) || k == &format!("MATRIX_{key}") {
            return Some(v.clone());
        }
        // FIBER_MATRIX_os style
        if k.eq_ignore_ascii_case(&format!("FIBER_MATRIX_{key}")) {
            return Some(v.clone());
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
