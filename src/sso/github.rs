use crate::{
    authentication::token::{TokenClaims, TokenConfig, TokenError},
    config::GlobalConfig,
    database::Database,
    error::{self, Error},
    extract::Query,
    http::HttpClient,
    model::{Response, Status},
    session::{Scope, SessionClaims, SessionResponse},
    user::{Connection, UserDocument, UserError},
    utils, Result,
};

use super::{oauth::StateClaims, SsoError};

use axum::{
    extract::{Extension, TypedHeader},
    response::{IntoResponse, Redirect},
};

use headers::{Cookie, HeaderMap, HeaderValue};
use http::{
    header::{ACCEPT, AUTHORIZATION, SET_COOKIE},
    StatusCode,
};
use hyper::Uri;
use mongodb::bson::doc;
use reqwest::IntoUrl;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use url::Url;

#[derive(Debug, thiserror::Error)]
pub enum GitHubError {
    #[error("access token error: {0}")]
    TokenAccess(#[from] TokenAccessError),
    #[error("unknown error")]
    UnknownError,
}

impl error::ErrorResponse for GitHubError {
    type Response = Status;

    fn status_code(&self) -> StatusCode {
        match self {
            GitHubError::TokenAccess(e) => match e {
                TokenAccessError::BadVerificationCode | TokenAccessError::RedirectUriMismatch => {
                    StatusCode::UNAUTHORIZED
                }
                TokenAccessError::IncorrectClientCredentials => StatusCode::INTERNAL_SERVER_ERROR,
            },
            GitHubError::UnknownError => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> Self::Response {
        Status::new(self.status_code(), self.to_string())
    }
}

#[derive(Debug, thiserror::Error, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenAccessError {
    #[error("code passed is incorrect or expired")]
    BadVerificationCode,
    #[error("redirect_uri MUST match the registered callback URL for this application")]
    RedirectUriMismatch,
    #[error("client_id and/or client_secret passed are incorrect")]
    IncorrectClientCredentials,
}

#[derive(Debug, Clone)]
pub struct GitHub {
    client_id: String,
    client_secret: String,
    redirect_uri: Url,
    client: HttpClient,
}

impl GitHub {
    pub fn new<U>(
        client_id: String,
        client_secret: String,
        redirect: U,
        client: HttpClient,
    ) -> Result<Self>
    where
        U: IntoUrl,
    {
        Ok(Self {
            client_id,
            client_secret,
            redirect_uri: redirect.into_url()?,
            client,
        })
    }

    async fn get_access_token(&self, code: &str) -> Result<TokenResponse> {
        let url = Url::parse("https://github.com/login/oauth/access_token").unwrap();
        let form = TokenRequest {
            client_id: &self.client_id,
            client_secret: &self.client_secret,
            code,
            redirect_uri: &self.redirect_uri,
        };

        let res = self
            .client
            .post(url)
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .form(&form)
            .send()
            .await?;
        let body = res.error_for_status()?.json::<TokenResponse>().await?;

        if let Some(e) = body.error {
            return Err(SsoError::from(GitHubError::TokenAccess(e)).into());
        }

        Ok(body)
    }

    async fn get_current_user(&self, access_token: &str) -> Result<User> {
        let path = "/user";
        let res = self.api_get(path, access_token).await?;

        Ok(res)
    }

    async fn get_emails(&self, access_token: &str) -> Result<Vec<Email>> {
        let path = "/user/emails";
        let res = self.api_get(path, access_token).await?;

        Ok(res)
    }

    #[inline]
    async fn api_get<T>(&self, path: &str, access_token: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let url = Url::parse("https://api.github.com")
            .unwrap()
            .join(path)
            .unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github.v3+json"),
        );
        headers.insert(
            AUTHORIZATION,
            format!("token {}", access_token).parse().unwrap(),
        );

        let res = self.client.get(url).headers(headers).send().await?;
        let body = res.error_for_status()?.json().await?;

        Ok(body)
    }
}

#[derive(Debug, Serialize)]
struct TokenRequest<'a> {
    client_id: &'a str,
    client_secret: &'a str,
    code: &'a str,
    redirect_uri: &'a Url,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TokenType {
    Bearer,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    token_type: Option<TokenType>,
    scope: Option<String>,

    #[serde(flatten)]
    error: Option<TokenAccessError>,
}

#[derive(Debug, Deserialize)]
struct User {
    login: String,
    id: i64,
    // node_id: String,
    // avatar_url: Url,
    // gravatar_id: String,
    // url: Url,
    // html_url: Url,
    // followers_url: Url,
    // following_url: Url,
    // gists_url: Url,
    // starred_url: Url,
    // subscriptions_url: Url,
    // organizations_url: Url,
    // repos_url: Url,
    // events_url: Url,
    // received_events_url: Url,
    // r#type: String,
    // site_admin: bool,
    // name: String,
    // company: String,
    // blog: String,
    // location: String,
    // email: String,
    // hireable: bool,
    // bio: String,
    // twitter_username: String,
    // public_repos: i64,
    // public_gists: i64,
    // followers: i64,
    // following: i64,
    // created_at: String,
    // updated_at: String,
    // private_gists: i64,
    // total_private_repos: i64,
    // owned_private_repos: i64,
    // disk_usage: i64,
    // collaborators: i64,
    two_factor_authentication: bool,
}

