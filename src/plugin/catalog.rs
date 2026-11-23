//! 握手交给宿主的消息目录（`plugin.proto` 的 `MessageCatalog`，`handshake.md` §3 第 6 条，RFC-0010）。

use crate::primitives::{Catalog, DEFAULT_LOCALE, ERROR_CODE_INVALID_CATALOG, FrameworkError, SDK_NAME, builtin_locales};

/// 一个消息目录：目录名，加上区域 → 消息键 → 模板（`{name}` 占位符，RFC-0010「模板」）。
///
/// 宿主用握手时拿到的目录翻译插件返回的错误（`invocation.md` §3）；目录至少要有 `en-US`，[`Plugin::start`](super::Plugin::start)
/// 在监听之前核对。要让插件自己的 `Error::message` 也按目录解析，把 [`MessageCatalog::to_catalog`] 交给
/// [`set_default_messages`](crate::primitives::set_default_messages)。
///
/// 模板放在原语层的 [`Catalog`] 里，这里只多一个目录名与握手的要求（有名字、有 `en-US`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageCatalog {
    name: String,
    catalog: Catalog,
}

impl MessageCatalog {
    /// 创建空目录；目录名通常取清单 ID 的领域段（`plugin.proto`）。
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), catalog: Catalog::new() }
    }

    /// 从每种区域一份的 JSON 文本读取：每份是「消息键 → 模板」的对象，先过共同的 JSON 读取检查（重复的键不合法），读法同
    /// [`Catalog::from_json`]。
    ///
    /// 文本不是这样的对象、目录名为空、没有 `en-US`、键不合消息键文法或区域名不合法时返回 `LOCALIZATION_VALIDATION_002`
    /// （[`ERROR_CODE_INVALID_CATALOG`]）。
    ///
    /// ```
    /// use xihan_framework::plugin::MessageCatalog;
    ///
    /// let catalog = MessageCatalog::from_json("echo", [("en-US", r#"{"echo.text.empty":"The text must not be empty."}"#)]).unwrap();
    /// assert_eq!(catalog.templates("en-US").collect::<Vec<_>>(), [("echo.text.empty", "The text must not be empty.")]);
    /// ```
    pub fn from_json<I, L, T>(name: impl Into<String>, files: I) -> Result<Self, FrameworkError>
    where
        I: IntoIterator<Item = (L, T)>,
        L: Into<String>,
        T: AsRef<str>,
    {
        let name = name.into();
        let catalog = Catalog::read(files, &label(&name))?;
        let catalog = Self { name, catalog };
        catalog.validate()?;
        Ok(catalog)
    }

    /// 放入一条模板，返回同一区域同一键原来的模板；键与区域名在 [`Plugin::start`](super::Plugin::start) 时核对。
    pub fn insert(&mut self, locale: impl Into<String>, key: impl Into<String>, template: impl Into<String>) -> Option<String> {
        self.catalog.insert(locale, key, template)
    }

    /// 放入一条模板并返回目录本身，便于链式构建。
    pub fn with(mut self, locale: impl Into<String>, key: impl Into<String>, template: impl Into<String>) -> Self {
        self.insert(locale, key, template);
        self
    }

    /// 目录名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 按名字排序列出区域。
    pub fn locales(&self) -> impl Iterator<Item = &str> {
        self.catalog.locales()
    }

    /// 按键排序列出一种区域的模板；没有这种区域时为空。
    pub fn templates(&self, locale: &str) -> impl Iterator<Item = (&str, &str)> {
        self.catalog.templates(locale)
    }

    /// 转成原语层的 [`Catalog`]：可以作为进程默认的消息解析器，按区域解析、回退到 `en-US`、占位符缺参数时解析不出；几份目录用
    /// [`Catalog::extend`] 合成一份。
    pub fn to_catalog(&self) -> Catalog {
        self.catalog.clone()
    }

    /// 核对目录：名字非空、有 `en-US`（`handshake.md` §3 第 6 条「至少应含 en-US」）、区域名与消息键合文法。
    pub(crate) fn validate(&self) -> Result<(), FrameworkError> {
        let invalid = |message: String| Err(FrameworkError::new(ERROR_CODE_INVALID_CATALOG, message));
        if self.name.is_empty() {
            return invalid("A message catalog needs a name.".to_owned());
        }
        if !self.catalog.locales().any(|locale| locale == DEFAULT_LOCALE) {
            return invalid(format!("The message catalog '{}' has no {DEFAULT_LOCALE} templates.", self.name));
        }
        self.catalog.check(&label(&self.name))
    }
}

/// 错误说明里对一份有名字的目录的称呼。
fn label(name: &str) -> String {
    format!("message catalog '{name}'")
}

