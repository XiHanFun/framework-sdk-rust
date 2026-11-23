//! `xihan.manifest.json` 的模型（RFC-0002，Schema 见 `spec/manifest/manifest.schema.json`）。
//!
//! 只做数据与格式校验；收集、解析与求解是宿主的职责。[`read`] 按 RFC-0002 解析规则 2 的顺序读取并校验，错误都是
//! [`ERROR_CODE_INVALID_MANIFEST`] 加消息键与路径；[`write()`] 校验后按规范的排版写出（两空格缩进、LF、成员顺序固定、列表总写），
//! 各语言逐字相同，再读得到同一份清单。
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

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

mod shape;

use crate::grammar::positive;
use crate::primitives::json::{self, Node};
use crate::primitives::{Details, ERROR_CODE_INVALID_ARGUMENT, FrameworkError, PipelineStage, Stability, named_enum};

/// 清单文件名。
pub const FILE_NAME: &str = "xihan.manifest.json";

/// 缺省的激活事件：宿主启动时激活。
pub const ACTIVATION_ON_STARTUP: &str = "onStartup";

/// 写出的 `$schema`。
pub const SCHEMA_URI: &str = "https://spec.xihanfun.com/manifest/v1.json";

/// 读取清单失败的错误码（RFC-0002 解析规则 2）：不是 JSON、为空、不合 Schema、跨字段约束与文件不存在都用它。
pub const ERROR_CODE_INVALID_MANIFEST: &str = "KERNEL_CAPABILITY_006";

/// 不是 JSON，或有重复成员；参数 `reason`：`$` 或第一处重复的路径。
pub const MESSAGE_INVALID_JSON: &str = "kernel.manifest.invalid_json";

/// 根是字面量 `null`；没有参数。
pub const MESSAGE_EMPTY: &str = "kernel.manifest.empty";

/// 不合 Schema；参数 `reason`：第一处问题的路径。
pub const MESSAGE_INVALID: &str = "kernel.manifest.invalid";

/// 清单文件不存在；参数 `path`。
pub const MESSAGE_FILE_MISSING: &str = "kernel.manifest.file_missing";

/// 插件没有声明隔离级别；参数 `id`。
pub const MESSAGE_ISOLATION_REQUIRED: &str = "primitives.manifest.isolation_required";

/// 非插件声明了隔离级别或权限；参数 `id`。
pub const MESSAGE_ISOLATION_NOT_ALLOWED: &str = "primitives.manifest.isolation_not_allowed";

/// 提供的能力 ID 重复；参数 `id`。
pub const MESSAGE_DUPLICATE_PROVIDES: &str = "primitives.manifest.duplicate_provides";

/// 依赖的能力 ID 重复；参数 `id`。
pub const MESSAGE_DUPLICATE_REQUIRES: &str = "primitives.manifest.duplicate_requires";

/// 绑定名重复；参数 `id`。
pub const MESSAGE_DUPLICATE_BINDINGS: &str = "primitives.manifest.duplicate_bindings";

named_enum! {
    /// 清单种类；名字是清单里的小写名。
    ///
    /// 规范以后可以增加种类，所以是 `#[non_exhaustive]`：本 crate 之外的 `match` 要带通配分支。
    #[non_exhaustive]
    pub enum Kind ("manifest kind") {
        /// 能力包。
        Capability = 0 => "capability",
        /// Provider。
        Provider = 1 => "provider",
        /// 插件。
        Plugin = 2 => "plugin",
        /// 宿主。
        Host = 3 => "host",
        /// 协议适配器。
        Protocol = 4 => "protocol",
    }
}

named_enum! {
    /// Provider 角色；名字是清单里的小写名。
    pub enum Role ("provider role") {
        /// 普通 Provider（缺省）。
        Provider = 0 => "provider",
        /// 没有显式 Provider 时被选中的实现。
        Default = 1 => "default",
    }
}

named_enum! {
    /// 插件隔离级别；名字是清单里的小写名。
    pub enum Isolation ("plugin isolation") {
        /// 进程内。
        InProcess = 0 => "in-process",
        /// Wasm 沙箱。
        Wasm = 1 => "wasm",
        /// 进程外。
        OutOfProcess = 2 => "out-of-process",
    }
}

