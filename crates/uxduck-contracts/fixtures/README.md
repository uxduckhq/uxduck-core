# Contract fixtures

One JSON file per RPC variant and payload, named `<method>.<part>[.<case>].json`, with `/` in the method
written as `_` (`pairing_approve.request.json`). Parts: `request` (a `ClientRequest`), `result` (the
method's result), `notification` (a `ServerNotification` or `ClientNotification`) and `error` (an
`ApiError`, named `error.<kind>.json`).

Every fixture must pass the Rust serde round trip (`crates/uxduck-contracts/tests/fixtures.rs`) and the
generated Valibot schema (`packages/protocol`). A fixture that one side accepts and the other rejects is a
contract bug. Every method needs a request and a result fixture; the Rust test fails otherwise.
