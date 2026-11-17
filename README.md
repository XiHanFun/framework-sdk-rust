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

XiHan Framework SDK for Rust is the Rust implementation of [XiHan Spec](https://github.com/XiHanFun/framework-spec), providing the primitives layer, manifests, the invocation context, the pipeline and operation contracts, with semantics and wire encoding defined by the spec's RFCs; the .NET reference implementation is [XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework). The spec sits in `spec/` as a submodule, and conformance is verified by the spec repository's vectors: the tests read `spec/conformance/vectors/` directly instead of copying the vector files.

## Modules

The whole SDK is a single crate, `xihan-framework`, and every language's SDK is a single package; domain words follow the spec's vocabulary (`conventions/vocabulary.md`), with only the casing changed to this language's style.

| Module | Contents | Spec |
| --- | --- | --- |
| `primitives` | `Error` / error codes, `Result<T>` (the standard `Result<T, Error>`) and its wire encoding `WireResult`, `Maybe<T>` (`Option<T>`), `TypedId<Tag, V>`, `LifecycleState`, `PipelineStage`, `Stability`, Baggage key and value constraints, message keys and templates (with number symbols per locale), `Catalog` and the process default resolver, `FrameworkError`; `primitives::json`: the shared JSON text check (vector `primitives/json-text.json`), reading by the value model (`json::from_slice`) and writing (`json::Model`, `json::to_value`) | RFC-0001, RFC-0010, `conventions/` |
| `manifest` | The model of `xihan.manifest.json`, reading, writing in the spec's canonical layout, and validation; capability declarations and requirement text (versions without leading zeros, at most 2147483647); reading follows the order of RFC-0002 parsing rule 2 (vector `manifest/manifests.json`): JSON text → `null` → item-by-item check against the Schema → cross-field constraints, and every error is `KERNEL_CAPABILITY_006` with a message key and a path or the manifest ID | RFC-0002, `manifest/manifest.schema.json` |
| `invocation` | `Context`: an awaitable `CancellationToken` that propagates from parent to child, deadlines fired by a background timer, W3C tracing (`traceparent` accepts only version `00` and lowercase hex, and an invalid one is treated as absent; sampling looks only at the lowest bit and writes `00` or `01`, writing `01` when there is no upstream), registered Baggage (values of registered keys are non-empty, at most 256 UTF-16 code units and free of control characters, except the idempotency key: building a context treats members as external input, dropping non-conforming ones and keeping the existing value, while `with_baggage` panics on a non-conforming value), and an item bag accessed by type; timeouts and cancellation are attributed to `KERNEL_TIMEOUT_001` / `KERNEL_CANCELLED_001` by the deadline token alone | RFC-0003 |
| `pipeline` | The eleven-stage `Builder`: `use_middleware` / `use_at` / `use_named_instance`, `before` / `after` (instances registered by name use the instance name, everything else the middleware name; the subject of a constraint must already be registered when it is declared), `describe` / `dump` / `build`; ordering rules, error codes `KERNEL_PIPELINE_00x` and the exact English error messages (vector `pipeline/ordering.json`, where an instance registered by name is written `middleware-name[instance-name]`): registration methods take `&mut self` and return `Result`, duplicate registration and an unregistered constraint subject are reported at registration, while an unregistered constraint target, cross-stage constraints and cycles are reported at ordering; middleware and terminals are all asynchronous | RFC-0004 |
| `operation` | `Contract`, `Schema`, the asynchronous `Handler<Req, Res>`, a `Registry` with a capacity of 10000, and an `Invoker` that runs through the full pipeline (XGI: JSON request and response; plus a typed entry point that bypasses JSON); the pipeline carries three built-in steps, placed where the .NET built-in middleware sits; kernel error codes, keys, arguments and en-US templates match vector `kernel-messages.json` (`type_mismatch` carries only `id`); payloads first pass the shared JSON text check, the first problem in text order is reported, and `reason` is a JSON path (vector `operations/payload-reasons.json`); members holding arbitrary JSON use `json::Json` to keep numbers verbatim; the response is encoded only after the pipeline finishes, and a response that cannot be written is reported as `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception`; middleware that short-circuits with success but no response is reported as `KERNEL_UNEXPECTED_001` + `kernel.operation.no_response` | RFC-0011 |

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
4. `Context` deadlines are fired on time by a background thread in the process (`xihan-deadline-timer`, started when the first deadline is registered); `Context::done()` completes when the caller cancels or the deadline is reached, and `Context::cancellation()` gives `KERNEL_TIMEOUT_001` (`kernel.invocation.deadline_exceeded`) or `KERNEL_CANCELLED_001` (`kernel.invocation.cancelled`). The invoker's pipeline holds three built-in steps, all placed at `Beginning` of their stage under the owner `xihan.kernel.core` (`operation::KERNEL_OWNER`) and ordered against middleware in the same position by RFC-0004: `ObserveMiddleware` in the Observe stage translates panics from the middleware and handler after it into `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception` (a `FrameworkError` payload goes into `inner`), while panics from middleware before it reach the caller unchanged; `DeadlineMiddleware` in the Resilience stage checks the deadline against the clock on entry and, if it has passed, reports `KERNEL_TIMEOUT_001` + `kernel.invocation.deadline_before_start` without running the middleware and handler after it (the Ingress and Observe stages still run); `ValidationMiddleware` in the Validate stage decodes the payload of raw calls and is skipped for typed calls. Before the pipeline, only an unregistered operation and a type mismatch of a typed call are checked, in that order. Translating panics requires `panic = "unwind"`. The XGI entry point encodes the response into a payload only after the pipeline finishes, so middleware sees the object before encoding through `operation::with_response` (the response of a typed call is written to the same slot, like .NET's `FindOperationResponse`, just not encoded); when it cannot be written (a typed float that is NaN or infinite, a value the serializer does not support, a panic during serialization), the call ends with `KERNEL_UNEXPECTED_001` + `kernel.invocation.unhandled_exception` instead of propagating to the caller. Cancellation and deadlines are signaled only through the token: the invoker and the built-in steps neither abort the handler nor drop its future, the result the handler returns after the token fires is the result of the call, and attribution by the token happens only when the handler turns cancellation into its result (`Err(ctx.cancellation()…)`).

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
| `getrandom` | Random numbers for trace IDs and span IDs |
| `serde_path_to_error` | The JSON path of the error location when a payload is invalid (`$.lines[1].quantity`): RFC-0011 rule 2 requires `reason` to contain only the path, and `serde_json` itself reports only line and column |

