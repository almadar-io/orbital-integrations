# orbital-integrations

Rust providers for the kernel's `call-service`: the host-side twin of `@almadar/integrations`. The kernel stays I/O-free; this crate is what a Rust or native host plugs in to answer a `CallService` request. `Integrations` implements `orbital_core::effects::ServiceHandler`, so it goes wherever a handler goes (`KernelConfig::service_handler`).

```rust
let creds = Credentials::from_process_env();           // store (with_store) wins over env
let transport: Arc<dyn Transport> = Arc::new(UreqTransport::new());
let mut integrations = Integrations::new(&creds)?;     // mock mode from ALMADAR_INTEGRATIONS_MODE / USE_MOCK_DATA
orbital_integrations::llm::register(&mut integrations, &creds, transport);
```

A call is: provider lookup (unregistered, unconfigured or failed construction report their own error) then the provider's `admit` check (the credentials role gate runs here, as TS runs it before `validateParams`), then validation against the installed contract (`language::registry`) then the provider. Mock mode answers through a `MockResponder` (default `CoreMockResponder`: `orbital_core::runtime::mock_answer`, the registry-shaped value every host shares; `with_mock_responder` overrides it), except for `UNMOCKED_SERVICES` (`credentials`, `workspace`: TS's `unmockedServices`), which always reach their registered provider.

Bodies are bytes: `HttpRequest.body` and `HttpResponse.body` are `Vec<u8>`, one representation for text and binary. `HttpRequest::with_text_body(&str)` sets a text body; `HttpRequest::text()` and `HttpResponse::text()` read a body as fetch's `res.text()` does (UTF-8, invalid sequences become U+FFFD), which is what the JSON and XML readers use. A provider that moves binary content (S3 objects, `drive.getFile`/`uploadFile`, `esign.downloadDocument`, web-push ciphertext) reads and writes `body` directly and returns base64 in its result, as the TS twin does. A host's own `Transport` implements `send` only.

`http` (default) enables the `ureq` transport; without it the crate still builds and a host supplies its own `Transport`.

## Twin parity: record, then replay

Every provider is held equal to its TS twin by fixtures recorded from the TS provider.

Record (TS side, `packages/almadar-integrations`):

```bash
ALMADAR_RECORD_FIXTURES=$PWD/../../orbital-rust/crates/orbital-integrations/fixtures \
  CI=true pnpm exec vitest run __tests__/<service>-recorded-exchanges.test.ts
```

`__tests__/support/http-recorder.ts` is the recorder. `startStubServer(script)` is a localhost server that answers with scripted provider replies and records each request (method, url, the semantic headers with `authorization`/`x-api-key` replaced by `<redacted>`, body). `interceptGlobalFetch(stub)` sends calls bound for a hardcoded host (api.openai.com, ...) to the stub and records the original URL, so no case needs a real network or key. A recorded base URL is written as `{base}`. Without the env var the same file is an ordinary test that asserts the provider's results.

Fixture: `fixtures/<service>/<case>.json` with `env`, `params`, `exchanges[]` and `outcome` (`{ok: true, data}` or `{ok: false, code, message}`). Fixtures hold no secrets: keys are the literal `test-key`.

Replay (Rust side): `tests/<service>_parity.rs` feeds each fixture to the Rust provider through a mock transport that asserts the same request (method, url with `{base}` substituted, headers, body as JSON) and returns the recorded response (a text body stays a string in the fixture; a fixture whose request body is binary records it as base64 in `extra.bodyBase64`, which the `l13d_common` replay compares byte for byte). The provider must produce the same data, or the same error message and code. See `tests/llm_parity.rs`.

## Adding a provider (the recipe for L13b-i)

1. Read the TS integration (`src/integrations/<service>/`) and its contract (`orbital_core::language::registry` params and credentials). List the actions and every HTTP call each makes.
2. Add `__tests__/<service>-recorded-exchanges.test.ts` next to the llm one: one case per action, plus error cases (4xx body, missing credential, validation failures). Script realistic provider replies (shapes from the provider's own parsing). Record. Run it twice and diff to confirm the fixtures are deterministic.
3. Add `tests/<service>_parity.rs` (copy `llm_parity.rs`). It fails until the port lands.
4. Implement `src/<service>/` with a type implementing `Provider` (`call(action, params, ctx) -> Result<Json, IntegrationError>`; `ctx: &CallContext` carries the caller, the twin of `IntegrationCallContext`: read `ctx.principal()` / `ctx.role()` wherever TS reads `context.principal` / `context.role`), reading credentials through `Credentials::get` (store then env, same variable names as the TS manager) and doing I/O only through `Transport`. Keep waits behind an injectable sleeper.
5. Expose `register(&mut Integrations, &Credentials, Arc<dyn Transport>)` mirroring `llm::register`: configured when the TS manager would configure it, otherwise `register_unconfigured`.
6. `cargo test -p orbital-integrations` green. List what the twin does not do yet in `orbital-rust/Almadar_Gaps.md`.

Differences from TS that cannot be closed here are listed in `G-ORB-254`.

## License

[Business Source License 1.1](LICENSE) — free for non-commercial and evaluation use; converts to Apache-2.0 on the change date.
