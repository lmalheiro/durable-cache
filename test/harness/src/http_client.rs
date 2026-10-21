// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Thin `reqwest` wrapper. A raw `hyper` client is used directly (not
//! wrapped here) by the one trailers-focused test, since `reqwest` doesn't
//! expose HTTP/1.1/h2 trailers.

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build reqwest client")
}
