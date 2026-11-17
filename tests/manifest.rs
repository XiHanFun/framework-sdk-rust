//! 清单（RFC-0002）：回放向量 `manifest/manifests.json` 与 `primitives/capability-requirements.json`，以及读取的检查顺序、
//! 跨字段约束、写出与再读。

mod common;

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use xihan_framework::manifest::{
    self, Binding, CapabilityDeclaration, CapabilityRequirement, ERROR_CODE_INVALID_MANIFEST, Isolation, Kind, MESSAGE_DUPLICATE_BINDINGS,
    MESSAGE_DUPLICATE_PROVIDES, MESSAGE_DUPLICATE_REQUIRES, MESSAGE_EMPTY, MESSAGE_FILE_MISSING, MESSAGE_INVALID, MESSAGE_INVALID_JSON,
    MESSAGE_ISOLATION_NOT_ALLOWED, MESSAGE_ISOLATION_REQUIRED, Manifest, Role,
};
use xihan_framework::primitives::{self, ERROR_CODE_INVALID_ARGUMENT, FrameworkError, PipelineStage, Stability};

const ECHO_MANIFEST: &str = r#"{
  "$schema": "https://spec.xihanfun.com/manifest/v1.json",
  "id": "xihan.samples.echo",
  "package": "EchoPlugin",
  "version": "5.0.0-preview.1",
  "primitives": "1",
  "stability": "experimental",
  "kind": "plugin",
  "isolation": "out-of-process",
  "requires": [ { "id": "kernel.plugins", "version": "^1" } ],
  "contributes": {
    "operations": [ "echoes.create", "echoes.relay", "echoes.crash", "events.list" ],
    "config": { "section": "XiHan:Echo" },
    "health": [ "echo" ]
  },
  "permissions": [ "capability:kernel.pipeline" ],
  "activation": [ "onStartup" ],
  "entry": "EchoPlugin"
}"#;

