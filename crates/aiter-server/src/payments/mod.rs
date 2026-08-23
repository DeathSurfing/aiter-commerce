//! Razorpay payments rail (issues #18, #23).
//!
//! Minimal Razorpay Orders API client + environment-driven config. Sandbox is
//! the default mode. Credentials come from `RAZORPAY_KEY_ID` /
//! `RAZORPAY_KEY_SECRET` and are **never** logged: every `Debug` impl and
//! error path in this module redacts the secret.
//!
//! Layout (split of the former single-file module, issue #71):
//! * [`client`] — Razorpay API client, wire types, config/error plumbing
//! * [`webhook`] — webhook signature verification + order reconciliation
//! * [`api`] — axum handlers (`payment_link`, `/webhooks/razorpay`)
//!
//! Re-exports below keep every historical `crate::payments::*` call site
//! compiling unchanged.

pub(crate) mod api;
pub(crate) mod client;
#[cfg(test)]
mod tests;
pub(crate) mod webhook;

pub(crate) use api::{order_payment_link, razorpay_webhook};
pub(crate) use client::DEFAULT_BASE_URL;
pub use client::{RazorpayClient, RazorpayConfig, RazorpayError, RazorpayMode};