impl Default for Role {
    /// 缺省的 `provider`。
    fn default() -> Self {
        Self::Provider
    }
}

/// 判断清单 ID 是否符合 `xihan.<领域>.<名称>`：`^xihan\.[a-z][a-z0-9]*\.[a-z][a-z0-9-]*$`。
pub fn is_manifest_id(id: &str) -> bool {
    shape::is_manifest_id(id)
}

/// 判断能力 ID 是否符合 `<领域>.<能力>`：`^[a-z][a-z0-9]*\.[a-z][a-z0-9-]*$`。
pub fn is_capability_id(id: &str) -> bool {
    shape::is_capability_id(id)
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
    /// 是否必需；没写时读成 `true`（与 .NET 的 `Binding.Required` 一样填上缺省值）。
    #[serde(default = "required_by_default")]
    pub required: bool,
}

impl Binding {
    /// 用绑定名与资源类型创建，缺省为必需。
    pub fn new(name: impl Into<String>, resource_type: impl Into<String>) -> Self {
        Self { name: name.into(), resource_type: resource_type.into(), required: true }
    }

    /// 判断绑定是否必需。
    pub fn is_required(&self) -> bool {
        self.required
    }
}

fn required_by_default() -> bool {
    true
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
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
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
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
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

/// 一份清单；字段按 RFC-0002「写出」的成员顺序排列，也是 [`write()`] 的输出顺序。
///
/// 构造只收必需字段（[`Manifest::new`]），可选字段直接赋值；`#[non_exhaustive]` 让以后只加可选字段而不破坏调用方。
/// 与 .NET 的类型化模型一样，读出时填上缺省值：`role` 为 `provider`，`activation` 为 `["onStartup"]`，绑定的 `required` 为
/// `true`，`contributes` 为空的贡献点；`$schema` 不保留，写出时总是 [`SCHEMA_URI`]。所以写出的清单再读回来与原来的相等。
///
/// 按 Schema 的严格读取是 [`read`]；这里的 `Deserialize` 是给嵌在别的配置里用的，根上的未知成员（含 `$schema`）忽略。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Manifest {
    /// 清单 ID `xihan.<领域>.<名称>`。
    pub id: String,
    /// 所属包 ID。
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
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
    /// Provider 角色；没写时为 [`Role::Provider`]。
    #[serde(default)]
    pub role: Role,
    /// 插件隔离级别；插件必须声明，非插件不得声明。
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
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
    /// 贡献点；没写时为空。
    #[serde(default)]
    pub contributes: Contributions,
    /// 激活事件；没写时为 `["onStartup"]`，写了就至少一项，空数组不合法。
    #[serde(default = "default_activation", deserialize_with = "non_empty")]
    pub activation: Vec<String>,
    /// 插件所需权限，宿主配置授予。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<String>,
    /// 入口：插件可执行文件名、类型名或等价标识。
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
}

impl Manifest {
    /// 用必需字段创建清单；格式由 [`Manifest::validate`] 校验。
    pub fn new(id: impl Into<String>, version: impl Into<String>, primitives: u32, stability: Stability, kind: Kind) -> Self {
        Self {
            id: id.into(),
            package: None,
            version: version.into(),
            primitives,
            stability,
            kind,
            role: Role::Provider,
            isolation: None,
            provides: Vec::new(),
            requires: Vec::new(),
            bindings: Vec::new(),
            contributes: Contributions::default(),
            activation: default_activation(),
            permissions: Vec::new(),
            entry: None,
        }
    }

    /// 角色，同 `role`。
    pub fn effective_role(&self) -> Role {
        self.role
    }

    /// 激活事件；程序里清空了的按缺省的 `onStartup`。
    pub fn effective_activation(&self) -> Vec<&str> {
        if self.activation.is_empty() { vec![ACTIVATION_ON_STARTUP] } else { self.activation.iter().map(String::as_str).collect() }
    }

    /// 判断是否在宿主启动时激活。
    pub fn activates_on_startup(&self) -> bool {
        self.effective_activation().contains(&ACTIVATION_ON_STARTUP)
    }

    /// 贡献的操作 ID。
    pub fn operations(&self) -> &[String] {
        &self.contributes.operations
    }

