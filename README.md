<div align="center">

<h1>XiHan Framework SDK for Rust</h1>

<h3>The same primitives, manifests and pipeline as the .NET host, verified by shared conformance vectors</h3>

<p>
  <a href="https://github.com/XiHanFun/framework-sdk-rust/actions/workflows/ci.yml"><img alt="CI" src="https://img.shields.io/github/actions/workflow/status/XiHanFun/framework-sdk-rust/ci.yml?branch=main&style=flat-square&label=CI" /></a>
  <img alt="Rust 1.85+" src="https://img.shields.io/badge/Rust-1.85%2B-000000?style=flat-square&logo=rust&logoColor=white" />
  <a href="./LICENSE"><img alt="MIT License" src="https://img.shields.io/github/license/XiHanFun/framework-sdk-rust?style=flat-square&color=green" /></a>
  <a href="https://github.com/XiHanFun/framework-sdk-rust/stargazers"><img alt="GitHub Stars" src="https://img.shields.io/github/stars/XiHanFun/framework-sdk-rust?style=flat-square&logo=github&label=Stars&color=1f6feb" /></a>
</p>

<p>
  <a href="https://framework.docs.xihanfun.com"><b>Docs</b></a> ·
  <a href="#getting-started"><b>Getting Started</b></a> ·
  <a href="https://github.com/XiHanFun/framework-spec"><b>Specification</b></a> ·
  <a href="https://github.com/XiHanFun/XiHan.Framework"><b>Reference Implementation</b></a>
</p>

<p><b>English</b> | <a href="./README_cn.md">简体中文</a></p>

</div>

