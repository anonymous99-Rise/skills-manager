//! Gitea OAuth2 login + HMAC-signed cookie sessions.

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Deserialize;
use sha2::Sha256;
use std::sync::Arc;

use crate::AppState;

const SESSION_COOKIE: &str = "sm_session";
const SESSION_TTL_SECS: i64 = 7 * 24 * 3600;

pub struct AuthService {
    pub gitea_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub allowed_users: Option<Vec<String>>,
    pub dev_no_auth: bool,
    secret: Vec<u8>,
}

#[derive(Deserialize)]
pub struct SessionUser {
    pub login: String,
    pub exp: i64,
}

pub struct AuthError;

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            "Not signed in. Open /api/auth/gitea/login to sign in.",
        )
            .into_response()
    }
}

impl AuthService {
    pub fn new() -> Self {
        let gitea_url = std::env::var("GITEA_URL").unwrap_or_else(|_| "https://git.nxwysoft.com".into());
        let client_id = std::env::var("GITEA_CLIENT_ID").unwrap_or_default();
        let client_secret = std::env::var("GITEA_CLIENT_SECRET").unwrap_or_default();
        let allowed_users = std::env::var("SM_ALLOWED_USERS").ok().and_then(|csv| {
            let users: Vec<String> = csv
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if users.is_empty() { None } else { Some(users) }
        });
        let dev_no_auth = std::env::var("SM_DEV_NO_AUTH").ok().as_deref() == Some("1");

        let secret = match std::env::var("SM_SESSION_SECRET") {
            Ok(s) if !s.is_empty() => s.into_bytes(),
            _ => load_or_create_secret(),
        };

        Self { gitea_url, client_id, client_secret, allowed_users, dev_no_auth, secret }
    }

    fn callback_url(&self) -> String {
        match std::env::var("SM_PUBLIC_URL") {
            Ok(base) if !base.is_empty() => {
                format!("{}/api/auth/gitea/callback", base.trim_end_matches('/'))
            }
            _ => "/api/auth/gitea/callback".to_string(),
        }
    }

    pub fn sign(&self, payload: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret).expect("hmac key");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn issue(&self, login: &str) -> String {
        let exp = chrono::Utc::now().timestamp() + SESSION_TTL_SECS;
        let payload = format!("{}.{}", login, exp);
        format!("{}.{}", payload, self.sign(&payload))
    }

    /// Verify a cookie value; returns the session user.
    pub fn verify(&self, cookie_value: &str) -> Result<SessionUser, AuthError> {
        let (payload, sig) = cookie_value.rsplit_once('.').ok_or(AuthError)?;
        if !constant_time_eq(self.sign(payload).as_bytes(), sig.as_bytes()) {
            return Err(AuthError);
        }
        let mut parts = payload.splitn(2, '.');
        let login = parts.next().ok_or(AuthError)?;
        let exp: i64 = parts.next().ok_or(AuthError)?.parse().map_err(|_| AuthError)?;
        if chrono::Utc::now().timestamp() > exp {
            return Err(AuthError);
        }
        if let Some(allowed) = &self.allowed_users {
            if !allowed.iter().any(|u| u == login) {
                return Err(AuthError);
            }
        }
        Ok(SessionUser { login: login.to_string(), exp })
    }

    /// Extract + verify the session from request headers.
    pub fn session_from_headers(&self, headers: &HeaderMap) -> Result<SessionUser, AuthError> {
        if self.dev_no_auth {
            return Ok(SessionUser { login: "dev".into(), exp: i64::MAX / 2 });
        }
        let cookies = headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .ok_or(AuthError)?;
        for pair in cookies.split(';') {
            let pair = pair.trim();
            if let Some(value) = pair.strip_prefix(&format!("{SESSION_COOKIE}=")) {
                return self.verify(value);
            }
        }
        Err(AuthError)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn load_or_create_secret() -> Vec<u8> {
    let path = core::central_repo::home_base_dir().join(".session-secret");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            return existing.into_bytes();
        }
    }
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let secret = hex::encode(bytes);
    let _ = std::fs::write(&path, &secret);
    secret.into_bytes()
}

mod core {
    pub use crate::core::central_repo;
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/gitea/login", get(gitea_login))
        .route("/gitea/callback", get(gitea_callback))
        .route("/gitea/token", post(gitea_token_login))
        .route("/login", get(login_page))
        .route("/me", get(me))
        .route("/logout", post(logout))
        .route("/status", get(status))
}

use axum::routing::post;

/// PAT fallback login: the Gitea instance's OAuth token endpoint currently
/// rejects every client secret (instance-side issue), so users can sign in by
/// pasting a Gitea access token (Settings → Applications → Access Tokens).
/// The token is validated against /api/v1/user and only the session cookie is
/// kept — the token itself is not stored.
#[derive(Deserialize)]
struct TokenLogin {
    token: String,
}

