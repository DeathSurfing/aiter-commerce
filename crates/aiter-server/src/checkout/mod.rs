//! Cart + checkout session REST surface (issues #13, #14).
//!
//! Provides the merchant-facing checkout flow on top of the core models:
//!
//! * `POST /carts` — create a cart (optional initial items).
//! * `GET /carts/{id}` — read a cart with live re-derived totals.
//! * `PUT /carts/{id}` — replace a cart's items (quantity `0` removes a line).
//! * `POST /carts/{id}/cancel` — cancel a cart (idempotent).
//! * `POST /checkout_sessions` — snapshot a cart into a checkout session.
//! * `POST /checkout_sessions/{id}/complete` — drive the state machine
//!   `Pending -> Ready -> Paid` and produce an [`Order`] (idempotent).
//! * `POST /checkout_sessions/{id}/cancel` — cancel a session (idempotent).
//!
//! All state lives in the shared [`AppState`] defined in [`crate::catalog`]:
//! the in-memory cart / session / order stores; unit prices resolve from the
//! served catalog (`AppState::price_of`), never a separate price book.

pub(crate) mod cart;
pub(crate) mod error;
pub(crate) mod session;

pub(crate) use cart::{cancel_cart, create_cart, get_cart, update_cart, CreateCartRequest};
pub(crate) use error::ApiError;
pub(crate) use session::{cancel_checkout, complete_checkout, create_checkout_session};

pub(crate) use session::CreateSessionRequest;
