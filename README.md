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
  <a href="https://framework.docs.xihanfun.com/sdks/rust"><b>Docs</b></a> ·
  <a href="#getting-started"><b>Getting Started</b></a> ·
  <a href="https://github.com/XiHanFun/framework-spec"><b>Specification</b></a> ·
  <a href="https://github.com/XiHanFun/XiHan.Framework"><b>Reference Implementation</b></a>
</p>

<p><b>English</b> | <a href="./README_cn.md">简体中文</a></p>

</div>

XiHan Framework SDK for Rust is the Rust implementation of [XiHan Spec](https://github.com/XiHanFun/framework-spec), providing the primitives layer, manifests, the invocation context, the pipeline and operation contracts, plus the out-of-process plugin runtime (cargo feature `plugin`), with semantics and wire encoding defined by the spec's RFCs; the .NET reference implementation is [XiHan.Framework](https://github.com/XiHanFun/XiHan.Framework). The spec sits in `spec/` as a submodule, and conformance is verified by the spec repository's vectors.

## Modules

The whole SDK is a single crate, `xihan-framework`; domain words follow the spec's vocabulary, with only the casing changed to this language's style.

| Module | Contents | Spec |
| --- | --- | --- |
| `primitives` | `Error` and error codes, `Result<T>` and its wire encoding, `Maybe<T>`, `TypedId`, Baggage, message keys and templates, the shared JSON text check | RFC-0001, RFC-0010 |
| `manifest` | Reading, canonical writing and validation of `xihan.manifest.json` | RFC-0002 |
| `invocation` | `Context`: cancellation, deadlines, W3C tracing, registered Baggage | RFC-0003 |
| `pipeline` | The eleven-stage `Builder` and its ordering rules | RFC-0004 |
| `operation` | `Contract`, `Handler`, `Registry` and an `Invoker` that runs through the full pipeline | RFC-0011 |
| `plugin` (feature `plugin`) | The out-of-process plugin runtime on tonic: handshake, lifecycle, invocations, events and host callbacks | RFC-0017 |

Without the `plugin` feature the crate depends on no async runtime.

## Installation

```bash
cargo add xihan-framework
```

The plugin runtime is behind the cargo feature `plugin` (`--features plugin`). The crate is not yet published to crates.io: the version there is only a name placeholder.

## Getting Started

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

Middleware, handlers and the pipeline terminal return `BoxFuture`; hand the futures to tokio or any executor. Writing a plugin, the rules behind this example and the behavior shared with the host are in the documentation.

## Documentation

The documentation site is in Chinese.

| Where | Contents |
| --- | --- |
| [Rust SDK](https://framework.docs.xihanfun.com/sdks/rust) | Modules in detail, getting started, writing a plugin, behavior shared with the host, language mapping, design, conformance, breaking changes and migration |
| [Out-of-process plugins](https://framework.docs.xihanfun.com/plugins/out-of-process) | The plugin protocol and the host side |
| [Spec and languages](https://framework.docs.xihanfun.com/appendix/spec) | How the spec is organized, conformance profiles and every language's SDK |
| [XiHan Spec](https://github.com/XiHanFun/framework-spec) | RFCs, vocabulary, protocol mappings and conformance vectors |

## Status

Experimental until there is a release pinned to a spec tag; what is not provided yet is listed in the documentation.

- Pinned spec version: the spec tag the `spec/` submodule points to.
- Claimed profiles (RFC-0024): `core` and `plugin`.
- Gaps: none.

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

[Issues](https://github.com/XiHanFun/framework-sdk-rust/issues) and pull requests are welcome. Changes to public contracts are not made in the SDK repositories: they go through an RFC in [framework-spec](https://github.com/XiHanFun/framework-spec) first and are then synced to every SDK. Development conventions, gates and the release process are in [AGENTS.md](./AGENTS.md).

## License

[MIT](./LICENSE) © 2021-Present XiHanFun and contributors.
