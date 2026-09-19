//! What the real API refuses a request without, written once and read by every
//! fake upstream in this tree.
//!
//! # Why a fake upstream has to be strict about headers
//!
//! A borrowed request reaches the lender's own proxy with whatever the borrower
//! sent, and `api.anthropic.com` answers 400 to a `POST /v1/messages` that
//! carries a bearer and nothing else: no `anthropic-version` is a refusal, and
//! so is a body with no `content-type`. A fake upstream that answered 200 to
//! anything with a Bearer therefore proved the wrong thing. It let a whole
//! class of bug pass its own suite: the borrow path forwarded no client headers
//! at all, and every test still went green because the fake upstream did not
//! care.
//!
//! So the rule lives here rather than in each file's own handler. It is the
//! same fact three binaries assert against, and three copies of it would drift
//! apart the first time anybody widened one.
//!
//! # The names
//!
//! [`REQUIRED_REQUEST_HEADERS`] are the ones the API refuses a message request
//! without. `content-type` is asked only of a request that carries a body: a
//! `GET` has none and the API does not ask for one either, so requiring it of
//! every method would refuse requests the real origin accepts and the fake
//! upstream would be strict in a way that measures nothing.

use axum::body::Body;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;

/// The header names a message request must carry to be answered at all.
pub const REQUIRED_REQUEST_HEADERS: [&str; 2] = ["anthropic-version", "content-type"];

/// Which of [`REQUIRED_REQUEST_HEADERS`] this request is missing, in order.
///
/// `content-type` is only asked of a method that carries a body, for the reason
/// in the module docs.
pub fn missing_required_headers(method: &Method, headers: &HeaderMap) -> Vec<&'static str> {
    let carries_a_body = method != Method::GET && method != Method::HEAD;
    REQUIRED_REQUEST_HEADERS
        .into_iter()
        .filter(|name| *name != "content-type" || carries_a_body)
        .filter(|name| !headers.contains_key(*name))
        .collect()
}

/// The 400 the origin answers, naming what was missing so a failing test says
/// which header the path dropped rather than only that the borrow failed.
pub fn missing_header_refusal(missing: &[&'static str]) -> Response {
    let body = format!(
        r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"missing required header(s): {}"}}}}"#,
        missing.join(", ")
    );
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("the refusal builds")
}

/// The whole rule in one call: `Some(refusal)` when this request would be
/// refused by the origin, `None` when it may be answered.
pub fn refuse_if_incomplete(method: &Method, headers: &HeaderMap) -> Option<Response> {
    let missing = missing_required_headers(method, headers);
    if missing.is_empty() {
        return None;
    }
    Some(missing_header_refusal(&missing))
}