#[derive(Debug, Deserialize)]
struct Email {
    #[serde(rename = "email")]
    address: String,
    verified: bool,
    primary: bool,
    visibility: Option<String>,
}

pub(super) async fn authorize(
    Extension(gh): Extension<GitHub>,
    Extension(config): Extension<TokenConfig>,
) -> crate::Result<axum::response::Response> {
    let header = jsonwebtoken::Header::new(config.alg);
    let claims = StateClaims::new(config.validation.aud.clone().unwrap());
    let state =
        jsonwebtoken::encode(&header, &claims, &config.enc_key).map_err(TokenError::from)?;

    let pq = format!(
        "/login/oauth/authorize?client_id={client_id}&redirect_uri={redirect_uri}&scope={scope}&state={state}",
        client_id = gh.client_id,
        redirect_uri = gh.redirect_uri,
        scope = ["read:user", "user:email"].join("%20"),
        state = state,
    );

    let uri = Uri::builder()
        .scheme("https")
        .authority("github.com")
        .path_and_query(pq)
        .build()?;

    let mut redirect = Redirect::to(&uri.to_string()).into_response();
    let cookie = format!(
        "state={}; Path=/v1/sso/github; SameSite=Lax; Secure; HttpOnly",
        state
    )
    .parse()
    .unwrap();
    redirect.headers_mut().insert(SET_COOKIE, cookie);

    Ok(redirect)
}

#[derive(Debug, Deserialize)]
pub struct AuthorizedParams {
    code: String,
    state: String,
}

pub(super) async fn authorized(
    Query(params): Query<AuthorizedParams>,
    TypedHeader(cookies): TypedHeader<Cookie>,
    Extension(gh): Extension<GitHub>,
    Extension(db): Extension<Database>,
    Extension(global): Extension<GlobalConfig>,
    Extension(config): Extension<TokenConfig>,
) -> crate::Result<Response<SessionResponse>> {
    let state = cookies.get("state").ok_or(SsoError::StateMissing)?;

    if state != params.state {
        return Err(SsoError::InvalidState.into());
    }

    let _claims = jsonwebtoken::decode::<StateClaims>(state, &config.dec_key, &config.validation)
        .map_err(|_| SsoError::InvalidState)?;

    let TokenResponse { access_token, .. } = gh.get_access_token(&params.code).await?;
    let access_token = access_token.ok_or_else(|| {
        tracing::error!("missing access token field");
        SsoError::from(GitHubError::UnknownError)
    })?;

    let (user, emails) = tokio::try_join!(
        gh.get_current_user(&access_token),
        gh.get_emails(&access_token)
    )?;

    let email = emails
        .into_iter()
        .find(|e| e.primary && e.verified)
        .ok_or(SsoError::EmailInvalid)?;

    let connection = Connection::GitHub {
        user_id: user.id,
        login: user.login,
        two_factor_enabled: user.two_factor_authentication,
    };

    let query = doc! {"$or": [
        {"connections": { "$elemMatch": { "type": "github", "userId": user.id } }},
        {"email": &email.address },
    ]};

    let doc = match db.get_user(query).await {
        Ok(doc) => {
            if let Some(c) = doc.connections.iter().find(|&c| c.is_github()) {
                if c != &connection {
                    db.update_user_connection(doc.id, connection).await?
                } else {
                    doc
                }
            } else {
                db.insert_user_connection(doc.id, connection).await?
            }
        }
        Err(e) => match e {
            Error::User(e) if e == UserError::NotFound => {
                let domain =
                    utils::get_email_domain(&email.address).ok_or(UserError::InvalidAddr)?;

                if !global.is_allowed_domain(domain) {
                    return Err(UserError::DomainNotAllowed.into());
                }

                let doc = UserDocument {
                    email: email.address,
                    connections: vec![connection],
                    can_login: true,
                    verified: true,
                    ..Default::default()
                };

                db.insert_user(&doc).await?;

                doc
            }
            _ => return Err(e),
        },
    };

    let audience = config.validation.aud.clone().unwrap();
    let scope = Scope::from_roles(doc.roles);
    let claims = SessionClaims::with_scope(audience, &doc.id.to_hex(), scope);

    let token = claims.encode(&config)?;

    let response = SessionResponse {
        user: doc.id.to_hex(),
        token,
        expires_at: claims.exp,
    };

    db.set_user_session(doc.id).await?;

    Ok(Response::with_status(StatusCode::CREATED, response))
}