    /// 贡献的健康项。
    pub fn health(&self) -> &[String] {
        &self.contributes.health
    }

    /// 贡献的配置节；没有时为 `None`。
    pub fn config_section(&self) -> Option<&str> {
        self.contributes.config.as_ref().map(|config| config.section.as_str())
    }

    /// 校验字段的格式与跨字段约束；失败为 `PRIMITIVES_VALIDATION_001`（与 .NET 的 `Manifest.Validate` 同码）。
    ///
    /// 字段的格式按 [`read`] 第 3 步的同一套 Schema 核对，说明里带出第一处问题的路径，如 `$.provides[0].version`。跨字段约束按这个
    /// 顺序（RFC-0002 解析规则 2 第 4 步）：插件必须声明隔离（[`MESSAGE_ISOLATION_REQUIRED`]），非插件不得声明隔离或权限
    /// （[`MESSAGE_ISOLATION_NOT_ALLOWED`]），提供、依赖的能力 ID 与绑定名不得重复（[`MESSAGE_DUPLICATE_PROVIDES`]、
    /// [`MESSAGE_DUPLICATE_REQUIRES`]、[`MESSAGE_DUPLICATE_BINDINGS`]），带消息键，参数只有 `id`。
    pub fn validate(&self) -> Result<(), FrameworkError> {
        // 写出的文本总是 JSON；再按读取的同一套 Schema 核对，字段的格式与读取报得一样
        let node = json::parse(render(self).as_bytes())
            .map_err(|error| FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, format!("The manifest could not be checked: {error}.")))?;
        if let Err(path) = shape::check(&node) {
            return Err(FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, format!("The manifest '{}' is not valid at '{path}'.", self.id)));
        }
        match self.constraint() {
            Some(key) => Err(FrameworkError::with_message(ERROR_CODE_INVALID_ARGUMENT, key, id_argument(&self.id))),
            None => Ok(()),
        }
    }

    /// 第一个不满足的跨字段约束的消息键。
    fn constraint(&self) -> Option<&'static str> {
        if self.kind == Kind::Plugin && self.isolation.is_none() {
            return Some(MESSAGE_ISOLATION_REQUIRED);
        }
        if self.kind != Kind::Plugin && (self.isolation.is_some() || !self.permissions.is_empty()) {
            return Some(MESSAGE_ISOLATION_NOT_ALLOWED);
        }
        if has_duplicates(self.provides.iter().map(|declaration| declaration.id.as_str())) {
            return Some(MESSAGE_DUPLICATE_PROVIDES);
        }
        if has_duplicates(self.requires.iter().map(|requirement| requirement.id.as_str())) {
            return Some(MESSAGE_DUPLICATE_REQUIRES);
        }
        if has_duplicates(self.bindings.iter().map(|binding| binding.name.as_str())) {
            return Some(MESSAGE_DUPLICATE_BINDINGS);
        }
        None
    }
}

/// 去重集合只活在一次校验里，大小以清单本身为上限。
fn has_duplicates<'a>(items: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = HashSet::new();
    items.into_iter().any(|item| !seen.insert(item))
}

/// 跨字段约束的参数：清单 ID。
fn id_argument(id: &str) -> Details {
    Details::from([("id".to_owned(), Value::from(id))])
}

/// 读取清单失败：[`ERROR_CODE_INVALID_MANIFEST`] 加消息键与一个参数。
fn reading_error(key: &str, name: &str, value: &str) -> FrameworkError {
    FrameworkError::with_message(ERROR_CODE_INVALID_MANIFEST, key, Details::from([(name.to_owned(), Value::from(value))]))
}

