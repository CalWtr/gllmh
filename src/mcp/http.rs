//! MCP Streamable HTTP POSTs and session lifetime.

use eventsource_stream::Eventsource;
use futures_util::{
    StreamExt, future,
    stream::{self, BoxStream},
};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, Method, RequestBuilder, Response};
use serde_json::Value;
use tokio::time::timeout;

use super::{Error, PROTOCOL_VERSION, SHUTDOWN_GRACE};

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
const JSON: &str = "application/json";
const SSE: &str = "text/event-stream";

pub(super) type HttpReply = BoxStream<'static, Result<Value, Error>>;

pub(super) struct Http {
    client: Client,
    endpoint: String,
    session: Option<HeaderValue>,
}

impl Http {
    pub(super) fn new(endpoint: &str) -> Result<Self, Error> {
        let client = Client::builder().redirect(reqwest::redirect::Policy::none()).build()?;
        Ok(Self { client, endpoint: endpoint.into(), session: None })
    }

    fn message(&self, method: Method, initializing: bool) -> RequestBuilder {
        let mut request = self.client.request(method, &self.endpoint);
        if !initializing {
            request = request.header(VERSION_HEADER, PROTOCOL_VERSION);
        }
        if let Some(session) = &self.session {
            request = request.header(SESSION_HEADER, session);
        }
        request
    }

    async fn post(&self, value: Value, initializing: bool) -> Result<Response, Error> {
        let response = self
            .message(Method::POST, initializing)
            .header(ACCEPT, "application/json, text/event-stream")
            .json(&value)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::HttpStatus(response.status()));
        }
        Ok(response)
    }

    pub(super) async fn request(&mut self, value: Value, initializing: bool) -> Result<HttpReply, Error> {
        let response = self.post(value, initializing).await?;
        if initializing {
            self.session = response.headers().get(SESSION_HEADER).cloned();
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();

        if content_type.eq_ignore_ascii_case(JSON) {
            Ok(stream::once(async move { Ok(response.json().await?) }).boxed())
        } else if content_type.eq_ignore_ascii_case(SSE) {
            let events = response.bytes_stream().eventsource().filter_map(|event| {
                future::ready(match event {
                    Ok(event) if event.data.is_empty() => None,
                    Ok(event) => Some(serde_json::from_str(&event.data).map_err(Error::Json)),
                    Err(error) => Some(Err(error.into())),
                })
            });
            Ok(events.boxed())
        } else {
            Err(Error::Protocol(format!("unexpected HTTP response content type {content_type:?}")))
        }
    }

    pub(super) async fn send(&self, value: Value) -> Result<(), Error> {
        self.post(value, false).await?;
        Ok(())
    }

    pub(super) async fn close(self) {
        if self.session.is_some() {
            // Servers may decline DELETE. Cleanup must not change the tool result.
            let _ = timeout(SHUTDOWN_GRACE, self.message(Method::DELETE, false).send()).await;
        }
    }
}
