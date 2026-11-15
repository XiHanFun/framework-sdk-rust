//! `xihan.manifest.json` 的模型（RFC-0002，Schema 见 `spec/manifest/manifest.schema.json`）。
//!
//! 只做数据与格式校验；收集、解析与求解是宿主的职责。[`read`] 解析并校验，[`write()`] 校验后输出规范化的 JSON
//! （两空格缩进，字段按 Schema 顺序，缺省值省略），二者对同一份清单往返不变。
//!
//! ```
//! use xihan_framework::manifest::{self, Isolation, Kind};
//!
//! let manifest = manifest::read(r#"{
//!   "id": "xihan.samples.echo", "version": "1.0.0", "primitives": "1", "stability": "experimental",
//!   "kind": "plugin", "isolation": "out-of-process", "requires": [ { "id": "kernel.plugins", "version": "^1" } ]
//! }"#).unwrap();
//! assert_eq!((manifest.kind, manifest.isolation), (Kind::Plugin, Some(Isolation::OutOfProcess)));
//! assert_eq!(manifest.requires[0].to_string(), "kernel.plugins@1");
//! ```

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::grammar::{all, digit, lower, lower_digit, lower_digit_hyphen, positive, upper, word};
use crate::primitives::{ERROR_CODE_INVALID_ARGUMENT, FrameworkError, PipelineStage, Stability};

/// 清单文件名。
pub const FILE_NAME: &str = "xihan.manifest.json";

/// 缺省的激活事件：宿主启动时激活。
pub const ACTIVATION_ON_STARTUP: &str = "onStartup";

/// 清单种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// 能力包。
    Capability,
    /// Provider。
    Provider,
    /// 插件。
    Plugin,
    /// 宿主。
    Host,
    /// 协议适配器。
    Protocol,
}

/// Provider 角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// 普通 Provider（缺省）。
    Provider,
    /// 没有显式 Provider 时被选中的实现。
    Default,
}

/// 插件隔离级别。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Isolation {
    /// 进程内。
    InProcess,
    /// Wasm 沙箱。
    Wasm,
    /// 进程外。
    OutOfProcess,
}

impl Kind {
    /// 清单里的名字。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capability => "capability",
            Self::Provider => "provider",
            Self::Plugin => "plugin",
            Self::Host => "host",
            Self::Protocol => "protocol",
        }
    }
}

impl Role {
    /// 清单里的名字。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Default => "default",
        }
    }
}

impl Isolation {
    /// 清单里的名字。
    pub const fn name(self) -> &'static str {
        match self {
            Self::InProcess => "in-process",
            Self::Wasm => "wasm",
            Self::OutOfProcess => "out-of-process",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

impl fmt::Display for Isolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// 判断清单 ID 是否符合 `xihan.<领域>.<名称>`：`^xihan\.[a-z][a-z0-9]*\.[a-z][a-z0-9-]*$`。
pub fn is_manifest_id(id: &str) -> bool {
    id.strip_prefix("xihan.").is_some_and(is_capability_id)
}

/// 判断能力 ID 是否符合 `<领域>.<能力>`：`^[a-z][a-z0-9]*\.[a-z][a-z0-9-]*$`。
pub fn is_capability_id(id: &str) -> bool {
    id.split_once('.').is_some_and(|(domain, capability)| word(domain, lower, lower_digit) && word(capability, lower, lower_digit_hyphen))
}

/// 返回清单 ID 或能力 ID 的领域段；都不合法时为 `None`。
pub fn domain(id: &str) -> Option<&str> {
    if let Some(rest) = id.strip_prefix("xihan.").filter(|rest| is_capability_id(rest)) {
        return rest.split('.').next();
    }
    is_capability_id(id).then(|| id.split('.').next()).flatten()
}

/// 提供的能力：`id@version`，接口版本是正整数。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CapabilityDeclaration {
    /// 能力 ID。
    pub id: String,
    /// 接口版本；线上编码为数字字符串（`"1"`）。
    #[serde(with = "version_text")]
    pub version: u32,
}

impl CapabilityDeclaration {
    /// 用能力 ID 与接口版本创建；格式由 [`Manifest::validate`] 校验。
    pub fn new(id: impl Into<String>, version: u32) -> Self {
        Self { id: id.into(), version }
    }

