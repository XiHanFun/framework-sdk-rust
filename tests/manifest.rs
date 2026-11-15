//! 清单：一致性向量与 Go SDK `manifest` 测试的移植。

mod common;

use serde::Deserialize;
use xihan_framework::manifest::{
    self, Binding, CapabilityDeclaration, CapabilityRequirement, Contributions, Isolation, Kind, Manifest, PipelineContribution, Role,
};
use xihan_framework::primitives::{ERROR_CODE_INVALID_ARGUMENT, PipelineStage, Stability};

const ECHO_MANIFEST: &str = r#"{
  "$schema": "https://spec.xihanfun.com/manifest/v1.json",
  "id": "xihan.samples.echo",
  "package": "EchoPlugin",
  "version": "5.0.0-preview.1",
  "primitives": "1",
  "stability": "experimental",
  "kind": "plugin",
  "isolation": "out-of-process",
  "requires": [ { "id": "kernel.plugins", "version": "1" } ],
  "contributes": {
    "operations": [ "echoes.create", "echoes.relay", "echoes.crash", "events.list" ],
    "config": { "section": "XiHan:Echo" },
    "health": [ "echo" ]
  },
  "permissions": [ "capability:kernel.pipeline" ],
  "activation": [ "onStartup" ],
  "entry": "EchoPlugin"
}"#;

/// 规范化输出：字段按 Schema 顺序，缺省值省略，两空格缩进。
const ECHO_WRITTEN: &str = r#"{
  "$schema": "https://spec.xihanfun.com/manifest/v1.json",
  "id": "xihan.samples.echo",
  "package": "EchoPlugin",
  "version": "5.0.0-preview.1",
  "primitives": "1",
  "stability": "experimental",
  "kind": "plugin",
  "isolation": "out-of-process",
  "requires": [
    {
      "id": "kernel.plugins",
      "version": "1"
    }
  ],
  "contributes": {
    "operations": [
      "echoes.create",
      "echoes.relay",
      "echoes.crash",
      "events.list"
    ],
    "config": {
      "section": "XiHan:Echo"
    },
    "health": [
      "echo"
    ]
  },
  "activation": [
    "onStartup"
  ],
  "permissions": [
    "capability:kernel.pipeline"
  ],
  "entry": "EchoPlugin"
}"#;

#[test]
fn read_parses_a_plugin_manifest() {
    let m = manifest::read(ECHO_MANIFEST).unwrap();
    assert_eq!(m.id, "xihan.samples.echo");
    assert_eq!((m.kind, m.isolation, m.primitives, m.stability), (Kind::Plugin, Some(Isolation::OutOfProcess), 1, Stability::Experimental));
    assert_eq!(m.requires.len(), 1);
    assert_eq!(m.requires[0].to_string(), "kernel.plugins@1");
    assert_eq!(m.operations().len(), 4);
    assert_eq!(m.config_section(), Some("XiHan:Echo"));
    assert_eq!(m.health(), ["echo"]);
    assert!(m.activates_on_startup());
    assert_eq!(m.effective_role(), Role::Provider);
    assert_eq!(manifest::domain(&m.id), Some("samples"));
    assert_eq!(manifest::domain("caching.distributed"), Some("caching"));

    let written = manifest::write(&m).unwrap();
    assert_eq!(written, ECHO_WRITTEN);
    let again = manifest::read(&written).unwrap();
    assert_eq!(again, m, "round trip");
    assert_eq!(manifest::write(&again).unwrap(), written);
}

