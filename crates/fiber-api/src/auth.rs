use crate::state::AppState;
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use fiber_core::Agent;
use fiber_core::PublicUser;

pub struct AuthUser(pub PublicUser);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_from_headers(&parts.headers)
            .ok_or((StatusCode::UNAUTHORIZED, "missing bearer token"))?;
        let user = state
            .store
            .user_by_session_token(&token)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "auth error"))?
            .ok_or((StatusCode::UNAUTHORIZED, "invalid session"))?;
        Ok(AuthUser(user))
    }
}

/// Agent token auth (same token used for `/ws/agent?token=`).
pub struct AuthAgent(pub Agent);

impl FromRequestParts<AppState> for AuthAgent {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = agent_token_from_parts(parts)
            .ok_or((StatusCode::UNAUTHORIZED, "missing agent token"))?;
        let agent = state
            .store
            .agent_by_token(&token)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "auth error"))?
            .ok_or((StatusCode::UNAUTHORIZED, "invalid agent token"))?;
        Ok(AuthAgent(agent))
    }
}

fn agent_token_from_parts(parts: &Parts) -> Option<String> {
    if let Some(t) = bearer_from_headers(&parts.headers) {
        return Some(t);
    }
    let query = parts.uri.query()?;
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        if k == "token" {
            return Some(v.into_owned());
        }
    }
    None
}

pub fn bearer_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        h
    }

    fn parts_with(uri: &str, auth: Option<&str>) -> Parts {
        let mut req = axum::http::Request::builder().uri(uri);
        if let Some(a) = auth {
            req = req.header(AUTHORIZATION, a);
        }
        req.body(()).unwrap().into_parts().0
    }

    #[test]
    fn a_bearer_token_is_extracted_in_either_case() {
        assert_eq!(
            bearer_from_headers(&headers("Bearer abc")).as_deref(),
            Some("abc")
        );
        assert_eq!(
            bearer_from_headers(&headers("bearer abc")).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_from_the_token() {
        assert_eq!(
            bearer_from_headers(&headers("Bearer   abc  ")).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn a_missing_or_non_bearer_header_yields_nothing() {
        assert_eq!(bearer_from_headers(&HeaderMap::new()), None);
        // Basic and the bare token must not be mistaken for a session token.
        assert_eq!(bearer_from_headers(&headers("Basic abc")), None);
        assert_eq!(bearer_from_headers(&headers("abc")), None);
        assert_eq!(bearer_from_headers(&headers("Bearer")), None);
        // The prefix is matched with its space; "Bearerabc" is not a bearer header.
        assert_eq!(bearer_from_headers(&headers("Bearerabc")), None);
    }

    #[test]
    fn an_empty_bearer_token_is_not_treated_as_absent() {
        // It reaches the store as "" and fails the lookup there rather than falling
        // through to another credential source.
        assert_eq!(
            bearer_from_headers(&headers("Bearer ")).as_deref(),
            Some("")
        );
    }

    #[test]
    fn an_agent_prefers_the_header_over_the_query_token() {
        let parts = parts_with("/ws/agent?token=from-query", Some("Bearer from-header"));
        assert_eq!(
            agent_token_from_parts(&parts).as_deref(),
            Some("from-header")
        );
    }

    #[test]
    fn an_agent_token_falls_back_to_the_query_string() {
        // Browsers and the agent's WS handshake cannot set headers, hence ?token=.
        let parts = parts_with("/ws/agent?token=abc123", None);
        assert_eq!(agent_token_from_parts(&parts).as_deref(), Some("abc123"));
    }

    #[test]
    fn a_percent_encoded_query_token_is_decoded() {
        let parts = parts_with("/ws/agent?token=a%2Bb%3Dc", None);
        assert_eq!(agent_token_from_parts(&parts).as_deref(), Some("a+b=c"));
    }

    #[test]
    fn an_unrelated_query_parameter_is_not_mistaken_for_a_token() {
        assert_eq!(
            agent_token_from_parts(&parts_with("/ws/agent?id=abc", None)),
            None
        );
        assert_eq!(agent_token_from_parts(&parts_with("/ws/agent", None)), None);
    }
}