    /// 解析 `id@version`；不合文法时为 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        let (id, version) = text.split_once('@')?;
        let version = positive(version)?;
        is_capability_id(id).then(|| Self::new(id, version))
    }
}

impl FromStr for CapabilityDeclaration {
    type Err = FrameworkError;

    fn from_str(text: &str) -> Result<Self, FrameworkError> {
        Self::parse(text).ok_or_else(|| invalid_text(format!("The capability declaration '{text}' must match <capability>@<version>.")))
    }
}

impl fmt::Display for CapabilityDeclaration {
    /// `id@version`。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.id, self.version)
    }
}

/// 依赖的能力：`id@[^]version[?]`；`^N` 与 `N` 等价。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CapabilityRequirement {
    /// 能力 ID。
    pub id: String,
    /// 接口版本；读入接受 `"1"` 与 `"^1"`，写出为 `"1"`。
    #[serde(serialize_with = "version_text::serialize", deserialize_with = "version_text::deserialize_range")]
    pub version: u32,
    /// 是否可选；可选依赖缺失不阻止启动。
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
}

impl CapabilityRequirement {
    /// 用能力 ID 与接口版本创建必需的依赖；格式由 [`Manifest::validate`] 校验。
    pub fn new(id: impl Into<String>, version: u32) -> Self {
        Self { id: id.into(), version, optional: false }
    }

    /// 解析 `id@[^]version[?]`；不合文法时为 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        let (id, range) = text.split_once('@')?;
        let (range, optional) = match range.strip_suffix('?') {
            Some(range) => (range, true),
            None => (range, false),
        };
        let version = positive(range.strip_prefix('^').unwrap_or(range))?;
        is_capability_id(id).then(|| Self { id: id.to_owned(), version, optional })
    }

    /// 判断一个声明是否满足本依赖：同一能力且同一接口版本。
    pub fn is_satisfied_by(&self, declaration: &CapabilityDeclaration) -> bool {
        self.id == declaration.id && self.version == declaration.version
    }
}

impl FromStr for CapabilityRequirement {
    type Err = FrameworkError;

    fn from_str(text: &str) -> Result<Self, FrameworkError> {
        Self::parse(text).ok_or_else(|| invalid_text(format!("The capability requirement '{text}' must match <capability>@[^]<version>[?].")))
    }
}

impl fmt::Display for CapabilityRequirement {
    /// `id@version[?]`，不带 `^`。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}{}", self.id, self.version, if self.optional { "?" } else { "" })
    }
}

/// 需要宿主注入的资源。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Binding {
    /// 绑定名 `^[a-z][a-z0-9-]*$`。
    pub name: String,
    /// 资源类型 `^[a-z]+(:[a-z]+)?$`，如 `connection:relational`、`cache:distributed`、`queue`。
    #[serde(rename = "type")]
    pub resource_type: String,
    /// 是否必需；`None` 即缺省的必需。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

impl Binding {
    /// 用绑定名与资源类型创建。
    pub fn new(name: impl Into<String>, resource_type: impl Into<String>) -> Self {
        Self { name: name.into(), resource_type: resource_type.into(), required: None }
    }

    /// 判断绑定是否必需；缺省为必需。
    pub fn is_required(&self) -> bool {
        self.required.unwrap_or(true)
    }
}

/// 贡献的一个中间件（RFC-0004）。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct PipelineContribution {
    /// 中间件名。
    pub middleware: String,
    /// 所在阶段。
    pub stage: PipelineStage,
    /// 要排在其前的中间件名。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    /// 要排在其后的中间件名。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

impl PipelineContribution {
    /// 用中间件名与阶段创建。
    pub fn new(middleware: impl Into<String>, stage: PipelineStage) -> Self {
        Self { middleware: middleware.into(), stage, before: Vec::new(), after: Vec::new() }
    }
}