/// 规范的排版（RFC-0002「写出」）：成员顺序固定，缺省值写出，列表总写，版本不带 ^。
const ECHO_WRITTEN: &str = r#"{
  "$schema": "https://spec.xihanfun.com/manifest/v1.json",
  "id": "xihan.samples.echo",
  "package": "EchoPlugin",
  "version": "5.0.0-preview.1",
  "primitives": "1",
  "stability": "experimental",
  "kind": "plugin",
  "role": "provider",
  "isolation": "out-of-process",
  "provides": [],
  "requires": [
    {
      "id": "kernel.plugins",
      "version": "1",
      "optional": false
    }
  ],
  "bindings": [],
  "contributes": {
    "pipeline": [],
    "operations": [
      "echoes.create",
      "echoes.relay",
      "echoes.crash",
      "events.list"
    ],
    "config": {
      "section": "XiHan:Echo"
    },
    "permissions": [],
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

/// 核对读取失败的错误：码、消息键与参数。
fn assert_error(error: &FrameworkError, key: &str, arguments: &[(&str, &str)], context: &str) {
    assert_eq!((error.code(), error.message_key()), (ERROR_CODE_INVALID_MANIFEST, Some(key)), "{context}: {error}");
    let actual: BTreeMap<&str, &str> = error.details().into_iter().flatten().map(|(name, value)| (name.as_str(), value.as_str().unwrap())).collect();
    assert_eq!(actual, arguments.iter().copied().collect(), "{context}: {error}");
}

/// 一份最小的能力清单，`extra` 接在必填成员后面。
fn minimal(extra: &str) -> String {
    format!(r#"{{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability"{extra}}}"#)
}

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
    // 读出时填上了缺省值，写出的清单再读回来与原来的相等
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

    // 读出时与 .NET 的类型化模型一样填上缺省值；程序里创建的清单也是同样的缺省值
    let minimal = Manifest::new("xihan.a.b", "1.0.0", 1, Stability::Stable, Kind::Capability);
    let read = manifest::read(
        r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","bindings":[{"name":"db","type":"queue"}]}"#,
    )
    .unwrap();
    assert_eq!((read.role, read.activation.as_slice(), read.bindings[0].required), (Role::Provider, ["onStartup".to_owned()].as_slice(), true));
    assert_eq!((minimal.role, minimal.activation.as_slice(), &minimal.contributes), (Role::Provider, ["onStartup".to_owned()].as_slice(), &read.contributes));
    assert_eq!(minimal.effective_activation(), ["onStartup"]);
    assert_eq!(
        manifest::write(&minimal).unwrap(),
        "{\n  \"$schema\": \"https://spec.xihanfun.com/manifest/v1.json\",\n  \"id\": \"xihan.a.b\",\n  \"version\": \"1.0.0\",\n  \"primitives\": \"1\",\n  \"stability\": \"stable\",\n  \"kind\": \"capability\",\n  \"role\": \"provider\",\n  \"provides\": [],\n  \"requires\": [],\n  \"bindings\": [],\n  \"contributes\": {\n    \"pipeline\": [],\n    \"operations\": [],\n    \"permissions\": [],\n    \"health\": []\n  },\n  \"activation\": [\n    \"onStartup\"\n  ],\n  \"permissions\": []\n}"
    );
}

/// 配置节文法与宿主一致：领域节后可接任意层 PascalCase 子节（RFC-0002「配置节」）。
#[test]
fn config_section_may_have_subsections() {
    let template = |section: &str| minimal(&format!(r#","contributes":{{"config":{{"section":"{section}"}}}}"#));
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
        let error = manifest::read(template(section)).expect_err(section);
        assert_error(&error, MESSAGE_INVALID, &[("reason", "$.contributes.config.section")], section);
    }
}

#[test]
fn contributed_permission_codes_follow_the_host_grammar() {
    let template = |codes: &str| minimal(&format!(r#","contributes":{{"permissions":{codes}}}"#));
    for codes in [r#"[]"#, r#"["orders.read"]"#, r#"["sales.orders.read","sales.orders.write"]"#, r#"["echo2.items-v2.read"]"#] {
        let m = manifest::read(template(codes)).unwrap_or_else(|error| panic!("{codes} must be accepted: {error}"));
        assert_eq!(m.contributes.permissions.is_empty(), codes == "[]", "{codes}");
    }
    for (codes, path) in [
        (r#"["orders"]"#, "$.contributes.permissions[0]"),
        (r#"["Orders.read"]"#, "$.contributes.permissions[0]"),
        (r#"["orders.Read"]"#, "$.contributes.permissions[0]"),
        (r#"["orders..read"]"#, "$.contributes.permissions[0]"),
        (r#"["orders.read."]"#, "$.contributes.permissions[0]"),
        (r#"[".orders.read"]"#, "$.contributes.permissions[0]"),
        (r#"["1orders.read"]"#, "$.contributes.permissions[0]"),
        (r#"["orders.1read"]"#, "$.contributes.permissions[0]"),
        (r#"["orders_x.read"]"#, "$.contributes.permissions[0]"),
        (r#"["orders:read"]"#, "$.contributes.permissions[0]"),
        // 重复时报第二次出现的项
        (r#"["orders.read","orders.read"]"#, "$.contributes.permissions[1]"),
        (r#"["orders.read","orders.write","orders.read"]"#, "$.contributes.permissions[2]"),
    ] {
        assert_error(&manifest::read(template(codes)).expect_err(codes), MESSAGE_INVALID, &[("reason", path)], codes);
    }
}

/// 读取的错误都是 KERNEL_CAPABILITY_006（RFC-0002 解析规则 2）：形状问题写路径，跨字段约束带清单 ID。
#[test]
fn read_rejects_invalid_manifests() {
    let base = r#""id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable""#;
    let cases = [
        (r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability","extra":1}"#, "$.extra"),
        (&*minimal(r#","requires":[{"id":"a.b","version":"1","extra":true}]"#), "$.requires[0].extra"),
        (r#"{"ID":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability"}"#, "$.ID"),
        (r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable"}"#, "$.kind"),
        (r#"{"id":"a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"capability"}"#, "$.id"),
        (r#"{"id":"xihan.a.b","version":"1.0","primitives":"1","stability":"stable","kind":"capability"}"#, "$.version"),
        (r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"0","stability":"stable","kind":"capability"}"#, "$.primitives"),
        (r#"{"id":"xihan.a.b","version":"1.0.0","primitives":1,"stability":"stable","kind":"capability"}"#, "$.primitives"),
        (r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"^1","stability":"stable","kind":"capability"}"#, "$.primitives"),
        (r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"solid","kind":"capability"}"#, "$.stability"),
        (r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"thing"}"#, "$.kind"),
        (r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"provider","role":"main"}"#, "$.role"),
        (r#"{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"plugin","isolation":"vm"}"#, "$.isolation"),
        (&*minimal(r#","provides":[{"id":"a.b","version":"^1"}]"#), "$.provides[0].version"),
        (&*minimal(r#","provides":[{"id":"A.b","version":"1"}]"#), "$.provides[0].id"),
        (&*minimal(r#","requires":[{"id":"a","version":"1"}]"#), "$.requires[0].id"),
        (&*minimal(r#","activation":["onBoot"]"#), "$.activation[0]"),
        (&*minimal(r#","permissions":["sudo"]"#), "$.permissions[0]"),
        (&*minimal(r#","contributes":{"operations":["Orders.Create"]}"#), "$.contributes.operations[0]"),
        (&*minimal(r#","contributes":{"pipeline":[{"middleware":"x","stage":"middle"}]}"#), "$.contributes.pipeline[0].stage"),
        (&*minimal(r#","contributes":{"pipeline":[{"middleware":"","stage":"observe"}]}"#), "$.contributes.pipeline[0].middleware"),
        (&*minimal(r#","contributes":{"health":["Echo"]}"#), "$.contributes.health[0]"),
        (&*minimal(r#","bindings":[{"name":"Db","type":"connection:relational"}]"#), "$.bindings[0].name"),
        (&*minimal(r#","provides":null"#), "$.provides"),
    ];
    for (data, path) in cases {
        assert_error(&manifest::read(data).expect_err(data), MESSAGE_INVALID, &[("reason", path)], data);
    }
    for (data, key) in [
        (format!(r#"{{{base},"kind":"plugin"}}"#), MESSAGE_ISOLATION_REQUIRED),
        (minimal(r#","isolation":"wasm""#), MESSAGE_ISOLATION_NOT_ALLOWED),
        (minimal(r#","permissions":["capability:x"]"#), MESSAGE_ISOLATION_NOT_ALLOWED),
        (minimal(r#","provides":[{"id":"a.b","version":"1"},{"id":"a.b","version":"2"}]"#), MESSAGE_DUPLICATE_PROVIDES),
        (minimal(r#","requires":[{"id":"a.b","version":"1"},{"id":"a.b","version":"^1"}]"#), MESSAGE_DUPLICATE_REQUIRES),
        (minimal(r#","bindings":[{"name":"db","type":"queue"},{"name":"db","type":"blob"}]"#), MESSAGE_DUPLICATE_BINDINGS),
    ] {
        assert_error(&manifest::read(&data).expect_err(&data), key, &[("id", "xihan.a.b")], &data);
    }
    for data in [format!("{} {{}}", minimal("")), "{".to_owned(), String::new()] {
        assert_error(&manifest::read(&data).expect_err(&data), MESSAGE_INVALID_JSON, &[("reason", "$")], &data);
    }
    assert_error(&manifest::read("null").unwrap_err(), MESSAGE_EMPTY, &[], "null");
    let error = manifest::read(minimal(r#","provides":[]"#).replace(r#""kind":"capability""#, r#""kind":"capability","kind":"plugin""#)).unwrap_err();
    assert_error(&error, MESSAGE_INVALID_JSON, &[("reason", "$.kind")], "duplicate member");
    assert_eq!(error.message(), "The manifest is not valid JSON: $.kind", "the built-in en-US template");
}

/// 文件按 UTF-8 读、跳过开头的 UTF-8 BOM；文本里的 BOM 是语法错误；文件不存在为 file_missing。
#[test]
fn read_file_skips_the_bom() {
    let directory = std::env::temp_dir().join(format!("xihan-manifest-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(manifest::FILE_NAME);
    std::fs::write(&path, [b"\xEF\xBB\xBF".as_slice(), minimal("").as_bytes()].concat()).unwrap();
    assert_eq!(manifest::read_file(&path).unwrap().id, "xihan.a.b");
    assert_error(&manifest::read(std::fs::read(&path).unwrap()).unwrap_err(), MESSAGE_INVALID_JSON, &[("reason", "$")], "BOM in the text");
    std::fs::write(&path, b"{\"id\":\"\xFF\"}").unwrap();
    assert_error(&manifest::read_file(&path).unwrap_err(), MESSAGE_INVALID_JSON, &[("reason", "$")], "invalid UTF-8");
    std::fs::remove_dir_all(&directory).unwrap();

    let missing = "does-not-exist/xihan.manifest.json";
    let error = manifest::read_file(missing).unwrap_err();
    assert_error(&error, MESSAGE_FILE_MISSING, &[("path", missing)], missing);
    assert_eq!(error.message(), "Manifest file 'does-not-exist/xihan.manifest.json' does not exist.");
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

/// 能力依赖按 capability-requirements.json：版本不带前导零、符号与空白，不超过 2147483647，按整串匹配（ADR-0236）。
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
    for text in ["caching.distributed@^2", "caching.distributed", "caching.distributed@0", "Caching.x@1", "caching.distributed@2147483648"] {
        assert!(CapabilityDeclaration::parse(text).is_none(), "{text:?} must not parse as a declaration");
    }
}

/// 只有 Wasm 插件能贡献中间件是宿主在发现插件时的核对（PLUGINS_VALIDATION_001，RFC-0017「范围」）；
/// 读取清单只按 RFC-0002 解析规则 2，与 .NET 的 ManifestReader 一样不管它。
#[test]
fn pipeline_contributions_are_left_to_the_host() {
    for isolation in ["out-of-process", "in-process", "wasm"] {
        let data = format!(
            r#"{{"id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable","kind":"plugin","isolation":"{isolation}","contributes":{{"pipeline":[{{"middleware":"audit","stage":"observe","before":["x"]}},{{"middleware":"tenant-guard","stage":"tenant"}}]}}}}"#
        );
        let m = manifest::read(&data).unwrap_or_else(|error| panic!("{isolation}: {error}"));
        let pipeline = &m.contributes.pipeline;
        assert_eq!((pipeline[0].before.as_slice(), pipeline[1].stage), (["x".to_owned()].as_slice(), PipelineStage::Tenant));
        let written = manifest::write(&m).unwrap();
        assert!(
            written.contains("\"pipeline\": [\n      {\n        \"middleware\": \"audit\",\n        \"stage\": \"observe\",\n        \"before\": [\n          \"x\"\n        ],\n        \"after\": []\n      },"),
            "{written}"
        );
    }
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
    assert_eq!(manifest::write(&manifest::read(&written).unwrap()).unwrap(), written);

    // 格式按读取的同一套 Schema 核对，说明带路径；跨字段约束带消息键，码与 .NET 的 Manifest.Validate 一样
    m.provides.push(CapabilityDeclaration::new("cache.local", 0));
    let error = manifest::write(&m).unwrap_err();
    assert_eq!((error.code(), error.message()), (ERROR_CODE_INVALID_ARGUMENT, "The manifest 'xihan.a.b' is not valid at '$.provides[1].version'."));
    m.provides.pop();
    m.primitives = 2_147_483_648;
    assert_eq!(manifest::write(&m).unwrap_err().message(), "The manifest 'xihan.a.b' is not valid at '$.primitives'.");
    m.primitives = 1;
    m.isolation = Some(Isolation::Wasm);
    let error = manifest::write(&m).unwrap_err();
    assert_eq!((error.code(), error.message_key()), (ERROR_CODE_INVALID_ARGUMENT, Some(MESSAGE_ISOLATION_NOT_ALLOWED)));
    assert_eq!(error.message(), "Manifest 'xihan.a.b' is not a plugin and must not declare 'isolation' or 'permissions'.");
}

/// 读取按 Schema 逐字执行（RFC-0002 解析规则 2）：可选成员写成 null 不合法，contributes.operations 按操作 ID 文法。
#[test]
fn reading_follows_the_schema_verbatim() {
    let base = r#""id":"xihan.a.b","version":"1.0.0","primitives":"1","stability":"stable""#;
    for (member, path) in [
        (r#""$schema":null"#, "$.$schema"),
        (r#""package":null"#, "$.package"),
        (r#""role":null"#, "$.role"),
        (r#""contributes":null"#, "$.contributes"),
        (r#""entry":null"#, "$.entry"),
        (r#""activation":null"#, "$.activation"),
        (r#""requires":[{"id":"a.b","version":"1","optional":null}]"#, "$.requires[0].optional"),
        (r#""bindings":[{"name":"db","type":"queue","required":null}]"#, "$.bindings[0].required"),
        (r#""contributes":{"config":null}"#, "$.contributes.config"),
        (r#""contributes":{"config":{"section":"XiHan:A","schema":null}}"#, "$.contributes.config.schema"),
    ] {
        let data = format!(r#"{{{base},"kind":"provider",{member}}}"#);
        assert_error(&manifest::read(&data).expect_err(member), MESSAGE_INVALID, &[("reason", path)], member);
    }
    let error = manifest::read(format!(r#"{{{base},"kind":"plugin","isolation":null}}"#)).unwrap_err();
    assert_error(&error, MESSAGE_INVALID, &[("reason", "$.isolation")], "a null isolation is not a missing one");

    let operations = |operation: &str| minimal(&format!(r#","contributes":{{"operations":["{operation}"]}}"#));
    for operation in ["orders.create", "order-lines.list", "a1.b2", "tenant-members.list-online"] {
        assert!(manifest::read(operations(operation)).is_ok(), "{operation} must be accepted");
    }
    for operation in
        ["orders-.create", "orders.-create", "order--lines.list", "orders.create-", "orders", "orders.create.now", "Orders.create", "orders_x.create"]
    {
        assert!(manifest::read(operations(operation)).is_err(), "{operation} must be rejected");
    }
}

/// 对象位置只收 JSON 对象：serde 派生的结构体会把字段同序的数组读成结构体，形状核对按类型不符报出路径。
#[test]
fn object_positions_reject_arrays() {
    for (member, path) in [
        (r#","provides":[["orders.store","1"]]"#, "$.provides[0]"),
        (r#","requires":[{"id":"a.b","version":"1"},["a.c","1",false]]"#, "$.requires[1]"),
        (r#","bindings":[["db","queue"]]"#, "$.bindings[0]"),
        (r#","contributes":[[],[],null,[],[]]"#, "$.contributes"),
        (r#","contributes":{"pipeline":[["audit","observe"]]}"#, "$.contributes.pipeline[0]"),
        (r#","contributes":{"config":["XiHan:A"]}"#, "$.contributes.config"),
    ] {
        assert_error(&manifest::read(minimal(member)).expect_err(member), MESSAGE_INVALID, &[("reason", path)], member);
    }
    let root = r#"["xihan.a.b","1.0.0","1","stable","capability",null,null,null,null,null,null,null,null,null,null,null]"#;
    assert_error(&manifest::read(root).unwrap_err(), MESSAGE_INVALID, &[("reason", "$")], "the array form of the root");
}

#[derive(Deserialize)]
struct ManifestVectors {
    valid: Vec<ManifestCase>,
    invalid: Vec<ManifestCase>,
}

#[derive(Deserialize)]
struct ManifestCase {
    name: String,
    /// 清单的 JSON 值，保留原文以保留成员在文档里的顺序；`null` 根也是一份原文。
    #[serde(default, deserialize_with = "raw")]
    manifest: Option<Box<RawValue>>,
    /// JSON 值表达不了的原文；空串是合法的原文，与没有给出不同。
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    written: Option<String>,
    #[serde(default)]
    error: Option<ManifestError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestError {
    code: String,
    message_key: String,
    arguments: BTreeMap<String, String>,
}

/// `manifest` 写成 `null` 也保留原文，不读成「没有」。
fn raw<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(deserializer).map(Some)
}

/// 去掉字符串以外的空白：向量里的清单按紧凑写法交给读取，成员顺序照写。
fn compact(text: &str) -> String {
    let (mut output, mut quoted, mut escaped) = (String::with_capacity(text.len()), false, false);
    for char in text.chars() {
        if quoted {
            output.push(char);
            match char {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
        } else if char == '"' {
            quoted = true;
            output.push(char);
        } else if !matches!(char, ' ' | '\t' | '\n' | '\r') {
            output.push(char);
        }
    }
    output
}

impl ManifestCase {
    /// 清单的原文：有 text 用 text，否则是 manifest 的紧凑文本。
    fn source(&self) -> String {
        match (&self.text, &self.manifest) {
            (Some(text), None) => text.clone(),
            (None, Some(manifest)) => compact(manifest.get()),
            _ => panic!("{} must give exactly one of manifest and text", self.name),
        }
    }
}

/// 清单的读取与写出按 manifests.json（RFC-0002 解析规则 2 与「写出」）：合法的读出后再写出与 written 逐字相等，
/// 不合法的错误码、消息键与参数逐字相等。
#[test]
fn manifests_follow_the_vectors() {
    let vectors: ManifestVectors = common::vector("manifest/manifests.json");
    let templates = common::kernel_messages();
    for case in &vectors.valid {
        let read = manifest::read(case.source()).unwrap_or_else(|error| panic!("{} must be read: {error}", case.name));
        let written = manifest::write(&read).unwrap();
        assert_eq!(Some(&written), case.written.as_ref(), "{}", case.name);
        assert_eq!(manifest::read(&written).unwrap(), read, "{} reads back", case.name);
    }
    for case in &vectors.invalid {
        let error = manifest::read(case.source()).expect_err(&case.name);
        let expected = case.error.as_ref().unwrap_or_else(|| panic!("{} has no expected error", case.name));
        assert_eq!(error.code(), expected.code, "{}", case.name);
        let arguments: Vec<(&str, &str)> = expected.arguments.iter().map(|(name, value)| (name.as_str(), value.as_str())).collect();
        assert_error(&error, &expected.message_key, &arguments, &case.name);
        // 消息是 kernel-messages.json 的 en-US 模板填入参数，与 .NET 相同（ADR-0236）
        let template = templates
            .iter()
            .find(|message| message.key == expected.message_key)
            .unwrap_or_else(|| panic!("{} is not in kernel-messages.json", expected.message_key));
        assert_eq!(template.code, expected.code, "{}", case.name);
        assert_eq!(error.message(), primitives::format_message(&template.template, error.details().unwrap()), "{}", case.name);
        assert_eq!(error.to_error().message(), error.message(), "{}", case.name);
    }
}
