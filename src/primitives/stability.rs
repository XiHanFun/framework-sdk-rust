use super::named::named_enum;

named_enum! {
    /// 五级稳定性（RFC-0001「稳定性级别」）；名字是清单与协议里的小写名。
    pub enum Stability ("stability") {
        /// 同一大版本内不破坏。
        Stable = 0 => "stable",
        /// 小版本可以破坏，补丁版本不破坏。
        Preview = 1 => "preview",
        /// 小版本可以破坏或移除，补丁版本不破坏。
        Experimental = 2 => "experimental",
        /// 不承诺，只供声明它的包自己使用。
        Internal = 3 => "internal",
        /// 已弃用，只在大版本移除。
        Deprecated = 4 => "deprecated",
    }
}