/// 贡献的配置节：领域节 `XiHan:<领域>`，可再带一层或多层 PascalCase 子节（如 `XiHan:Caching:Redis`，RFC-0002「配置节」）。
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ConfigContribution {
    /// 配置节。
    pub section: String,
    /// 配置节的 JSON Schema。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

impl ConfigContribution {
    /// 用配置节创建。
    pub fn new(section: impl Into<String>) -> Self {
        Self { section: section.into(), schema: None }
    }
}

/// 贡献点。
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Contributions {
    /// 管道中间件；插件里只有 Wasm 插件能贡献。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pipeline: Vec<PipelineContribution>,
    /// 操作 ID。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<String>,
    /// 配置节。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigContribution>,
    /// 此包定义的权限码（RFC-0008 文法：小写、点分、至少两段，如 `orders.read`），不重复；
    /// 插件的由宿主在登记它的代理操作时一并登记（`plugin/invocation.md` §1）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
    /// 健康项。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub health: Vec<String>,
}

impl Contributions {
    /// 创建空的贡献点。
    pub fn new() -> Self {
        Self::default()
    }
}

/// 一份清单；字段顺序与 Schema 和 Go SDK 一致，也是 [`write()`] 的输出顺序。
///
/// 构造只收必需字段（[`Manifest::new`]），可选字段直接赋值；`#[non_exhaustive]` 让以后只加可选字段而不破坏调用方。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Manifest {
    /// `$schema`。
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// 清单 ID `xihan.<领域>.<名称>`。
    pub id: String,
    /// 所属包 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// 语义化版本。
    pub version: String,
    /// 依赖的原语层大版本；线上编码为数字字符串。
    #[serde(with = "version_text")]
    pub primitives: u32,
    /// 稳定性。
    pub stability: Stability,
    /// 种类。
    pub kind: Kind,
    /// Provider 角色；`None` 即缺省的 `provider`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    /// 插件隔离级别；插件必须声明，非插件不得声明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<Isolation>,
    /// 提供的能力。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<CapabilityDeclaration>,
    /// 依赖的能力。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<CapabilityRequirement>,
    /// 需要宿主注入的资源。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,
    /// 贡献点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contributes: Option<Contributions>,
    /// 激活事件；空即缺省的 `onStartup`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub activation: Vec<String>,
    /// 插件所需权限，宿主配置授予。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
    /// 入口：插件可执行文件名、类型名或等价标识。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

impl Manifest {
    /// 用必需字段创建清单；格式由 [`Manifest::validate`] 校验。
    pub fn new(id: impl Into<String>, version: impl Into<String>, primitives: u32, stability: Stability, kind: Kind) -> Self {
        Self {
            schema: None,
            id: id.into(),
            package: None,
            version: version.into(),
            primitives,
            stability,
            kind,
            role: None,
            isolation: None,
            provides: Vec::new(),
            requires: Vec::new(),
            bindings: Vec::new(),
            contributes: None,
            activation: Vec::new(),
            permissions: Vec::new(),
            entry: None,
        }
    }

    /// 角色，缺省为 [`Role::Provider`]。
    pub fn effective_role(&self) -> Role {
        self.role.unwrap_or(Role::Provider)
    }

    /// 激活事件，缺省为 `onStartup`。
    pub fn effective_activation(&self) -> Vec<&str> {
        if self.activation.is_empty() { vec![ACTIVATION_ON_STARTUP] } else { self.activation.iter().map(String::as_str).collect() }
    }

    /// 判断是否在宿主启动时激活。
    pub fn activates_on_startup(&self) -> bool {
        self.effective_activation().contains(&ACTIVATION_ON_STARTUP)
    }

    /// 贡献的操作 ID；没有贡献时为空。
    pub fn operations(&self) -> &[String] {
        self.contributes.as_ref().map_or(&[], |contributes| &contributes.operations)
    }

    /// 贡献的健康项；没有贡献时为空。
    pub fn health(&self) -> &[String] {
        self.contributes.as_ref().map_or(&[], |contributes| &contributes.health)
    }

    /// 贡献的配置节；没有时为 `None`。
    pub fn config_section(&self) -> Option<&str> {
        self.contributes.as_ref()?.config.as_ref().map(|config| config.section.as_str())
    }

    /// 校验格式与跨字段约束；失败为 `PRIMITIVES_VALIDATION_001`。
    ///
    /// 插件必须声明隔离，非插件不得声明隔离或权限，只有 Wasm 插件可以贡献中间件，提供、依赖与绑定不得重复。
    pub fn validate(&self) -> Result<(), FrameworkError> {
        let id = self.id.as_str();
        if !is_manifest_id(id) {
            return invalid(format!("the manifest ID '{id}' must match xihan.<domain>.<name>"));
        }
        if !is_semver(&self.version) {
            return invalid(format!("the version '{}' of '{id}' must be a semantic version", self.version));
        }
        if self.primitives < 1 {
            return invalid(format!("the primitives version of '{id}' must be a positive integer"));
        }
        match (self.kind, self.isolation) {
            (Kind::Plugin, None) => return invalid(format!("the plugin '{id}' must declare its isolation")),
            (kind, Some(_)) if kind != Kind::Plugin => return invalid(format!("'{id}' is not a plugin and must not declare an isolation")),
            _ => {}
        }
        if self.kind != Kind::Plugin && !self.permissions.is_empty() {
            return invalid(format!("'{id}' is not a plugin and must not declare permissions"));
        }

        // 去重集合只活在一次校验里，大小以清单本身为上限
        let mut seen = HashSet::new();
        for declaration in &self.provides {
            if !is_capability_id(&declaration.id) || declaration.version < 1 {
                return invalid(format!("the provided capability '{}' of '{id}' is not valid", declaration.id));
            }
            if !seen.insert(declaration.id.as_str()) {
                return invalid(format!("'{id}' provides '{}' more than once", declaration.id));
            }
        }
        seen.clear();
        for requirement in &self.requires {
            if !is_capability_id(&requirement.id) || requirement.version < 1 {
                return invalid(format!("the required capability '{}' of '{id}' is not valid", requirement.id));
            }
            if !seen.insert(requirement.id.as_str()) {
                return invalid(format!("'{id}' requires '{}' more than once", requirement.id));
            }
        }
        seen.clear();
        for binding in &self.bindings {
            if !is_binding_name(&binding.name) || !is_binding_type(&binding.resource_type) {
                return invalid(format!("the binding '{}' ({}) of '{id}' is not valid", binding.name, binding.resource_type));
            }
            if !seen.insert(binding.name.as_str()) {
                return invalid(format!("'{id}' declares the binding '{}' more than once", binding.name));
            }
        }
        if let Some(event) = self.activation.iter().find(|event| !is_activation_event(event)) {
            return invalid(format!("the activation event '{event}' of '{id}' is not valid"));
        }
        if let Some(permission) = self.permissions.iter().find(|permission| !is_permission(permission)) {
            return invalid(format!("the permission '{permission}' of '{id}' is not valid"));
        }
        if let Some(contributes) = &self.contributes {
            contributes.validate(id)?;
            // 只有 Wasm 插件能贡献中间件（RFC-0017「范围」）：进程外与进程内插件的清单写了 contributes.pipeline，
            // .NET 宿主在发现或加载时以 PLUGINS_VALIDATION_001 拒绝；读清单时同样拒绝，不让作者以为中间件生效了、等到宿主那里才失败
            if let (Kind::Plugin, Some(isolation)) = (self.kind, self.isolation) {
                if isolation != Isolation::Wasm && !contributes.pipeline.is_empty() {
                    let names: Vec<&str> = contributes.pipeline.iter().map(|contribution| contribution.middleware.as_str()).collect();
                    return invalid(format!(
                        "the plugin '{id}' with isolation '{isolation}' must not declare 'contributes.pipeline' ({}): only wasm plugins can contribute middleware",
                        names.join(", ")
                    ));
                }
            }
        }
        Ok(())
    }
}

impl Contributions {
    /// 校验贡献点；`id` 是所在清单的 ID。
    fn validate(&self, id: &str) -> Result<(), FrameworkError> {
        if self.pipeline.iter().any(|contribution| contribution.middleware.is_empty()) {
            return invalid(format!("a pipeline contribution of '{id}' has no middleware name"));
        }
        if let Some(operation) = self.operations.iter().find(|operation| !is_manifest_operation(operation)) {
            return invalid(format!("the operation '{operation}' of '{id}' must match <resource>.<verb>"));
        }
        if let Some(config) = self.config.as_ref().filter(|config| !is_config_section(&config.section)) {
            return invalid(format!(
                "the configuration section '{}' of '{id}' must match XiHan:<Domain> or XiHan:<Domain>:<Section>..., with PascalCase segments",
                config.section
            ));
        }
        // 插件定义的权限码由宿主在登记代理操作时一并登记（plugin/invocation.md §1）：不合文法或重复的码，
        // 宿主在发现时拒绝整个插件；读清单时同样拒绝，不让它等到宿主那里才失败
        let mut seen = HashSet::new();
        for code in &self.permissions {
            if !is_permission_code(code) {
                return invalid(format!(
                    "the permission code '{code}' in 'contributes.permissions' of '{id}' must be a lowercase dot-separated code with at least two segments, such as 'orders.read'"
                ));
            }
            if !seen.insert(code.as_str()) {
                return invalid(format!("the permission code '{code}' appears more than once in 'contributes.permissions' of '{id}'"));
            }
        }
        if let Some(health) = self.health.iter().find(|health| !word(health, lower, |byte| lower_digit_hyphen(byte) || byte == b'.')) {
            return invalid(format!("the health item '{health}' of '{id}' is not valid"));
        }
        Ok(())
    }
}

/// 解析并校验清单 JSON；未知字段、格式与跨字段约束都算失败（`PRIMITIVES_VALIDATION_001`）。
pub fn read(data: impl AsRef<[u8]>) -> Result<Manifest, FrameworkError> {
    let manifest: Manifest = serde_json::from_slice(data.as_ref())
        .map_err(|error| FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, "The manifest is not valid JSON for the schema.").with_cause(error))?;
    manifest.validate()?;
    Ok(manifest)
}

