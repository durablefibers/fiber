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