async fn gitea_token_login(
    State(state): State<Arc<AppState>>,
    body: Option<Json<TokenLogin>>,
) -> Response {
    let Some(Json(body)) = body else {
        return (StatusCode::BAD_REQUEST, "missing token").into_response();
    };
    let token = body.token.trim().to_string();
    if token.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty token").into_response();
    }
    let auth = &state.auth;

    #[derive(Deserialize)]
    struct GiteaUser {
        login: String,
    }
    let client = reqwest::Client::new();
    let user: GiteaUser = match client
        .get(format!("{}/api/v1/user", auth.gitea_url))
        .header("Authorization", format!("token {token}"))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => match r.json().await {
            Ok(u) => u,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("user parse failed: {e}"),
                )
                    .into_response()
            }
        },
        Ok(r) => {
            return (
                StatusCode::UNAUTHORIZED,
                format!("Gitea rejected this token (HTTP {})", r.status()),
            )
                .into_response()
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("cannot reach Gitea: {e}"),
            )
                .into_response()
        }
    };

    if let Some(allowed) = &auth.allowed_users {
        if !allowed.iter().any(|u| *u == user.login) {
            return (
                StatusCode::FORBIDDEN,
                format!("user {} is not allowed on this deployment", user.login),
            )
                .into_response();
        }
    }

    let cookie_value = auth.issue(&user.login);
    let mut resp = (StatusCode::OK, format!("signed in as {}", user.login)).into_response();
    let cookie = format!(
        "{SESSION_COOKIE}={cookie_value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL_SECS}"
    );
    resp.headers_mut().append(header::SET_COOKIE, cookie.parse().unwrap());
    log::info!("gitea token login: {}", user.login);
    resp
}