#[test]
fn defaults_follow_the_schema() {
    let m = manifest::read(r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"provider","role":"default","bindings":[{"name":"db","type":"connection:relational"},{"name":"cache","type":"cache","required":false}],"activation":["onOperation:orders.*"]}"#).unwrap();
    assert_eq!(m.effective_role(), Role::Default);
    assert_eq!(m.effective_activation(), ["onOperation:orders.*"]);
    assert!(!m.activates_on_startup());
    assert!(m.bindings[0].is_required() && !m.bindings[1].is_required());
    assert_eq!(m.operations(), &[] as &[String]);
    assert_eq!(m.config_section(), None);

    let minimal = Manifest::new("xihan.a.b", "1.0.0", 1, Stability::Stable, Kind::Capability);
    assert_eq!(minimal.effective_activation(), ["onStartup"]);
    assert_eq!(
        manifest::write(&minimal).unwrap(),
        "{\n  \"id\": \"xihan.a.b\",\n  \"version\": \"1.0.0\",\n  \"primitives\": \"1\",\n  \"stability\": \"stable\",\n  \"kind\": \"capability\"\n}"
    );
}

/// 配置节文法与宿主一致：领域节后可接任意层 PascalCase 子节（RFC-0002「配置节」）。
#[test]
fn config_section_may_have_subsections() {
    let template = |section: &str| {
        format!(
            r#"{{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","contributes":{{"config":{{"section":"{section}"}}}}}}"#
        )
    };
    for section in ["XiHan:Caching", "XiHan:Caching:Redis", "XiHan:Protocols:CloudEvents", "XiHan:ObjectStorage:SignedUrls", "XiHan:A:B2:C"] {
        let m = manifest::read(template(section)).unwrap_or_else(|error| panic!("{section:?} must be accepted: {error}"));
        assert_eq!(m.config_section(), Some(section));
    }
    for section in [
        "Orders",
        "XiHan",
        "XiHan:",
        "XiHan:caching",
        "XiHan:Caching:",
        "XiHan:Caching::Redis",
        "XiHan:Caching:redis",
        "XiHan:Caching:Re-dis",
        "xihan:Caching",
        "XiHan:Caching:Redis:",
    ] {
        assert!(manifest::read(template(section)).is_err(), "{section:?} must be rejected");
    }
}

#[test]
fn contributed_permission_codes_follow_the_host_grammar() {
    let template = |codes: &str| {
        format!(
            r#"{{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"plugin","isolation":"out-of-process","contributes":{{"permissions":{codes}}}}}"#
        )
    };
    for codes in [r#"[]"#, r#"["orders.read"]"#, r#"["sales.orders.read","sales.orders.write"]"#, r#"["echo2.items-v2.read"]"#] {
        let m = manifest::read(template(codes)).unwrap_or_else(|error| panic!("{codes} must be accepted: {error}"));
        assert!(m.contributes.is_some(), "{codes} read without contributions");
    }
    for codes in [
        r#"["orders"]"#,
        r#"["Orders.read"]"#,
        r#"["orders.Read"]"#,
        r#"["orders..read"]"#,
        r#"["orders.read."]"#,
        r#"[".orders.read"]"#,
        r#"["1orders.read"]"#,
        r#"["orders.1read"]"#,
        r#"["orders_x.read"]"#,
        r#"["orders:read"]"#,
        r#"["orders.read","orders.read"]"#,
        r#"["orders.read","orders.write","orders.read"]"#,
    ] {
        assert!(manifest::read(template(codes)).is_err(), "{codes} must be rejected");
    }
}