/// 从 JSON 文本读取清单（RFC-0002 解析规则 2）：按下面的顺序逐项核对，报第一处问题，错误码都是 [`ERROR_CODE_INVALID_MANIFEST`]。
///
/// 1. JSON 文本：过共同的读取检查（[`json::check`]）。不是 JSON 时为 [`MESSAGE_INVALID_JSON`]，`reason` 为 `$`；任何一层有重复成员
///    时同一个键，`reason` 为第一处重复的路径。文本按原样检查，开头的 BOM 是语法错误（[`read_file`] 会跳过它）。
/// 2. 根是字面量 `null`：[`MESSAGE_EMPTY`]，没有参数。
/// 3. 形状：按 Schema 逐项核对，第一处问题为 [`MESSAGE_INVALID`]，`reason` 是该处的路径；根不是对象时为 `$`。成员按在文本里出现的
///    顺序，每个成员依次查 Schema 之外的成员（成员名区分大小写）→ `null` → 类型 → 取值，对象与数组进到里面再查；对象的成员查完后
///    按 Schema 里 `required` 的顺序报缺少的成员。接口版本是不带前导零的正整数、不超过 2147483647，只有 `requires[].version` 可以带
///    一个 `^`；`activation` 至少一项；`contributes.permissions` 不得重复，路径是第二次出现的项。
/// 4. 跨字段约束：同 [`Manifest::validate`] 的顺序与消息键，参数只有 `id`。
///
/// 只有 Wasm 插件能贡献中间件是宿主在发现插件时的核对（`PLUGINS_VALIDATION_001`），读取不管。
pub fn read(data: impl AsRef<[u8]>) -> Result<Manifest, FrameworkError> {
    let node = json::parse(data.as_ref()).map_err(|error| reading_error(MESSAGE_INVALID_JSON, "reason", error.path()))?;
    if node == Node::Null {
        return Err(FrameworkError::with_message(ERROR_CODE_INVALID_MANIFEST, MESSAGE_EMPTY, Details::new()));
    }
    shape::check(&node).map_err(|path| reading_error(MESSAGE_INVALID, "reason", &path))?;
    // 形状核对过，按类型读入不会再失败；万一失败也按同一个键报出位置
    let manifest: Manifest = json::decode(&node).map_err(|error| reading_error(MESSAGE_INVALID, "reason", error.path()))?;
    match manifest.constraint() {
        Some(key) => Err(FrameworkError::with_message(ERROR_CODE_INVALID_MANIFEST, key, id_argument(&manifest.id))),
        None => Ok(manifest),
    }
}

/// 读取清单文件：按 UTF-8 读、跳过开头的 UTF-8 BOM，其余同 [`read`]；文件不存在（或不是文件）为 [`MESSAGE_FILE_MISSING`]，参数 `path`。
pub fn read_file(path: impl AsRef<Path>) -> Result<Manifest, FrameworkError> {
    let path = path.as_ref();
    if !path.is_file() {
        return Err(reading_error(MESSAGE_FILE_MISSING, "path", &path.display().to_string()));
    }
    let data = std::fs::read(path).map_err(|error| {
        FrameworkError::new(ERROR_CODE_INVALID_MANIFEST, format!("The manifest file '{}' could not be read.", path.display())).with_cause(error)
    })?;
    read(data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&data))
}

/// 校验（[`Manifest::validate`]）后按规范的排版写出（RFC-0002「写出」），各语言逐字相同。
///
/// 两个空格缩进，换行只用 LF，末尾不换行，成员名与值之间是 `: `；空数组写 `[]`，非空数组每项一行。成员顺序固定：`$schema`
/// （总写 [`SCHEMA_URI`]）、`id`、`package`、`version`、`primitives`、`stability`、`kind`、`role`（缺省写 `provider`）、`isolation`、
/// `provides`、`requires`（每项都写 `optional`）、`bindings`（每项都写 `required`）、`contributes`（`pipeline`、`operations`、
/// `config`、`permissions`、`health`）、`activation`（缺省写 `["onStartup"]`）、`permissions`、`entry`；没有值的可选成员不写，
/// 列表总写。接口版本写成不带 `^` 的十进制文本，字符串只转义 `"`、`\` 与控制字符。
pub fn write(manifest: &Manifest) -> Result<String, FrameworkError> {
    manifest.validate()?;
    Ok(render(manifest))
}

/// 写出时的值：借用清单里的文本。
enum Out<'a> {
    Text(&'a str),
    Number(u32),
    Bool(bool),
    List(Vec<Out<'a>>),
    Object(Vec<(&'static str, Out<'a>)>),
}

fn texts(items: &[String]) -> Out<'_> {
    Out::List(items.iter().map(|item| Out::Text(item)).collect())
}

