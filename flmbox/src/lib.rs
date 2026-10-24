/*
Copyright 2026 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! `flmbox`: a sandbox application for Flame.
//!
//! Each session gets a gVisor (`runsc`) sandbox created from an OCI image.
//! Modules: request/response types ([`api`]), environment driven configuration
//! with the image allowlist ([`config`]), the OCI image store ([`image`],
//! [`images`]), process execution ([`runner`]), the `runsc` wrapper
//! ([`runsc`]), the OCI bundle writer ([`bundle`]), the network seam
//! ([`network`]), the sandbox lifecycle ([`sandbox`]) and the Flame service
//! ([`service`]).

pub mod api;
pub mod bundle;
pub mod config;
pub mod image;
pub mod images;
pub mod network;
pub mod runner;
pub mod runsc;
pub mod sandbox;
pub mod service;
mod unpack;

#[cfg(test)]
mod testutil;