#[test]
fn read_rejects_invalid_manifests() {
    let cases = [
        ("unknown field", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","extra":1}"#),
        (
            "unknown nested field",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","requires":[{"id":"a.b","version":"1","extra":true}]}"#,
        ),
        ("field names are case-sensitive", r#"{"ID":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability"}"#),
        ("missing kind", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable"}"#),
        ("bad id", r#"{"id":"a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability"}"#),
        ("bad version", r#"{"id":"xihan.a.b","version":"1.0","primitives":"1","stability":"stable","kind":"capability"}"#),
        ("bad primitives", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"0","stability":"stable","kind":"capability"}"#),
        ("numeric primitives", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":1,"stability":"stable","kind":"capability"}"#),
        ("caret primitives", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"^1","stability":"stable","kind":"capability"}"#),
        ("bad stability", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"solid","kind":"capability"}"#),
        ("bad kind", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"thing"}"#),
        ("bad role", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"provider","role":"main"}"#),
        ("plugin needs isolation", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"plugin"}"#),
        ("bad isolation", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"plugin","isolation":"vm"}"#),
        ("non-plugin isolation", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","isolation":"wasm"}"#),
        (
            "non-plugin permissions",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","permissions":["capability:x"]}"#,
        ),
        (
            "duplicate provides",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","provides":[{"id":"a.b","version":"1"},{"id":"a.b","version":"2"}]}"#,
        ),
        (
            "caret provides",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","provides":[{"id":"a.b","version":"^1"}]}"#,
        ),
        (
            "bad provides",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","provides":[{"id":"A.b","version":"1"}]}"#,
        ),
        (
            "duplicate requires",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","requires":[{"id":"a.b","version":"1"},{"id":"a.b","version":"^1"}]}"#,
        ),
        (
            "bad requires",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","requires":[{"id":"a","version":"1"}]}"#,
        ),
        (
            "bad activation",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"plugin","isolation":"wasm","activation":["onBoot"]}"#,
        ),
        (
            "bad permission",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"plugin","isolation":"wasm","permissions":["sudo"]}"#,
        ),
        (
            "bad operation",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","contributes":{"operations":["Orders.Create"]}}"#,
        ),
        (
            "bad section",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","contributes":{"config":{"section":"Orders"}}}"#,
        ),
        (
            "bad stage",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","contributes":{"pipeline":[{"middleware":"x","stage":"middle"}]}}"#,
        ),
        (
            "empty middleware",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","contributes":{"pipeline":[{"middleware":"","stage":"observe"}]}}"#,
        ),
        ("bad health", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","contributes":{"health":["Echo"]}}"#),
        (
            "bad binding",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","bindings":[{"name":"Db","type":"connection:relational"}]}"#,
        ),
        (
            "duplicate binding",
            r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","bindings":[{"name":"db","type":"queue"},{"name":"db","type":"blob"}]}"#,
        ),
        ("null list", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","provides":null}"#),
        ("trailing data", r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability"} {}"#),
        ("not json", "{"),
    ];
    for (name, data) in cases {
        let error = manifest::read(data).expect_err(name);
        assert_eq!(error.code(), ERROR_CODE_INVALID_ARGUMENT, "{name}");
    }
    let missing = manifest::read_file("does-not-exist/xihan.manifest.json").unwrap_err();
    assert_eq!((missing.code(), missing.message()), (ERROR_CODE_INVALID_ARGUMENT, "The manifest file could not be read."));
}

#[derive(Deserialize)]
struct RequirementVectors {
    valid: Vec<RequirementCase>,
    invalid: Vec<String>,
}

#[derive(Deserialize)]
struct RequirementCase {
    input: String,
    id: String,
    version: u32,
    optional: bool,
}

#[test]
fn requirements_follow_the_vectors() {
    let vectors: RequirementVectors = common::vector("primitives/capability-requirements.json");
    for case in &vectors.valid {
        let requirement = CapabilityRequirement::parse(&case.input).unwrap_or_else(|| panic!("{:?} must parse", case.input));
        assert_eq!((requirement.id.as_str(), requirement.version, requirement.optional), (case.id.as_str(), case.version, case.optional), "{:?}", case.input);
    }
    for text in &vectors.invalid {
        assert!(CapabilityRequirement::parse(text).is_none(), "{text:?} must not parse");
        assert_eq!(text.parse::<CapabilityRequirement>().unwrap_err().code(), ERROR_CODE_INVALID_ARGUMENT);
    }
    let declaration: CapabilityDeclaration = "caching.distributed@2".parse().unwrap();
    assert_eq!(declaration.to_string(), "caching.distributed@2");
    let requirement = CapabilityRequirement::parse("caching.distributed@^2?").unwrap();
    assert!(requirement.is_satisfied_by(&declaration));
    assert_eq!(requirement.to_string(), "caching.distributed@2?");
    assert!(!requirement.is_satisfied_by(&CapabilityDeclaration::parse("caching.distributed@1").unwrap()), "versions must match");
    for text in ["caching.distributed@^2", "caching.distributed", "caching.distributed@0", "Caching.x@1"] {
        assert!(CapabilityDeclaration::parse(text).is_none(), "{text:?} must not parse as a declaration");
    }
}

/// 与 .NET 宿主同一规则（RFC-0017「范围」）：只有 Wasm 插件能贡献中间件，进程外与进程内插件的清单写了非空
/// `contributes.pipeline` 在读清单时即被拒绝；能力包等非插件照常可以写。
#[test]
fn only_wasm_plugins_may_contribute_middleware() {
    let template = |kind: &str, isolation: &str, pipeline: &str| {
        format!(
            r#"{{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"{kind}"{isolation},"contributes":{{"pipeline":{pipeline}}}}}"#
        )
    };
    let audit = r#"[{"middleware":"audit","stage":"observe"},{"middleware":"tenant-guard","stage":"tenant"}]"#;
    for isolation in [Isolation::OutOfProcess, Isolation::InProcess] {
        let error = manifest::read(template("plugin", &format!(r#","isolation":"{isolation}""#), audit))
            .expect_err("a non-wasm plugin contributing middleware must be rejected");
        assert_eq!(error.code(), ERROR_CODE_INVALID_ARGUMENT);
        for fragment in ["'xihan.a.b'", "'contributes.pipeline'", "audit, tenant-guard", isolation.name()] {
            assert!(error.message().contains(fragment), "the message for a {isolation} plugin must mention {fragment}: {}", error.message());
        }
    }
    let accepted = [
        ("wasm plugin", template("plugin", r#","isolation":"wasm""#, audit)),
        ("capability", template("capability", "", audit)),
        ("provider", template("provider", "", audit)),
        ("out-of-process, empty list", template("plugin", r#","isolation":"out-of-process""#, "[]")),
        ("in-process, empty list", template("plugin", r#","isolation":"in-process""#, "[]")),
    ];
    for (name, data) in accepted {
        let m = manifest::read(&data).unwrap_or_else(|error| panic!("{name} must be accepted: {error}"));
        if name == "wasm plugin" {
            assert_eq!(m.contributes.as_ref().unwrap().pipeline[1].stage, PipelineStage::Tenant);
        }
    }

    // 写出同样核对：不生成宿主会拒绝的清单
    let mut written = Manifest::new("xihan.a.b", "1.0.0", 1, Stability::Stable, Kind::Plugin);
    written.isolation = Some(Isolation::OutOfProcess);
    let mut contributes = Contributions::new();
    contributes.pipeline.push(PipelineContribution::new("audit", PipelineStage::Observe));
    written.contributes = Some(contributes);
    assert!(manifest::write(&written).is_err(), "writing an out-of-process plugin manifest that contributes middleware must fail");
}

#[test]
fn write_validates_programmatic_manifests() {
    let mut m = Manifest::new("xihan.a.b", "1.0.0", 1, Stability::Preview, Kind::Provider);
    m.provides.push(CapabilityDeclaration::new("persistence.relational", 1));
    m.requires.push(CapabilityRequirement::new("kernel.hosting", 1));
    let mut optional = CapabilityRequirement::new("multitenancy.context", 2);
    optional.optional = true;
    m.requires.push(optional);
    m.bindings.push(Binding::new("db", "connection:relational"));
    let written = manifest::write(&m).unwrap();
    assert!(written.contains(r#""version": "2","#) && written.contains(r#""optional": true"#), "{written}");
    assert_eq!(manifest::read(&written).unwrap(), m);

    m.provides.push(CapabilityDeclaration::new("cache.local", 0));
    assert!(manifest::write(&m).unwrap_err().message().contains("the provided capability 'cache.local' of 'xihan.a.b' is not valid."));
    m.provides.pop();
    m.primitives = 0;
    assert_eq!(manifest::write(&m).unwrap_err().message(), "the primitives version of 'xihan.a.b' must be a positive integer.");
}