XiHan Framework SDK for Rust is the Rust implementation of [XiHan Spec](https://github.com/XiHanFun/framework-spec), providing the primitives layer, manifests, the invocation context, the pipeline and operation contracts, plus the out-of-process plugin runtime (cargo feature `plugin`), with semantics and wire encoding defined by the spec's RFCs; the .NET reference implementation is [XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework). The spec sits in `spec/` as a submodule, and conformance is verified by the spec repository's vectors: the tests read `spec/conformance/vectors/` directly instead of copying the vector files.

## Modules

The whole SDK is a single crate, `xihan-framework`, and every language's SDK is a single package; domain words follow the spec's vocabulary (`conventions/vocabulary.md`), with only the casing changed to this language's style.

| Module | Contents | Spec |
| --- | --- | --- |
| `primitives` | `Error` / error codes, `Result<T>` (the standard `Result<T, Error>`) and its wire encoding `WireResult`, `Maybe<T>` (`Option<T>`), `TypedId<Tag, V>`, `LifecycleState`, `PipelineStage`, `Stability`, Baggage key and value constraints, message keys and templates (with number symbols per locale), `Catalog` and the process default resolver, `FrameworkError`; `primitives::json`: the shared JSON text check (vector `primitives/json-text.json`), reading by the value model (`json::from_slice`) and writing (`json::Model`, `json::to_value`) | RFC-0001, RFC-0010, `conventions/` |
| `manifest` | The model of `xihan.manifest.json`, reading, writing in the spec's canonical layout, and validation; capability declarations and requirement text (versions without leading zeros, at most 2147483647); reading follows the order of RFC-0002 parsing rule 2 (vector `manifest/manifests.json`): JSON text → `null` → item-by-item check against the Schema → cross-field constraints, and every error is `KERNEL_CAPABILITY_006` with a message key and a path or the manifest ID | RFC-0002, `manifest/manifest.schema.json` |
| `invocation` | `Context`: an awaitable `CancellationToken` that propagates from parent to child, deadlines fired by a background timer, W3C tracing (`traceparent` accepts only version `00` and lowercase hex, and an invalid one is treated as absent; sampling looks only at the lowest bit and writes `00` or `01`, writing `01` when there is no upstream), registered Baggage (values of registered keys are non-empty, at most 256 UTF-16 code units and free of control characters, except the idempotency key: building a context treats members as external input, dropping non-conforming ones and keeping the existing value, while `with_baggage` panics on a non-conforming value), and an item bag accessed by type; timeouts and cancellation are attributed to `KERNEL_TIMEOUT_001` / `KERNEL_CANCELLED_001` by the deadline token alone | RFC-0003 |
| `pipeline` | The eleven-stage `Builder`: `use_middleware` / `use_at` / `use_named_instance`, `before` / `after` (instances registered by name use the instance name, everything else the middleware name; the subject of a constraint must already be registered when it is declared), `describe` / `dump` / `build`; ordering rules, error codes `KERNEL_PIPELINE_00x` and the exact English error messages (vector `pipeline/ordering.json`, where an instance registered by name is written `middleware-name[instance-name]`): registration methods take `&mut self` and return `Result`, duplicate registration and an unregistered constraint subject are reported at registration, while an unregistered constraint target, cross-stage constraints and cycles are reported at ordering; middleware and terminals are all asynchronous | RFC-0004 |
| `operation` | `Contract`, `Schema`, the asynchronous `Handler<Req, Res>`, a `Registry` with a capacity of 10000, and an `Invoker` that runs through the full pipeline (XGI: JSON request and response; plus a typed entry point that bypasses JSON); the pipeline carries three built-in steps, placed where the .NET built-in middleware sits; kernel error codes, keys, arguments and en-US templates match vector `kernel-messages.json` (`type_mismatch` carries only `id`); payloads first pass the shared JSON text check, the first problem in text order is reported, and `reason` is a JSON path (vector `operations/payload-reasons.json`); members holding arbitrary JSON use `json::Json` to keep numbers verbatim; the response is encoded only after the pipeline finishes, and a response that cannot be written is reported as `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception`; middleware that short-circuits with success but no response is reported as `KERNEL_UNEXPECTED_001` + `kernel.operation.no_response` | RFC-0011 |
| `plugin` (feature `plugin`) | The out-of-process plugin runtime: `Plugin` reads the manifest and the environment variables passed by the host, checks its declarations before listening, listens on a local endpoint, writes the handshake line to stdout, and serves the `Plugin` service of `xihan.plugin.v1` as a gRPC server (handshake, configure, start and stop, health and health push, invocations, event delivery), checking the session token in constant time before every RPC; the lifecycle follows the order table, configuration section, readiness, health, stop and shutdown rules of `lifecycle.md` §7, with a state machine on `LifecycleState`; `Invoke` runs through `operation::Invoker` and the full pipeline, with the invocation context restored from `InvocationMetadata`; `Host` calls back `Host.Invoke` and `Host.Publish`, checking the permission and the tenant locally before sending and mapping gRPC statuses by the spec's table; `Logger` writes JSON log lines to stderr; `plugin::v1` is the code generated from `plugin.proto`; vectors `handshake-lines.json`, `host-addresses.json`, `protocol-versions.json`, `deadlines.json`, `plugin-messages.json` and `callback-status.json` under `plugin/` | RFC-0017, `plugin/handshake.md`, `plugin/lifecycle.md`, `plugin/invocation.md`, `plugin/plugin.proto` |

The `plugin` module is optional: `cargo add xihan-framework --features plugin`. It brings in tonic, prost and tokio; without the feature none of these dependencies are pulled in, and the other modules depend on no async runtime.

## Getting Started

```bash
cargo add xihan-framework
```

```rust
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use xihan_framework::invocation::Context;
use xihan_framework::operation::{self, Contract, EXPOSE_HTTP, Invoker, Registry, Request};
use xihan_framework::pipeline::{self, APP_OWNER, Builder, Placement};
use xihan_framework::primitives::{BAGGAGE_TENANT, Error, PipelineStage, Stability};

#[derive(Clone, Deserialize)]
struct CreateOrder {
    quantity: u32,
}

#[derive(Serialize)]
struct OrderCreated {
    quantity: u32,
    tenant: Option<String>,
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::new());
    registry.add(
        Contract::new("orders.create").with_stability(Stability::Experimental).with_expose([EXPOSE_HTTP]),
        operation::handler_fn(|ctx: &Context, request: CreateOrder| {
            Box::pin(async move {
                if request.quantity == 0 {
                    return Err(Error::validation("ORDERS_VALIDATION_001", "orders.quantity.invalid").with_detail("quantity", 0));
                }
                Ok(OrderCreated { quantity: request.quantity, tenant: ctx.tenant().map(str::to_owned) })
            })
        }),
    )?;

    let observe = pipeline::from_fn("Observe", |ctx, next| {
        Box::pin(async move {
            let outcome = next.run(ctx).await;
            if let Err(error) = &outcome {
                eprintln!("{} failed: {error}", ctx.trace_id());
            }
            outcome
        })
    });
    let mut builder = Builder::new();
    builder.use_at(observe, PipelineStage::Observe, Placement::End, APP_OWNER)?;
    print!("{}", builder.dump()?); // observe      Observe

    let invoker = Invoker::new(registry, &builder)?;
    let ctx = Context::builder().timeout(Duration::from_secs(2)).baggage(BAGGAGE_TENANT, "t1").build();
    let payload = invoker.invoke(&ctx, Request::new("orders.create", r#"{"quantity":2}"#)).await?;
    assert_eq!(payload, br#"{"quantity":2,"tenant":"t1"}"#);
    Ok(())
}
```

1. Middleware, handlers and the pipeline terminal return `BoxFuture` (`Pin<Box<dyn Future + Send>>`), written as `Box::pin(async move { … })`; the crate depends on no async runtime, so hand the futures to tokio, `futures::executor::block_on` or any executor. The signatures of `Invoker::invoke` and `invoke_typed` state that they return `Send` futures, and `invoke_owned` returns a `'static` future that can go straight to `tokio::spawn`.
2. Recoverable failures return `Err(Error)`. Error codes follow `<DOMAIN>_<KIND>_<NNN>`, and text is a message key plus arguments (`Error::validation(code, key).with_detail(…)`); `Error::message` resolves first through the resolver set by `primitives::set_default_messages`, then through the en-US templates bundled with the SDK (`primitives::builtin_catalog()`: the kernel errors shared by the SDK and the host, verbatim from vector `kernel-messages.json`), and is "key (arguments)" when neither has the key. Configuration-time errors with a registered key (such as registering an operation twice) carry the key and arguments through `FrameworkError::with_message`, and `to_error()` carries them over.
3. Programming errors (error codes, message keys or Baggage keys that break the grammar) panic, and each function's documentation states its panic conditions. The panic payload is the `FrameworkError` itself (`std::panic::panic_any`), not a string; after catching it, take it out with `downcast_ref::<FrameworkError>()`. The standard library's default panic hook prints only `Box<dyn Any>` for it, so install your own hook to see the code and description. Check values from untrusted input with `is_error_code` and `is_baggage_key_well_formed` first, or use `Error::try_new`. Configuration-time failures such as manifest validation, pipeline ordering and operation registration return `FrameworkError`.
4. `Context` deadlines are fired on time by a background thread in the process (`xihan-deadline-timer`, started when the first deadline is registered); `Context::done()` completes when the caller cancels or the deadline is reached, and `Context::cancellation()` gives `KERNEL_TIMEOUT_001` (`kernel.invocation.deadline_exceeded`) or `KERNEL_CANCELLED_001` (`kernel.invocation.cancelled`). The invoker's pipeline holds three built-in steps, all placed at `Beginning` of their stage under the owner `xihan.kernel.core` (`operation::KERNEL_OWNER`) and ordered against middleware in the same position by RFC-0004: `ObserveMiddleware` in the Observe stage translates panics from the middleware and handler after it into `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception` (a `FrameworkError` payload goes into `inner`), while panics from middleware before it reach the caller unchanged; `DeadlineMiddleware` in the Resilience stage checks the deadline against the clock on entry and, if it has passed, reports `KERNEL_TIMEOUT_001` + `kernel.invocation.deadline_before_start` without running the middleware and handler after it (the Ingress and Observe stages still run), and when something downstream writes a cancellation as a `KERNEL_CANCELLED_001` result while the deadline token has fired, it reports `KERNEL_TIMEOUT_001` + `kernel.invocation.deadline_exceeded` instead (RFC-0003 rule 1); `ValidationMiddleware` in the Validate stage decodes the payload of raw calls and is skipped for typed calls. Before the pipeline, only an unregistered operation and a type mismatch of a typed call are checked, in that order. Translating panics requires `panic = "unwind"`. The XGI entry point encodes the response into a payload only after the pipeline finishes, so middleware sees the object before encoding through `operation::with_response` (the response of a typed call is written to the same slot, like .NET's `FindOperationResponse`, just not encoded); when it cannot be written (a typed float that is NaN or infinite, a value the serializer does not support, a panic during serialization), the call ends with `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception` instead of propagating to the caller. Cancellation and deadlines are signaled only through the token: the invoker and the built-in steps neither abort the handler nor drop its future, the result the handler returns after the token fires is the result of the call, and attribution by the token happens only when the handler turns cancellation into its result (`Err(ctx.cancellation()…)`).