/// 读取并校验清单文件。
pub fn read_file(path: impl AsRef<Path>) -> Result<Manifest, FrameworkError> {
    let data =
        std::fs::read(path).map_err(|error| FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, "The manifest file could not be read.").with_cause(error))?;
    read(data)
}

/// 校验后输出规范化的清单 JSON：两空格缩进，字段按 Schema 顺序，缺省值省略，末尾没有换行。
pub fn write(manifest: &Manifest) -> Result<String, FrameworkError> {
    manifest.validate()?;
    serde_json::to_string_pretty(manifest)
        .map_err(|error| FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, "The manifest could not be encoded.").with_cause(error))
}

/// 清单校验失败的配置错误：消息补上句号。
fn invalid(message: String) -> Result<(), FrameworkError> {
    Err(FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, message + "."))
}

fn invalid_text(message: String) -> FrameworkError {
    FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, message)
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 语义化版本 `^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$`（与 Schema 一致，允许前导零）。
fn is_semver(version: &str) -> bool {
    let identifier = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-';
    // 预发布与构建段都不含 '+'，第一个 '+' 之后是构建段
    let (rest, build) = match version.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (version, None),
    };
    // 核心三段不含 '-'，第一个 '-' 之后是预发布段
    let (core, pre_release) = match rest.split_once('-') {
        Some((core, pre_release)) => (core, Some(pre_release)),
        None => (rest, None),
    };
    let mut numbers = core.split('.');
    let core_valid = (0..3).all(|_| numbers.next().is_some_and(|number| all(number, digit))) && numbers.next().is_none();
    core_valid && pre_release.is_none_or(|text| all(text, identifier)) && build.is_none_or(|text| all(text, identifier))
}