The shared JSON text check is a built-in byte-by-byte reader (`primitives::json`) that produces a value tree in document order, which a built-in serde `Deserializer` then reads into the target type: `serde_json` keeps the last of duplicate members, does not check lone surrogate escapes, reads `-0` as a float and rejects `1e400`, and `Value` sorts keys, none of which matches the spec.

The grammars in the spec (error codes, message keys, Baggage keys, manifest fields, `traceparent` and so on) are hand-written byte-by-byte checks with no regex library; the cancellation token and deadlines are a lightweight built-in implementation with no async runtime. There are no dev dependencies: the conformance tests use `RawValue` to take the manifests out of the vectors as written, keeping members in document order, and the tests run futures on a minimal hand-written executor.

## Development

```bash
git submodule update --init
```

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

```bash
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

The minimum Rust version is 1.85 (required by edition 2024), and clippy's `incompatible_msrv` checks standard library APIs against it. `spec/` and `tests/` are not included in the crate package.

## Status

`0.1`: primitives, manifests, the invocation context, the pipeline and operations are usable and pass all vectors for primitives (including `json-text.json`), manifest, pipeline and operations under `conformance/vectors/` in the spec repository (not yet published to crates.io, where 0.0.1 is a name placeholder). Planned:

- the out-of-process plugin runtime `plugin` (`xihan.plugin.v1`, based on tonic, enabled through a cargo feature; Unix sockets / Windows named pipes / TCP loopback), verified with the spec repository's black-box runner `xihan-conformance plugin --dir <artifact-dir>`;
- the Wasm plugin ABI (RFC-0018, `xihan_*` exports, `xihan` imports, WASI preview 1 reactor), with a template in XiHan.Framework's `samples/EchoWasmPlugin`;
- operation sets for remote providers (RFC-0019, starting with `caching.distributed@1`) and sample plugins.

## Ecosystem

| Project | Description |
| --- | --- |
| [framework-spec](https://github.com/XiHanFun/framework-spec) | XiHan Spec: RFCs, vocabulary, protocol mappings and conformance vectors |
| [XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework) | The .NET reference implementation |
| [framework-sdk-go](https://github.com/XiHanFun/framework-sdk-go) | Go SDK, including the out-of-process plugin runtime |
| [framework-sdk-typescript](https://github.com/XiHanFun/framework-sdk-typescript) | TypeScript SDK |
| [framework-sdk-java](https://github.com/XiHanFun/framework-sdk-java) | Java SDK |
| [framework-sdk-python](https://github.com/XiHanFun/framework-sdk-python) | Python SDK |
| [framework-sdk-php](https://github.com/XiHanFun/framework-sdk-php) | PHP SDK |

## Contributing

[Issues](https://github.com/XiHanFun/framework-sdk-rust/issues) and pull requests are welcome. Changes to public contracts are not made in the SDK repositories: they go through the RFC process in [framework-spec](https://github.com/XiHanFun/framework-spec) and are then synced to every SDK.

## License

[MIT](./LICENSE) © 2021-Present XiHanFun and contributors.
