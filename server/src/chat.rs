//! Web chat for testers: a ChatGPT-style page next to the MCP connector.
//! The page speaks MCP JSON-RPC to the same tools an assistant uses, so
//! permissions, the offline guard and reply filtering apply unchanged.
//!   /chat           signed-in testers; the page posts to /chat/rpc
//!   /mcp/{secret}   an access link opened in a browser; posts to itself

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use maud::{html, Markup, DOCTYPE};

use crate::app::{App, Fail};
use crate::auth::{check_csrf, User};
use crate::web::{ic, orb};

fn shell(who: &str, rpc: &str, csrf: &str, back: bool) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="referrer" content="no-referrer";
                title { "Chat · Substrate" }
                link rel="icon" href="/static/favicon.svg" type="image/svg+xml";
                link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter+Tight:wght@400;500;600&family=JetBrains+Mono:wght@400;500&display=swap";
                link rel="stylesheet" href=(format!("/static/app.css?v={}", crate::asset_v()));
                script src=(format!("/static/app.js?v={}", crate::asset_v())) defer {}
            }
            body.chat-body {
                div #chat .chat data-rpc=(rpc) data-csrf=(csrf) {
                    aside.chat-side {
                        div.chat-brand { a.brand href=(if back { "/" } else { "#" }) { (orb("")) "Substrate" } }
                        button.btn #chat-new type="button" { (ic("square-pen")) "New chat" }
                        div.label { "Projects" }
                        div #chat-projects {}
                        div.label { "Recent" }
                        div #chat-convs {}
                        div.chat-who.faint { (who) }
                    }
                    main.chat-main {
                        header.chat-top { div.chat-head #chat-head {} div.chat-opts #chat-opts hidden {} }
                        div.chat-log #chat-log {
                            div.chat-empty { (orb("xl")) h2 { "Talk to the developer's agent." } p.muted { "Report a bug, ask a question or request a change. The developer sees every message." } }
                        }
                        div.chat-banner.notice.warn #chat-banner hidden role="status" { span.ni { (ic("alert")) } div.nb #chat-banner-text {} button.x #chat-banner-x type="button" aria-label="Dismiss" hidden { (ic("x")) } }
                        div.chat-files #chat-files hidden {}
                        form.chat-compose #chat-form {
                            input #chat-file type="file" multiple hidden accept="image/png,image/jpeg,image/webp,application/pdf,text/plain";
                            button.btn.tertiary.icon #chat-attach type="button" aria-label="Attach files" { (ic("paperclip")) }
                            textarea #chat-text rows="1" placeholder="Message the agent" {}
                            button.btn.primary #chat-send type="submit" { (ic("send")) span { "Send" } }
                        }
                    }
                }
            }
        }
    }
}

/// GET /chat/live: "changed" when an agent of a project this person tests changes.
pub async fn live(State(app): State<App>, User(a): User) -> Result<Response, Fail> {
    let owners: Vec<String> = sqlx::query_scalar("SELECT DISTINCT p.owner_id FROM members m JOIN projects p ON p.id=m.project_id WHERE m.account_id=$1")
        .bind(&a.id).fetch_all(&app.db).await?;
    Ok(crate::web::account_live(&app, &owners).into_response())
}

/// GET /mcp/{secret}/live: the same for an access link.
pub async fn link_live(State(app): State<App>, Path(secret): Path<String>) -> Result<Response, Fail> {
    let (acct, _, _) = crate::links::resolve(&app, &secret).await.ok_or_else(|| Fail::new(404, "this access link is not valid"))?;
    let owners: Vec<String> = sqlx::query_scalar("SELECT DISTINCT p.owner_id FROM members m JOIN projects p ON p.id=m.project_id WHERE m.account_id=$1")
        .bind(&acct).fetch_all(&app.db).await?;
    Ok(crate::web::account_live(&app, &owners).into_response())
}

pub async fn page(User(a): User) -> Response {
    Html(shell(&a.label(), "/chat/rpc", &a.csrf, true).into_string()).into_response()
}

/// The session's own MCP endpoint: same tools, the browser's cookie instead of a bearer.
pub async fn rpc(State(app): State<App>, headers: HeaderMap, User(a): User, body: axum::body::Bytes) -> Result<Response, Fail> {
    let csrf = headers.get("x-csrf").and_then(|v| v.to_str().ok()).unwrap_or("");
    check_csrf(&app, &headers, &a, csrf)?;
    Ok(crate::mcp::serve(&app, &headers, &body, &a.id, "Substrate chat", None).await)
}

/// GET on an access link: a browser gets the chat, anything else the MCP 405.
pub async fn link_page(State(app): State<App>, headers: HeaderMap, Path(secret): Path<String>) -> Response {
    let html_wanted = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("text/html"));
    if !html_wanted {
        return crate::mcp::get().await;
    }
    match crate::links::resolve(&app, &secret).await {
        Some((_, label, _)) => Html(shell(&format!("Access link: {label}"), &format!("/mcp/{secret}"), "", false).into_string()).into_response(),
        None => (StatusCode::NOT_FOUND, crate::web::error_page("Link not valid", "Ask the developer for a new one.")).into_response(),
    }
}
