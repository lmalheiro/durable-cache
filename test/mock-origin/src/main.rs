// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Thin standalone wrapper for manual runs (`cargo run -p durable-cache-mock-origin`);
//! `test/compat` normally uses the library directly via `tokio::spawn`.

use durable_cache_mock_origin::script::Scenario;
use durable_cache_mock_origin::MockOrigin;

#[tokio::main]
async fn main() {
    let origin = MockOrigin::spawn()
        .await
        .expect("failed to bind mock-origin");
    origin.set_scenario(
        "/hello",
        Scenario::fixed(
            200,
            vec![
                ("content-type".into(), "text/plain".into()),
                ("cache-control".into(), "max-age=60".into()),
            ],
            "hello from mock-origin",
        ),
    );
    println!("mock-origin listening on {}", origin.base_url());
    println!("try: curl {}/hello", origin.base_url());
    std::future::pending::<()>().await;
}
