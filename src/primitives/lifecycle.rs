use super::named::named_enum;

named_enum! {
    /// 九态生命周期（RFC-0001）；数值与 .NET 枚举和插件协议的 `LifecycleState` 一致，名字是 PascalCase。
    pub enum LifecycleState ("lifecycle state") {
        /// 已声明。
        Declared = 0 => "Declared",
        /// 依赖已解析。
        Resolved = 1 => "Resolved",
        /// 已配置。
        Configured = 2 => "Configured",
        /// 已启动。
        Started = 3 => "Started",
        /// 就绪，可服务。
        Ready = 4 => "Ready",
        /// 降级，仍可服务。
        Degraded = 5 => "Degraded",
        /// 排空中。
        Draining = 6 => "Draining",
        /// 已停止，终态。
        Stopped = 7 => "Stopped",
        /// 失败，可以重新解析。
        Failed = 8 => "Failed",
    }
}

impl LifecycleState {
    /// 协议里的数值。
    pub const fn value(self) -> i32 {
        self as i32
    }

    /// 按协议数值取状态；不对应任何状态时为 `None`。
    pub fn from_value(value: i32) -> Option<Self> {
        Self::ALL.iter().copied().find(|state| state.value() == value)
    }

    /// 从本状态出发的全部合法迁移（`spec/conformance/vectors/primitives/lifecycle-transitions.json`）；未列出的一律非法。
    pub const fn transitions(self) -> &'static [Self] {
        use LifecycleState::*;
        match self {
            Declared => &[Resolved, Failed],
            Resolved => &[Configured, Failed],
            Configured => &[Started, Failed],
            Started => &[Ready, Degraded, Draining, Failed],
            Ready => &[Degraded, Draining, Failed],
            Degraded => &[Ready, Draining, Failed],
            Draining => &[Stopped, Failed],
            Stopped => &[],
            Failed => &[Resolved],
        }
    }

    /// 判断迁移是否合法。
    pub fn can_transition_to(self, to: Self) -> bool {
        self.transitions().contains(&to)
    }

    /// 判断是否可服务：`Ready` 或 `Degraded`。
    pub const fn is_serving(self) -> bool {
        matches!(self, Self::Ready | Self::Degraded)
    }

    /// 判断是否终态：`Stopped`。
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped)
    }
}