/// 按规范的排版写出，不校验。
fn render(manifest: &Manifest) -> String {
    let contributes = &manifest.contributes;
    let mut root = vec![("$schema", Out::Text(SCHEMA_URI)), ("id", Out::Text(&manifest.id))];
    if let Some(package) = &manifest.package {
        root.push(("package", Out::Text(package)));
    }
    root.extend([
        ("version", Out::Text(&manifest.version)),
        ("primitives", Out::Number(manifest.primitives)),
        ("stability", Out::Text(manifest.stability.name())),
        ("kind", Out::Text(manifest.kind.name())),
        ("role", Out::Text(manifest.role.name())),
    ]);
    if let Some(isolation) = manifest.isolation {
        root.push(("isolation", Out::Text(isolation.name())));
    }
    let provides = manifest.provides.iter().map(|item| Out::Object(vec![("id", Out::Text(&item.id)), ("version", Out::Number(item.version))]));
    let requires = manifest
        .requires
        .iter()
        .map(|item| Out::Object(vec![("id", Out::Text(&item.id)), ("version", Out::Number(item.version)), ("optional", Out::Bool(item.optional))]));
    let bindings = manifest
        .bindings
        .iter()
        .map(|item| Out::Object(vec![("name", Out::Text(&item.name)), ("type", Out::Text(&item.resource_type)), ("required", Out::Bool(item.is_required()))]));
    let pipeline = contributes.pipeline.iter().map(|item| {
        Out::Object(vec![
            ("middleware", Out::Text(&item.middleware)),
            ("stage", Out::Text(item.stage.manifest_name())),
            ("before", texts(&item.before)),
            ("after", texts(&item.after)),
        ])
    });
    let mut contributions = vec![("pipeline", Out::List(pipeline.collect())), ("operations", texts(&contributes.operations))];
    if let Some(config) = &contributes.config {
        let mut members = vec![("section", Out::Text(&config.section))];
        if let Some(schema) = &config.schema {
            members.push(("schema", Out::Text(schema)));
        }
        contributions.push(("config", Out::Object(members)));
    }
    contributions.extend([("permissions", texts(&contributes.permissions)), ("health", texts(&contributes.health))]);
    root.extend([
        ("provides", Out::List(provides.collect())),
        ("requires", Out::List(requires.collect())),
        ("bindings", Out::List(bindings.collect())),
        ("contributes", Out::Object(contributions)),
        ("activation", Out::List(manifest.effective_activation().into_iter().map(Out::Text).collect())),
        ("permissions", texts(&manifest.permissions)),
    ]);
    if let Some(entry) = &manifest.entry {
        root.push(("entry", Out::Text(entry)));
    }
    let mut text = String::new();
    emit(&mut text, &Out::Object(root), 0);
    text
}

fn emit(out: &mut String, value: &Out<'_>, indent: usize) {
    let newline = |out: &mut String, indent: usize| {
        out.push('\n');
        for _ in 0..indent {
            out.push_str("  ");
        }
    };
    match value {
        Out::Text(text) => json::write_string(out, text),
        Out::Number(number) => json::write_string(out, &number.to_string()),
        Out::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Out::List(items) if items.is_empty() => out.push_str("[]"),
        Out::List(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, indent + 1);
                emit(out, item, indent + 1);
            }
            newline(out, indent);
            out.push(']');
        }
        Out::Object(members) => {
            out.push('{');
            for (index, (name, member)) in members.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, indent + 1);
                json::write_string(out, name);
                out.push_str(": ");
                emit(out, member, indent + 1);
            }
            newline(out, indent);
            out.push('}');
        }
    }
}

fn invalid_text(message: String) -> FrameworkError {
    FrameworkError::new(ERROR_CODE_INVALID_ARGUMENT, message)
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 可选成员写了就必须有值：缺失表示没有，写成 `null` 不合法（RFC-0002 解析规则 2）。
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

/// 缺省的激活事件 `["onStartup"]`。
fn default_activation() -> Vec<String> {
    vec![ACTIVATION_ON_STARTUP.to_owned()]
}

/// 写了就至少一项：`activation` 缺失才是缺省的 `onStartup`，空数组不合法（RFC-0002 解析规则 2，ADR-0235）。
fn non_empty<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let events = Vec::<String>::deserialize(deserializer)?;
    if events.is_empty() {
        return Err(serde::de::Error::custom("the activation must list at least one event"));
    }
    Ok(events)
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