async fn login_page() -> Response {
    let gitea_url = std::env::var("GITEA_URL").unwrap_or_default();
    let html = format!(
        r#"<!doctype html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>登录 · Skills Manager</title>
<style>
  body {{ font-family: system-ui, sans-serif; background: #0d1117; color: #e6edf3;
         display: flex; align-items: center; justify-content: center; min-height: 100vh; margin: 0; }}
  .card {{ background: #161b22; border: 1px solid #30363d; border-radius: 12px;
           padding: 32px; width: 380px; }}
  h1 {{ font-size: 18px; margin: 0 0 20px; }}
  button, input {{ width: 100%; box-sizing: border-box; padding: 10px 12px; border-radius: 8px;
        font-size: 14px; margin-bottom: 12px; }}
  button {{ background: #238636; color: #fff; border: none; cursor: pointer; font-weight: 600; }}
  button.ghost {{ background: transparent; border: 1px solid #30363d; color: #e6edf3; }}
  input {{ background: #0d1117; border: 1px solid #30363d; color: #e6edf3; }}
  .hint {{ color: #8b949e; font-size: 12px; line-height: 1.6; margin-top: 8px; }}
  a {{ color: #58a6ff; }}
  #msg {{ color: #f85149; font-size: 13px; min-height: 18px; }}
</style>
</head>
<body>
<div class="card">
  <h1>Skills Manager — 登录</h1>
  <button onclick="location='/api/auth/gitea/login'">使用 Gitea OAuth 登录</button>
  <div class="hint">OAuth 端点若被服务端禁用,请用下方令牌方式。</div>
  <hr style="border-color:#30363d;margin:16px 0">
  <input id="tok" type="password" placeholder="粘贴 Gitea 访问令牌 (PAT)" autocomplete="off">
  <button onclick="tokenLogin()">使用访问令牌登录</button>
  <div id="msg"></div>
  <div class="hint">令牌生成:{gitea_url} → 右上角头像 → 设置 → 应用 → 生成新令牌。
  令牌仅用于本次登录校验,不会被保存。</div>
</div>
<script>
async function tokenLogin() {{
  const t = document.getElementById('tok').value.trim();
  const msg = document.getElementById('msg');
  if (!t) {{ msg.textContent = '请输入令牌'; return; }}
  const r = await fetch('/api/auth/gitea/token', {{
    method: 'POST', headers: {{'Content-Type': 'application/json'}},
    body: JSON.stringify({{ token: t }})
  }});
  if (r.ok) {{ location = '/'; }} else {{ msg.textContent = await r.text(); }}
}}
</script>
</body>
</html>"#,
        gitea_url = gitea_url
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

async fn gitea_login(State(state): State<Arc<AppState>>) -> Response {
    let auth = &state.auth;
    // Short-lived state value binds the browser session to this flow.
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let state_value = hex::encode(bytes);
    let redirect = format!(
        "{}/login/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&state={state_value}",
        auth.gitea_url,
        urlencoding::encode(&auth.client_id),
        urlencoding::encode(&auth.callback_url()),
    );
    let mut resp = Redirect::to(&redirect).into_response();
    // State cookie: signed so a forged callback state fails.
    let signed = format!("{state_value}.{}", auth.sign(&state_value));
    let cookie = format!(
        "{}_oauth={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600",
        SESSION_COOKIE, signed
    );
    resp.headers_mut().append(header::SET_COOKIE, cookie.parse().unwrap());
    resp
}

async fn gitea_callback(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Response {
    let auth = &state.auth;
    let Some(code) = params.code else {
        return (StatusCode::BAD_REQUEST, "missing code").into_response();
    };

    // Verify state against the oauth cookie.
    let expected_prefix = format!("{SESSION_COOKIE}_oauth=");
    let state_ok = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|pair| {
                let pair = pair.trim();
                let value = pair.strip_prefix(&expected_prefix)?;
                let (value, sig) = value.rsplit_once('.')?;
                (auth.sign(value) == sig && params.state.as_deref() == Some(value)).then_some(())
            })
        })
        .is_some();
    if !state_ok {
        return (StatusCode::BAD_REQUEST, "invalid OAuth state").into_response();
    }

    // Exchange the code for an access token.
    let client = reqwest::Client::new();
    let token_resp = client
        .post(format!("{}/login/oauth/access_token", auth.gitea_url))
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", auth.client_id.as_str()),
            ("client_secret", auth.client_secret.as_str()),
            ("code", code.as_str()),
            (
                "redirect_uri",
                auth.callback_url().as_str(),
            ),
        ])
        .send()
        .await;
    let token_resp = match token_resp {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("token exchange failed: {e}")).into_response(),
    };
    if !token_resp.status().is_success() {
        let status = token_resp.status();
        let body = token_resp.text().await.unwrap_or_default();
        log::warn!("gitea token exchange failed: HTTP {status} body={body}");
        return (
            StatusCode::BAD_GATEWAY,
            format!("token exchange failed: HTTP {status} — {body}"),
        )
            .into_response();
    }
    #[derive(Deserialize)]
    struct TokenErr {
        error: Option<String>,
        error_description: Option<String>,
    }
    #[derive(Deserialize)]
    struct TokenResp {
        access_token: Option<String>,
    }
    let token: TokenResp = match token_resp.json().await {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("token parse failed: {e}")).into_response(),
    };
    let Some(access_token) = token.access_token else {
        return (StatusCode::BAD_GATEWAY, "no access_token from Gitea").into_response();
    };

    // Fetch the user profile.
    #[derive(Deserialize)]
    struct GiteaUser {
        login: String,
    }
    let user: GiteaUser = match client
        .get(format!("{}/api/v1/user", auth.gitea_url))
        .header("Authorization", format!("Bearer {access_token}"))
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => match r.json().await {
            Ok(u) => u,
            Err(e) => return (StatusCode::BAD_GATEWAY, format!("user parse failed: {e}")).into_response(),
        },
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("user fetch failed: {e}")).into_response(),
    };

    if let Some(allowed) = &auth.allowed_users {
        if !allowed.iter().any(|u| *u == user.login) {
            return (
                StatusCode::FORBIDDEN,
                format!("user {} is not allowed on this deployment", user.login),
            )
                .into_response();
        }
    }

    let cookie_value = auth.issue(&user.login);
    let cookie = format!(
        "{SESSION_COOKIE}={cookie_value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL_SECS}"
    );
    let mut resp = Redirect::to("/").into_response();
    resp.headers_mut().append(header::SET_COOKIE, cookie.parse().unwrap());
    // Clear the oauth state cookie.
    let clear = format!("{SESSION_COOKIE}_oauth=; Path=/; HttpOnly; Max-Age=0");
    resp.headers_mut().append(header::SET_COOKIE, clear.parse().unwrap());
    log::info!("gitea login: {}", user.login);
    resp
}

async fn me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AuthError> {
    let session = state.auth.session_from_headers(&headers)?;
    Ok(Json(serde_json::json!({ "login": session.login })))
}

async fn logout() -> Response {
    let cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; Max-Age=0");
    let mut resp = (StatusCode::OK, "signed out").into_response();
    resp.headers_mut().append(header::SET_COOKIE, cookie.parse().unwrap());
    resp
}

async fn status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "gitea_configured": !state.auth.client_id.is_empty() && !state.auth.client_secret.is_empty(),
        "gitea_url": state.auth.gitea_url,
    }))
}
