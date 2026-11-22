//! The test kit every conformance test uses: an in-process server built from the phase 0
//! frame, the track under test and the stand-ins a test opts into (never from `register_all`),
//! clients that send what go-github v93 sends, a webhook receiver, the `OpenAPI` and octokit
//! validators, and the fixture seed.

pub mod answer;
mod client;
pub mod coverage;
pub mod fixture;
mod graphql_answer;
mod receiver;
mod server;
pub mod standins;
pub mod validate;

pub use client::{Answer, GoGithub, Graphql, check_answer, contents_path, path_escape, ref_escape};
pub use receiver::{Delivery, Receiver};
pub use server::{Builder, TestServer};
