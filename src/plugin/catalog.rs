//! 握手交给宿主的消息目录（`plugin.proto` 的 `MessageCatalog`，`handshake.md` §3 第 6 条，RFC-0010）。

use std::collections::BTreeMap;

use crate::primitives::{self, Catalog, DEFAULT_LOCALE, FrameworkError, SDK_NAME, is_message_key, json};

/// 消息目录不合法（JSON、键或区域名错误）的错误码（`conventions/error-codes.md`）。
const ERROR_CODE_INVALID_CATALOG: &str = "LOCALIZATION_VALIDATION_002";

/// 内置的 10 种区域（RFC-0010），[`DEFAULT_LOCALE`]（`en-US`）在前，也是回退区域。
pub const LOCALES: [&str; 10] = ["en-US", "zh-CN", "zh-TW", "ja-JP", "ko-KR", "de-DE", "es-ES", "fr-FR", "pt-BR", "ru-RU"];

/// 一个消息目录：目录名，加上区域 → 消息键 → 模板（`{name}` 占位符，RFC-0010「模板」）。
///
/// 宿主用握手时拿到的目录翻译插件返回的错误（`invocation.md` §3）；目录至少要有 `en-US`，[`Plugin::start`](super::Plugin::start)
/// 在监听之前核对。要让插件自己的 `Error::message` 也按目录解析，把 [`MessageCatalog::to_catalog`] 交给
/// [`set_default_messages`](crate::primitives::set_default_messages)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageCatalog {
    name: String,
    locales: BTreeMap<String, BTreeMap<String, String>>,
}

impl MessageCatalog {
    /// 创建空目录；目录名通常取清单 ID 的领域段（`plugin.proto`）。
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), locales: BTreeMap::new() }
    }

    /// 从每种区域一份的 JSON 文本读取：每份是「消息键 → 模板」的对象，先过共同的 JSON 读取检查（重复的键不合法）。
    ///
    /// 文本不是这样的对象、键不合消息键文法或区域名不合法时返回 `LOCALIZATION_VALIDATION_002`。
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
        let mut catalog = Self::new(name);
        for (locale, text) in files {
            let locale = locale.into();
            let templates: BTreeMap<String, String> = json::from_str(text.as_ref()).map_err(|error| {
                FrameworkError::new(
                    ERROR_CODE_INVALID_CATALOG,
                    format!("The '{locale}' templates of the message catalog '{}' are not a JSON object of strings: {error}.", catalog.name),
                )
            })?;
            for (key, template) in templates {
                catalog.insert(locale.clone(), key, template);
            }
        }
        catalog.validate()?;
        Ok(catalog)
    }

    /// 放入一条模板，返回同一区域同一键原来的模板；键与区域名在 [`Plugin::start`](super::Plugin::start) 时核对。
    pub fn insert(&mut self, locale: impl Into<String>, key: impl Into<String>, template: impl Into<String>) -> Option<String> {
        self.locales.entry(locale.into()).or_default().insert(key.into(), template.into())
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
        self.locales.keys().map(String::as_str)
    }

    /// 按键排序列出一种区域的模板；没有这种区域时为空。
    pub fn templates(&self, locale: &str) -> impl Iterator<Item = (&str, &str)> {
        self.locales.get(locale).into_iter().flatten().map(|(key, template)| (key.as_str(), template.as_str()))
    }

    /// 转成原语层的 [`Catalog`]：可以作为进程默认的消息解析器，按区域解析、回退到 `en-US`、占位符缺参数时解析不出。
    pub fn to_catalog(&self) -> Catalog {
        let mut catalog = Catalog::new();
        for (locale, templates) in &self.locales {
            for (key, template) in templates {
                catalog.insert(locale.clone(), key.clone(), template.clone());
            }
        }
        catalog
    }

    /// 核对目录：名字非空、有 `en-US`（`handshake.md` §3 第 6 条「至少应含 en-US」）、区域名与消息键合文法。
    pub(crate) fn validate(&self) -> Result<(), FrameworkError> {
        let invalid = |message: String| Err(FrameworkError::new(ERROR_CODE_INVALID_CATALOG, message));
        if self.name.is_empty() {
            return invalid("A message catalog needs a name.".to_owned());
        }
        if !self.locales.contains_key(DEFAULT_LOCALE) {
            return invalid(format!("The message catalog '{}' has no {DEFAULT_LOCALE} templates.", self.name));
        }
        for (locale, templates) in &self.locales {
            if !primitives::is_locale_name(locale) {
                return invalid(format!("The message catalog '{}' has an invalid locale name '{locale}'.", self.name));
            }
            if let Some(key) = templates.keys().find(|key| !is_message_key(key)) {
                return invalid(format!("The message catalog '{}' has an invalid message key '{key}' in '{locale}'.", self.name));
            }
        }
        Ok(())
    }
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
    const FILES: [(&str, &str); 10] = [
        ("en-US", include_str!("localization/en-US.json")),
        ("zh-CN", include_str!("localization/zh-CN.json")),
        ("zh-TW", include_str!("localization/zh-TW.json")),
        ("ja-JP", include_str!("localization/ja-JP.json")),
        ("ko-KR", include_str!("localization/ko-KR.json")),
        ("de-DE", include_str!("localization/de-DE.json")),
        ("es-ES", include_str!("localization/es-ES.json")),
        ("fr-FR", include_str!("localization/fr-FR.json")),
        ("pt-BR", include_str!("localization/pt-BR.json")),
        ("ru-RU", include_str!("localization/ru-RU.json")),
    ];
    // 文件随 crate 编译进来，内容由测试核对，读不出只能是改坏了文件
    MessageCatalog::from_json(SDK_NAME, FILES).unwrap_or_else(|error| panic!("The built-in plugin message catalog is invalid: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::BUILTIN_TEMPLATES;
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