### Write a Plugin

```bash
cargo add xihan-framework --features plugin
cargo add tokio --features macros,rt
cargo add serde --features derive
```

```rust,ignore
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use xihan_framework::operation::{self, Contract, Registry};
use xihan_framework::plugin::{MANIFEST_FILE_NAME, Plugin};

#[derive(Deserialize)]
struct Ping {
    text: String,
}

#[derive(Serialize)]
struct Pong {
    text: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let registry = Arc::new(Registry::new());
    registry.add(
        Contract::new("pings.create"),
        operation::handler_fn(|_ctx, ping: Ping| Box::pin(async move { Ok(Pong { text: ping.text }) })),
    )?;
    // The manifest handed to the host in the handshake must match the file in the plugin directory: the working directory is the plugin directory, so read it from there
    Plugin::from_manifest_file(MANIFEST_FILE_NAME)?.with_operations(registry).serve().await?;
    Ok(())
}
```

The plugin directory holds the executable and the manifest `xihan.manifest.json`:

```json
{
  "id": "xihan.samples.ping",
  "version": "1.0.0",
  "primitives": "1",
  "stability": "experimental",
  "kind": "plugin",
  "isolation": "out-of-process",
  "contributes": { "operations": [ "pings.create" ] },
  "entry": "ping"
}
```

