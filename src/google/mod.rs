//! The Google API client the `calendar` and `drive` providers share: the twin of `googleapis` over
//! `google-auth-library` and `gaxios` (service-account JWT or user refresh-token auth, retries, error text).

mod auth;
mod client;

pub use auth::{JwtSource, RefreshSource, TokenSource};
pub use client::{ApiResponse, Body, Gax, GaxError, GoogleClient, Sleeper};

/// The `Authorization` scheme both token sources produce.
pub const BEARER: &str = "Bearer";