/// 激活事件 `^(onStartup|onOperation:[a-z0-9.*-]+|onEvent:[a-z0-9.*-]+)$`。
fn is_activation_event(event: &str) -> bool {
    let pattern = |text: &str| all(text, |byte| lower_digit(byte) || matches!(byte, b'.' | b'*' | b'-'));
    event == ACTIVATION_ON_STARTUP || event.strip_prefix("onOperation:").is_some_and(pattern) || event.strip_prefix("onEvent:").is_some_and(pattern)
}

/// 插件权限 `^(net\.outbound|fs\.read|fs\.write|capability):.+$`；`.` 不匹配换行。
fn is_permission(permission: &str) -> bool {
    permission
        .split_once(':')
        .is_some_and(|(scope, target)| matches!(scope, "net.outbound" | "fs.read" | "fs.write" | "capability") && !target.is_empty() && !target.contains('\n'))
}

/// 绑定名 `^[a-z][a-z0-9-]*$`。
fn is_binding_name(name: &str) -> bool {
    word(name, lower, lower_digit_hyphen)
}

/// 资源类型 `^[a-z]+(:[a-z]+)?$`。
fn is_binding_type(resource_type: &str) -> bool {
    match resource_type.split_once(':') {
        Some((kind, detail)) => all(kind, lower) && all(detail, lower),
        None => all(resource_type, lower),
    }
}