The example above needs the `plugin` feature, while the README examples are also compiled by the tests without features, so it is marked `ignore`; the same code is the example in the `plugin` module documentation, compiled under `--features plugin`. The complete example is `examples/echo` (operations, event subscriptions, a health item, configuration and host callbacks).

1. Manifest and the checks before starting: `kind` is `plugin`, `isolation` is `out-of-process`, `entry` is the executable relative to the plugin directory (on Windows the host appends `.exe` when there is no extension), and `primitives` equals the primitives major version this SDK implements, `1`; every registered operation must be in `contributes.operations` and health items in `contributes.health` (undeclared ones are dropped when reported, with one warning per name), and calling back the host requires declaring `capability:kernel.pipeline` (`Host.Invoke`) or `capability:messaging.queue` (`Host.Publish`) in `permissions`; an out-of-process plugin may not write `contributes.pipeline` (only Wasm plugins can contribute middleware). Before listening and writing the handshake line, `Plugin::start` reads the manifest by RFC-0002 and checks the points above, the topic grammar of subscriptions and that no (topic, type) pair repeats, and that every message catalog has `en-US`; otherwise it logs one `error` line to stderr, does not listen, and returns a `FrameworkError` (the host sees the process exit before the handshake). The handshake hands over the manifest's `primitives`, the manifest text as written (without a leading BOM; the host compares it field by field with the file in the directory), declarations of unary operations only, the subscriptions in declaration order (`mode` always written as `competing` or `broadcast`) and the plugin's own message catalogs.
2. Endpoints: on Unix a Unix domain socket in a private directory newly created under the temp directory (directory `0700`, random name, a different name when it already exists, socket `0600`; when the temp directory's path contains whitespace or is not UTF-8, `/tmp` is used instead, so the address in the handshake line has no whitespace), removed together with the directory after stopping; on Windows a named pipe `\\.\pipe\xihan-plugin-<random>` that rejects remote clients and uses the system's default security descriptor: the current user, administrators and SYSTEM can read and write, other local users can only open it read-only and cannot send requests (this crate forbids `unsafe`, so it cannot set an access control list containing only the current user), so, as with TCP loopback, isolation relies on the session token; `with_network(Network::Tcp)` switches to TCP loopback bound only to `127.0.0.1`, which other users on the machine can reach, so isolation relies on the session token alone.
3. Callback endpoint: `XIHAN_PLUGIN_HOST_ADDRESS` is one or more `<network> <address>` entries ordered by the host's preference and separated by `;` (`plugin::parse_host_addresses`; one invalid entry makes the whole value invalid), and the runtime takes the first entry this platform can connect to (`Network::supported()`: `unix` and `tcp` on Unix, `npipe` and `tcp` on Windows; on Windows the host offers a named pipe plus TCP loopback and the pipe is used, and when it offers only TCP loopback the callbacks go over TCP loopback), never switching afterwards. When the variable is missing, invalid or has no entry it can connect to, the plugin still serves, logs one `warning` to stderr at startup, and every callback fails with `PLUGINS_UNAVAILABLE_001` + `plugins.host.unavailable` (`reason` `no supported callback endpoint`) without sending a request.
4. Lifecycle (`lifecycle.md` §7): every RPC (including `Handshake`, and a streaming call before its first frame is read) first checks the metadata `xihan-plugin-session` in constant time; it must hold exactly one matching value, and none or two or more (even with one of them right) are refused with `Unauthenticated` without running anything. Order: `Handshake` only in `Declared`, `Configure` only in `Resolved`, `Start` only in `Configured`, `Stop` in any state except `Declared`, `Draining`, `Stopped` and while stopping; anything else is refused with `FailedPrecondition` and the state stays; `Health`, invocations and deliveries are allowed in any state. An empty configuration section counts as `{}`; one that is not a JSON object (after the common JSON reading checks) does not reach `Hooks::configure` and puts the plugin into `Failed` with `PLUGINS_UNEXPECTED_001` + `plugins.process.failed` (`phase` `configure`); `Configuration::section` is the text as written and `Configuration::decode` is the typed reading: member names match ASCII case-insensitively, and leaves the host wrote as strings can be read as numbers (by the JSON number grammar) and booleans (`true` / `false`, case-insensitive). After `Hooks::start` succeeds the plugin enters `Started`, asks the health hook once if there is one and enters `Ready`, `Degraded` or `Failed` by the result (`Ready` for any other reported state, `Degraded` when the hook panics, `Ready` without a health hook), and only then replies; only `Ready` and `Degraded` serve, and invocations and deliveries in any other state fail with `PLUGINS_UNAVAILABLE_001` + `plugins.process.unavailable` (`state` is the state name). Before `Start`, `Health` reports the local state without health items or calling the hook; while serving every `Health` asks `Hooks::health` once, only the `Ready`, `Degraded` or `Failed` it reports changes the state (there is no way back from `Failed`), and a panic in it counts as reporting `Degraded`: that answer is `Degraded` without health items, the state moves to `Degraded`, and later answers decide the state as usual; `WatchHealth` pushes the current state first, pushes on changes, asks the hook every 10 seconds while serving, pushes once on entering `Draining`, and ends after replying to `Stop`. An error returned by a hook goes into `status` as it is; a panic is `plugins.process.failed` with `reason` `panic` (Rust has no exceptions), and the panic's content goes only to stderr.
5. Stop and shutdown: when the start hook never succeeded, `Stop` does not call `Hooks::stop` and replies `ok`; otherwise a serving plugin enters `Draining` and refuses new calls, waits for calls in flight up to the grace period (`grace_seconds`, 5 seconds when it is 0), at the end of which it fires their cancellation tokens and stops waiting, then calls `Hooks::stop` (at most once per process; a failure or panic only goes into `StopResponse.status`), stops serving after replying, and `serve` returns `Ok` so the process exits with code 0. Closing stdin and SIGTERM (Unix): a serving plugin enters `Draining`, fires the cancellation tokens of all calls in flight at once and waits for them, and a plugin whose start succeeded calls `Hooks::stop` once (`reason` `shutdown`); at most 3 seconds after the request the process exits with code 0 whether or not this has finished, and a `Stop` in progress is bound by the same limit. `Running::shutdown` winds down the same way without ending the process.
6. Invocations and deliveries: `Invoke` checks in order: an undeclared operation ends with `Unimplemented`, a plugin that is not serving gives `PLUGINS_UNAVAILABLE_001`, and then the call runs through the invoker and the full pipeline (the pipeline inside the plugin process is given with `with_pipeline`), an empty `payload` counting as `{}`. The context is restored from `InvocationMetadata`: tracing from `trace_parent` (a new trace when it does not match the RFC-0003 grammar), `trace_state` does not enter the context, Baggage keeps only registered keys, and `deadline` is read by the spec's strict grammar (treated as absent when it cannot be read), the effective deadline being the earlier of it and the gRPC deadline; when the host cancels the call, the stop grace period runs out or the plugin shuts down, the context token fires, and the handler still runs to the end in its own task. Business failures go into `Status.error`: an error with a message key leaves `text` empty for the host to translate into the caller's locale, only free-text errors write `text`, `details` is JSON object text, and the gRPC status stays `OK`. `HandleEvent` checks in order: no envelope, or a mode other than empty, `competing` and `broadcast`, is `InvalidArgument`; the subscriptions are found by (topic, mode, type), and none gives `KERNEL_NOTFOUND_001` + `plugins.event.unsubscribed`; a plugin that is not serving gives `PLUGINS_UNAVAILABLE_001`; the matching subscriptions run one after another in declaration order and the first failure is the result, a handler panic being `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception`. Streaming is not supported: until the RFC for streaming operation contracts, plugins neither implement streaming calls nor declare streaming operations, and the SDK has no API for them; `InvokeStream` reads the first frame and ends with `InvalidArgument` when the stream ends before it or it is not `start`, and with `Unimplemented` when it is `start`.
7. Callbacks: handlers get the `Host` through `plugin::host(ctx)` (outside calls through `Configuration::host()`); `invoke` and `publish` carry the session token, with `metadata` taken from the invocation context they run in (the traceparent of the current span, the deadline in UTC with seven fractional digits and `+00:00`, registered Baggage, an empty `trace_state`) and a gRPC deadline equal to `deadline`. Before sending, the checks run in order, and nothing is sent when one fails: first whether there is a usable callback endpoint (see item 3); then, in the host's order, the permission must be in the manifest `permissions` and granted, otherwise `PLUGINS_DENIED_002` + `plugins.callback.denied`, and `xihan.tenant` in the context can only be the tenant of a call or delivery the host is currently handing to this plugin, otherwise `PLUGINS_DENIED_002` + `plugins.callback.tenant_denied`; last, a callback context whose deadline has passed gives the `DeadlineExceeded` row (`KERNEL_TIMEOUT_001`) and one that has been cancelled the "`Cancelled`, cancellation signal fired" row (`KERNEL_CANCELLED_001`), and a call abandoned because its context is cancelled after sending ends the same way. For `Host.Publish` all of these are results for the whole batch, and the `xihantenant` of envelopes is not checked locally but left to the host item by item. The host's gRPC statuses map only by the table in `invocation.md` §5 (vector `callback-status.json`), without rewriting the result by the clock or the token, since timeout attribution belongs to the deadline step of the plugin's own pipeline; a connection failure counts as `Unavailable`, and the `Cancelled` (`Timeout expired`) tonic reports when it aborts a call locally at the deadline counts as `DeadlineExceeded`; when `Host.Publish` fails as an RPC every item gets the same error, and when the number of results differs from the number of envelopes every item is `plugins.process.failed`; an `Error` written by the host is read by `invocation.md` §3 (vector `error-reading.json`): an invalid error code becomes `plugins.process.failed` (`phase` always `invoke`, the original `inner` dropped), an error with a key does not take `text`, an invalid key makes a free-text error, and `details` that are empty, only whitespace, an empty object, fail the common JSON reading checks, are not an object or hold a number that overflows a double count as none; `ok` false without an `error`, or a missing `status`, is `plugins.process.failed` (`phase` the operation ID or `publish`).
8. Logs and message keys: `Logger` writes one JSON object per line to stderr (`level` is `trace` / `debug` / `information` / `warning` / `error` / `critical`, plus `message`, with the other members as scope properties; every runtime line carries `plugin`). The runtime shares the seven message keys of `invocation.md` §8 with the host: `plugins.process.unavailable`, `plugins.process.failed`, `plugins.event.unsubscribed`, `plugins.host.unavailable`, `plugins.callback.denied`, `plugins.callback.tenant_denied` and `plugins.callback.session_invalid`, whose arguments are exactly the error's `details`. The host has their catalog, so the runtime does not send it in the handshake; the SDK carries the same texts in 10 locales (`plugin::runtime_catalog()`), the en-US templates join `primitives::builtin_catalog()`, and to render other locales locally hand its `to_catalog()` to `primitives::set_default_messages`. The plugin's own catalogs use `MessageCatalog` (at least `en-US`; the catalog name may not be the SDK name).
9. Verification: run every case of the spec repository's black-box runner (requires Go):

```bash
cd spec/conformance
go run ./cmd/xihan-conformance plugin --dir <plugin-dir> --exclude <crashing-operation-id> --sample '<operation-id>=<request-json>'
```

## Behavior Shared with the Host

The wire encoding matches the .NET reference implementation:

```rust
use xihan_framework::primitives::{Error, WireResult};

let ok = serde_json::to_string(&WireResult(Ok::<_, Error>(7))).unwrap();
assert_eq!(ok, r#"{"ok":true,"value":7}"#);
let failed = serde_json::to_string(&WireResult(Err::<u32, _>(Error::literal("ORDERS_CONFLICT_001", "stale")))).unwrap();
assert_eq!(failed, r#"{"ok":false,"error":{"code":"ORDERS_CONFLICT_001","message":"stale"}}"#);
```

Reading and writing follow RFC-0001 "Result" and vector `wire-encoding.json`: a success without a value, `Result<()>`, is written only as `{"ok":true}`, and one read with `value` is rejected; for a success with a value, `value` is required and may be `null` only when the value type is `Maybe<T>` (`Option<T>`); an `Error` requires `code` and a non-empty `message`, and an optional member written as `null` is invalid; Result and Error accept only JSON objects, and unknown members are ignored. `Maybe<T>` is the value itself when present and `null` when absent; `TypedId` is encoded as its underlying value. Read wire text with `primitives::json::from_slice`: it first runs the shared JSON text check (RFC 8259, 64 levels of nesting, valid Unicode, duplicate members at any level, including inside values, details and inner errors), and reads numbers by the value model (integers within the 64-bit range round-trip unchanged, and `-0` is the integer 0); reading directly with `serde_json::from_str` skips these checks. Values are written through `json::Model`: floats with integral values are still written as floats (`2.0`, `-0.0`, `1e+17`), and non-finite floats are written as the strings `NaN`, `Infinity` and `-Infinity`.

Message templates follow RFC-0010 "Templates" and vector `message-templates.json`: `{name}` / `{0}`, with `{{` and `}}` as escapes; the text form of a value is a string as-is, a boolean as `true` / `false`, `null` as the empty string, a float with 1e-4 ≤ |x| < 1e17 as the shortest round-trip decimal (`2.0` is written `2`) and outside that range as `1E+17` or `9.9E-05`, negative zero as `-0`, and arrays and objects as compact JSON (members in UTF-16 code unit order of their names, with containers deeper than 64 levels written as `null`). Only format specifiers matching `^[DdFfNnXx][0-9]{0,2}$` are recognized: `D` and `X` / `x` apply only to integers (`X` only to non-negative integers), `F` and `N` apply to integers and floats, rounding half to even and writing all integer digits; every other specifier (`E2`, `0.00`, `yyyy-MM-dd`, `D100`) and every case where a specifier does not apply is output unformatted, on the host as well. `format_message` and its siblings use the invariant culture; `format_message_in` / `try_format_message_in` / `format_value_in` take the decimal and group separators from the locale (the spec's fixed table: de-DE, es-ES and pt-BR use `,` and `.`, fr-FR uses `,` and U+202F, ru-RU uses `,` and U+00A0, and other locales match the invariant culture), and `Catalog` formats in the requested locale. `Describe` lists arguments in ordinal order of their names (UTF-16 code units), in the invariant culture.

Manifests are read with `manifest::read` and written with `manifest::write` in the spec's canonical layout (two-space indentation, LF, no trailing newline, fixed member order, lists always written, defaults written out), identical character for character across languages; reading fills in defaults like the .NET typed model (`role` is `provider`, `activation` is `["onStartup"]`, a binding's `required` is `true`, `contributes` is empty, and `$schema` is not kept), and a written manifest read back equals the original; the `message` of a read failure is the en-US template of the manifest error in `kernel-messages.json`.

## Language Mapping

Mappings the spec leaves for each language to define:

**Request type members** (RFC-0011 rule 2). Payloads are read into the request type with serde: member names are case-sensitive, unknown members are ignored (do not add `deny_unknown_fields`), numbers are not read from strings, integers reject `1.0` and `1e2` and values outside the type's range are a mismatch, floats accept integer notation with `1e400` as infinity and `1e-400` as 0, and object positions accept only JSON objects.

| Spec | Rust |
| --- | --- |
| Required, `null` not accepted | A non-`Option` member without a default: `n: i64` |
| Required, `null` accepted | `#[serde(deserialize_with = "operation::nullable")] name: Option<String>`: missing is a mismatch, `null` is `None` |
| Optional, `null` not accepted (integers, floats, booleans) | `#[serde(default)] count: i64`: missing takes the default, `null` is a mismatch |
| Optional, `null` accepted (strings, arrays, objects) | `Option<T>`: missing and `null` are both `None` |
| Array items that accept `null` | `Vec<Option<String>>` |
| Arbitrary JSON | `primitives::json::Json` (keeps numbers as written), or `Option<Json>` when `null` means absent; `serde_json::Value` reads `1e400` as `null` and integers beyond 64 bits as approximations |

A missing required member is reported with the path of its enclosing object when that object ends (`$` at the root), and for type mismatches the first one in text order is reported; both follow from the read order of serde's derived code, so a custom `Deserialize` must likewise report missing members only after reading the whole object.

**Value model** (RFC-0010 "Templates"). Template arguments and error details are `serde_json::Value`:

- Integers: integers in `Value` within the `i64` range are integers; the upper half of `u64` (above `i64::MAX`) is a double; `i128` / `u128` are converted with `json::to_value`, and values beyond 64 bits are doubles.
- Floats: `f64` is taken as-is; `f32` is first converted to the nearest `f64` (`Value::from(0.1f32)` and `json::to_value(&0.1f32)` are both `0.10000000149011612`). `Value` cannot hold non-finite floats: `Value::from(f64::NAN)` is `null`, and `json::to_value(&f64::NAN)` is the string `"NaN"`; a number that overflows on reading (`1e400`) is infinity in an `f64` member, `null` in a `Value`, and kept as written in a `json::Json`. Non-finite floats in a typed response cannot be written (the call ends with an unhandled failure) and are not written as `null` or a string; only untyped values (error details, template arguments, the value of a `WireResult`) are written as strings by the value model.
- Characters: a `char` converted with `json::to_value` is a one-character string. Mapping: `BTreeMap<String, _>`, `HashMap<String, _>` and structs are objects; `Vec`, arrays and tuples are arrays.
- Types outside the model (`chrono` times, `uuid::Uuid`, enums and so on): convert them to values through their `Serialize` (`json::to_value`; times and UUIDs are usually strings, and unit enum variants are their variant names) or `to_string()` (`Display`) first; the SDK applies no formatting of its own. To format consistently across languages, convert to a string first.

**Grammar**: every regular expression in the spec is a hand-written byte-by-byte check with whole-string match, and a trailing newline does not count as a match.

## Design

Dependencies:

| Crate | Purpose |
| --- | --- |
| `serde` (`derive`), `serde_json` (`raw_value`) | Wire encoding of `Result` / `Error` / `Maybe`, XGI payload encoding and decoding, manifest reading and writing; `serde_json::Value` is the value type of error details and template arguments; `raw_value` lets `json::Json` write out its original text |
| `getrandom` | Random numbers for trace IDs, span IDs and plugin endpoint names |
| `serde_path_to_error` | The JSON path of the error location when a payload is invalid (`$.lines[1].quantity`): RFC-0011 rule 2 requires `reason` to contain only the path, and `serde_json` itself reports only line and column |
| `tonic` (`codegen`, `server`, `channel`), `tonic-prost`, `prost` | Only with the `plugin` feature: the gRPC server and callback client of `xihan.plugin.v1` and protobuf encoding; tonic requires Rust 1.88 from 0.14.6 on, so dependencies resolve to 0.14.5 by `rust-version`, and the generated code comes from the 0.14.5 generator as well |
| `tokio`, `tokio-stream`, `hyper-util` | Only with the `plugin` feature: listening on endpoints, calls in flight, health push, the stop grace period and SIGTERM, and building connections for callbacks; only the current-thread runtime is required, and whether to use the multi-threaded one is up to the plugin author |

The shared JSON text check is a built-in byte-by-byte reader (`primitives::json`) that produces a value tree in document order, which a built-in serde `Deserializer` then reads into the target type: `serde_json` keeps the last of duplicate members, does not check lone surrogate escapes, reads `-0` as a float and rejects `1e400`, and `Value` sorts keys, none of which matches the spec.

The grammars in the spec (error codes, message keys, Baggage keys, manifest fields, `traceparent` and so on) are hand-written byte-by-byte checks with no regex library; the cancellation token and deadlines are a lightweight built-in implementation with no async runtime. There are no dev dependencies: the conformance tests use `RawValue` to take the manifests out of the vectors as written, keeping members in document order, and the tests run futures on a minimal hand-written executor.

## Development

```bash
git submodule update --init
```

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo clippy --all-targets --features plugin -- -D warnings
```

```bash
cargo test && cargo test --features plugin
```

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --features plugin
```

The minimum Rust version is 1.85 (required by edition 2024), and clippy's `incompatible_msrv` checks standard library APIs against it; dependencies are resolved against it too (tonic resolves to 0.14.5). `spec/`, `tests/` and `tools/` are not included in the crate package.

The plugin runtime's conformance test `tests/plugin_conformance.rs` requires Go (version in `spec/conformance/go.mod`): it builds the echo example, puts it with its manifest into a temporary directory, runs every case of the black-box runner with `go run ./cmd/xihan-conformance plugin` in `spec/conformance`, and asserts that all pass; without Go it fails, and to skip it on purpose set `XIHAN_CONFORMANCE_REQUIRED=0` (it then prints the reason and returns early). `tests/conformance_vectors.rs` checks that every vector file in `spec/conformance/vectors` is either replayed by a test or exempted with a reason: when the spec adds a vector file it fails until the file is replayed or added to the exemptions with a reason.

The protocol code `src/plugin/generated/xihan.plugin.v1.rs` is generated from `spec/plugin/plugin.proto` and committed, so users need no protoc. After upgrading the spec submodule, regenerate it and commit it along (the `.proto` is compiled by protox in pure Rust, so no protoc is needed here either; CI regenerates it and checks that it matches the committed code):

```bash
cargo run --manifest-path tools/protogen/Cargo.toml
```

## Status

`0.1`: primitives, manifests, the invocation context, the pipeline, operations and the out-of-process plugin runtime are usable and pass all vectors for primitives (including `json-text.json`), manifest, pipeline, operations and plugin (handshake lines, callback endpoint lists, version negotiation, deadlines, message keys, callback failure mapping, error reading) under `conformance/vectors/` in the spec repository; the echo example passes every case of the black-box runner `xihan-conformance plugin` (not yet published to crates.io, where 0.0.1 is a name placeholder). Not yet provided:

- streaming operations: operation contracts (RFC-0011) have no streaming form yet, plugins do not implement streaming calls, and `InvokeStream` refuses every stream after reading its first frame (`invocation.md` §1);
- the Wasm plugin ABI (RFC-0018, `xihan_*` exports, `xihan` imports, WASI preview 1 reactor), with a template in XiHan.Framework's `samples/EchoWasmPlugin`;
- operation sets for remote providers (RFC-0019, starting with `caching.distributed@1`).

## Ecosystem

| Project | Description |
| --- | --- |
| [framework-spec](https://github.com/XiHanFun/framework-spec) | XiHan Spec: RFCs, vocabulary, protocol mappings and conformance vectors |
| [XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework) | The .NET reference implementation |
| [framework-sdk-go](https://github.com/XiHanFun/framework-sdk-go) | Go SDK |
| [framework-sdk-typescript](https://github.com/XiHanFun/framework-sdk-typescript) | TypeScript SDK |
| [framework-sdk-java](https://github.com/XiHanFun/framework-sdk-java) | Java SDK |
| [framework-sdk-python](https://github.com/XiHanFun/framework-sdk-python) | Python SDK |
| [framework-sdk-php](https://github.com/XiHanFun/framework-sdk-php) | PHP SDK |

## Contributing

[Issues](https://github.com/XiHanFun/framework-sdk-rust/issues) and pull requests are welcome.

1. Start with the README and the relevant RFCs of framework-spec.
2. Changes to public contracts are not made in the SDK repositories: they go through the RFC process in [framework-spec](https://github.com/XiHanFun/framework-spec) and are then synced to every SDK.

## License

[MIT](./LICENSE) © 2021-Present XiHanFun and contributors.