/// 按区域取随 crate 编译进来的文案文件 `localization/<区域>.json`；区域名来自原语层的那一份，这里不再写一遍。
macro_rules! locale_files {
    ($($name:literal),+) => {
        [$(($name, include_str!(concat!("localization/", $name, ".json")))),+]
    };
}

/// 运行时自己的消息目录：它与宿主共用的 `plugins.*` 消息键（`invocation.md` §8）在 10 种区域的文案，同参考实现，目录名是 SDK 名。
///
/// 宿主有这些键的目录，运行时不随握手发送它（`handshake.md` §3 第 6 条），错误交给宿主时只带键与参数、`text` 留空，由宿主按调用方的
/// 区域翻译。它供本地渲染：en-US 已经并进 SDK 自带的模板；要按别的区域渲染，把它的 [`MessageCatalog::to_catalog`] 与插件自己的目录
/// 一起交给 [`set_default_messages`](crate::primitives::set_default_messages)。
///
/// ```
/// use xihan_framework::plugin::{self, MESSAGE_UNAVAILABLE};
///
/// let catalog = plugin::runtime_catalog();
/// assert_eq!(catalog.locales().count(), plugin::LOCALES.len());
/// assert!(catalog.templates("zh-CN").any(|(key, _)| key == MESSAGE_UNAVAILABLE));
/// ```
pub fn runtime_catalog() -> MessageCatalog {
    const FILES: [(&str, &str); 10] = builtin_locales!(locale_files);
    // 文件随 crate 编译进来，内容由测试核对，读不出只能是改坏了文件
    MessageCatalog::from_json(SDK_NAME, FILES).unwrap_or_else(|error| panic!("The built-in plugin message catalog is invalid: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{BUILTIN_TEMPLATES, LOCALES};
    use crate::primitives::placeholders;

    /// 10 种区域的文件键相同、占位符相同，en-US 与自带的模板逐字相同；键、参数与 en-US 模板同向量 `plugin/plugin-messages.json`。
    #[test]
    fn runtime_catalog_covers_every_locale() {
        let catalog = runtime_catalog();
        assert_eq!(catalog.locales().count(), LOCALES.len());
        let english: Vec<(&str, &str)> = catalog.templates(DEFAULT_LOCALE).collect();
        let mut builtin = BUILTIN_TEMPLATES.to_vec();
        builtin.sort_unstable();
        assert_eq!(english, builtin);

        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/spec/conformance/vectors/plugin/plugin-messages.json");
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("cannot read {path}: {error}; run `git submodule update --init`"));
        let vectors: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut expected: Vec<(&str, &str)> =
            vectors["messages"].as_array().unwrap().iter().map(|message| (message["key"].as_str().unwrap(), message["template"].as_str().unwrap())).collect();
        expected.sort_unstable();
        assert_eq!(english, expected, "the runtime keys and en-US templates are those of plugin-messages.json, nothing more");
        for message in vectors["messages"].as_array().unwrap() {
            let mut arguments: Vec<&str> = message["arguments"].as_array().unwrap().iter().map(|argument| argument.as_str().unwrap()).collect();
            let mut used = placeholders(message["template"].as_str().unwrap());
            arguments.sort_unstable();
            used.sort_unstable();
            assert_eq!(used, arguments, "{}", message["key"]);
        }
        for locale in LOCALES {
            let templates: Vec<(&str, &str)> = catalog.templates(locale).collect();
            assert_eq!(templates.len(), english.len(), "{locale}");
            for ((key, template), (english_key, english_template)) in templates.iter().zip(&english) {
                assert_eq!(key, english_key, "{locale}");
                let (mut ours, mut theirs) = (placeholders(template), placeholders(english_template));
                ours.sort_unstable();
                theirs.sort_unstable();
                assert_eq!(ours, theirs, "{locale} {key}");
            }
        }
    }

    #[test]
    fn catalogs_are_validated() {
        assert!(MessageCatalog::new("echo").with("en-US", "echo.ok", "ok").validate().is_ok());
        for catalog in [
            MessageCatalog::new("").with("en-US", "echo.ok", "ok"),
            MessageCatalog::new("echo").with("zh-CN", "echo.ok", "好"),
            MessageCatalog::new("echo").with("en-US", "Echo.ok", "ok"),
            MessageCatalog::new("echo").with("en-US", "echo.ok", "ok").with("en_US", "echo.ok", "ok"),
        ] {
            assert_eq!(catalog.validate().unwrap_err().code(), ERROR_CODE_INVALID_CATALOG, "{catalog:?}");
        }
        let duplicate = MessageCatalog::from_json("echo", [("en-US", r#"{"echo.ok":"a","echo.ok":"b"}"#)]).unwrap_err();
        assert_eq!(duplicate.code(), ERROR_CODE_INVALID_CATALOG);
    }
}