/// 清单里的操作 ID `^[a-z][a-z0-9-]*\.[a-z][a-z0-9-]*$`（比操作契约的文法宽，与 Schema 一致）。
fn is_manifest_operation(operation: &str) -> bool {
    let mut parts = operation.split('.');
    let segment = |part: Option<&str>| part.is_some_and(|part| word(part, lower, lower_digit_hyphen));
    segment(parts.next()) && segment(parts.next()) && parts.next().is_none()
}

/// 配置节 `^XiHan:[A-Z][A-Za-z0-9]*(:[A-Z][A-Za-z0-9]*)*$`。
fn is_config_section(section: &str) -> bool {
    section.strip_prefix("XiHan:").is_some_and(|rest| rest.split(':').all(|segment| word(segment, upper, |byte| byte.is_ascii_alphanumeric())))
}

/// 权限码 `^[a-z][a-z0-9-]*(\.[a-z][a-z0-9-]*)+$`（RFC-0008）。
fn is_permission_code(code: &str) -> bool {
    let mut parts = code.split('.');
    let segment = |part: &str| word(part, lower, lower_digit_hyphen);
    parts.next().is_some_and(segment) && {
        let mut rest = parts.peekable();
        rest.peek().is_some() && rest.all(segment)
    }
}

/// 接口版本的线上编码：数字字符串。
mod version_text {
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::grammar::positive;

    /// 写出为数字字符串。
    pub(super) fn serialize<S: Serializer>(version: &u32, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(version)
    }

    /// 读入 `^[1-9][0-9]*$`（`primitives` 与 `provides[].version`）。
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
        let text = String::deserialize(deserializer)?;
        positive(&text).ok_or_else(|| D::Error::custom(format!("the interface version '{text}' must be a positive integer")))
    }

    /// 读入 `^(\^)?[1-9][0-9]*$`（`requires[].version`）；`^N` 与 `N` 等价。
    pub(super) fn deserialize_range<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
        let text = String::deserialize(deserializer)?;
        positive(text.strip_prefix('^').unwrap_or(&text))
            .ok_or_else(|| D::Error::custom(format!("the interface version '{text}' must be a positive integer, optionally prefixed with ^")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammars_match_the_schema() {
        for version in ["1.0.0", "5.0.0-preview.1", "1.2.3+build.7", "1.2.3-rc-1+a.b", "01.2.3"] {
            assert!(is_semver(version), "{version}");
        }
        for version in ["", "1.0", "1.0.0.0", "1.0.0-", "1.0.0+", "v1.0.0", "1.0.0-é", "1..0"] {
            assert!(!is_semver(version), "{version}");
        }
        for event in ["onStartup", "onOperation:orders.*", "onEvent:a-b.c"] {
            assert!(is_activation_event(event), "{event}");
        }
        for event in ["", "onBoot", "onOperation:", "onEvent:Orders", "onStartup:x"] {
            assert!(!is_activation_event(event), "{event}");
        }
        for permission in ["net.outbound:api.example.com", "fs.read:/tmp", "capability:kernel.pipeline", "fs.write::"] {
            assert!(is_permission(permission), "{permission}");
        }
        for permission in ["sudo", "capability:", "net.inbound:x", "fs.read:a\nb"] {
            assert!(!is_permission(permission), "{permission}");
        }
        for resource_type in ["queue", "connection:relational"] {
            assert!(is_binding_type(resource_type), "{resource_type}");
        }
        for resource_type in ["", "a:", ":a", "a:b:c", "Queue", "cache-x"] {
            assert!(!is_binding_type(resource_type), "{resource_type}");
        }
        assert!(is_manifest_operation("orders-x.create-1") && !is_manifest_operation("orders") && !is_manifest_operation("a.b.c"));
        assert_eq!(domain("xihan.samples.echo"), Some("samples"));
        assert_eq!(domain("caching.distributed"), Some("caching"));
        assert_eq!(domain("Caching"), None);
    }
}
