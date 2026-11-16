//! 固定阶段的执行管道（RFC-0004）：中间件按阶段贡献，排序确定、可打印、可测试。
//!
//! [`Builder`] 收集中间件与顺序约束，[`Builder::build`] 按规范排序得到 [`Pipeline`]；每次调用以
//! [`Pipeline::run`] 执行，终点（通常是操作分派）在 handle 阶段之后由调用方给出。中间件与终点都是异步的，
//! 返回与运行时无关的 [`BoxFuture`]。
//!
//! ```
//! use xihan_framework::invocation::Context;
//! use xihan_framework::pipeline::{self, Builder, Placement, APP_OWNER};
//! use xihan_framework::primitives::PipelineStage;
//!
//! let observe = pipeline::from_fn("Observe", |ctx, next| Box::pin(async move { next.run(ctx).await }));
//! let mut builder = Builder::new();
//! builder.use_at(observe, PipelineStage::Observe, Placement::End, APP_OWNER).unwrap(); // 重复登记等错误在登记时即返回
//! assert_eq!(builder.dump().unwrap(), "observe      Observe\n");
//! let pipeline = builder.build().unwrap();
//! let terminal = pipeline::step_fn(|_ctx| Box::pin(async { Ok(()) }));
//! let ctx = Context::background();
//! let _outcome = pipeline.run(&ctx, &terminal); // 交给执行器 await
//! ```

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use crate::BoxFuture;
use crate::invocation::Context;
use crate::primitives::{FrameworkError, PipelineStage, Result};

/// 中间件顺序约束成环。
pub const ERROR_CODE_CYCLE: &str = "KERNEL_PIPELINE_001";
/// 阶段名未知。
pub const ERROR_CODE_UNKNOWN_STAGE: &str = "KERNEL_PIPELINE_002";
/// 中间件重复登记，或顺序约束引用了未登记、跨阶段或有歧义的中间件。
pub const ERROR_CODE_REGISTRATION: &str = "KERNEL_PIPELINE_003";

/// 应用自己登记的中间件的归属。
pub const APP_OWNER: &str = "app";

/// 管道里的一步：管道的终点，或交给中间件的下游。
pub trait Step: Send + Sync {
    /// 执行这一步。
    fn run<'a>(&'a self, ctx: &'a Context) -> BoxFuture<'a, Result<()>>;
}

impl<S: Step + ?Sized> Step for Arc<S> {
    fn run<'a>(&'a self, ctx: &'a Context) -> BoxFuture<'a, Result<()>> {
        (**self).run(ctx)
    }
}

/// 由函数实现的 [`Step`]，见 [`step_fn`]。
pub struct StepFn<F>(F);

/// 用函数创建一步：`step_fn(|ctx| Box::pin(async move { … }))`。
pub fn step_fn<F>(function: F) -> StepFn<F>
where
    F: for<'a> Fn(&'a Context) -> BoxFuture<'a, Result<()>> + Send + Sync,
{
    StepFn(function)
}

impl<F> Step for StepFn<F>
where
    F: for<'a> Fn(&'a Context) -> BoxFuture<'a, Result<()>> + Send + Sync,
{
    fn run<'a>(&'a self, ctx: &'a Context) -> BoxFuture<'a, Result<()>> {
        (self.0)(ctx)
    }
}

impl<F> fmt::Debug for StepFn<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StepFn")
    }
}

/// 一个中间件：可以在进入下游前后做事，也可以不调 `next` 而短路。
///
/// 可恢复失败以 `Err` 返回，不 panic。`next` 可以调用多次（如重试），也可以交给它派生的上下文。
pub trait Middleware: Send + Sync {
    /// 中间件名：顺序约束、打印与描述用。
    fn name(&self) -> &str;

    /// 处理一次调用；`next` 进入下游。
    fn invoke<'a>(&'a self, ctx: &'a Context, next: Next<'a>) -> BoxFuture<'a, Result<()>>;
}

impl<M: Middleware + ?Sized> Middleware for Arc<M> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn invoke<'a>(&'a self, ctx: &'a Context, next: Next<'a>) -> BoxFuture<'a, Result<()>> {
        (**self).invoke(ctx, next)
    }
}

/// 由函数实现的 [`Middleware`]，见 [`from_fn`]。
pub struct FnMiddleware<F> {
    name: String,
    function: F,
}

/// 用名字与函数创建中间件：`from_fn("Audit", |ctx, next| Box::pin(async move { next.run(ctx).await }))`。
pub fn from_fn<F>(name: impl Into<String>, function: F) -> FnMiddleware<F>
where
    F: for<'a> Fn(&'a Context, Next<'a>) -> BoxFuture<'a, Result<()>> + Send + Sync,
{
    FnMiddleware { name: name.into(), function }
}

impl<F> Middleware for FnMiddleware<F>
where
    F: for<'a> Fn(&'a Context, Next<'a>) -> BoxFuture<'a, Result<()>> + Send + Sync,
{
    fn name(&self) -> &str {
        &self.name
    }

    fn invoke<'a>(&'a self, ctx: &'a Context, next: Next<'a>) -> BoxFuture<'a, Result<()>> {
        (self.function)(ctx, next)
    }
}

impl<F> fmt::Debug for FnMiddleware<F> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FnMiddleware").field("name", &self.name).finish_non_exhaustive()
    }
}

/// 中间件的下游：其后的中间件与终点。可复制，调用多次即多次进入下游。
#[derive(Clone, Copy)]
pub struct Next<'a> {
    rest: &'a [Arc<dyn Middleware>],
    terminal: &'a dyn Step,
}

impl<'a> Next<'a> {
    /// 进入下游。
    pub fn run(self, ctx: &'a Context) -> BoxFuture<'a, Result<()>> {
        match self.rest.split_first() {
            Some((middleware, rest)) => middleware.invoke(ctx, Next { rest, terminal: self.terminal }),
            None => self.terminal.run(ctx),
        }
    }
}

impl fmt::Debug for Next<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Next").field("remaining", &self.rest.len()).finish_non_exhaustive()
    }
}

/// 中间件在阶段内的位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Placement {
    /// 阶段开头。
    Beginning,
    /// 阶段末尾。
    End,
}

impl fmt::Display for Placement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Beginning => "Beginning",
            Self::End => "End",
        })
    }
}

/// 一个已排序的中间件的描述。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Descriptor {
    /// 所在阶段。
    pub stage: PipelineStage,
    /// 中间件名。
    pub middleware: String,
    /// 按名登记的实例名；默认实例为 `None`。
    pub instance: Option<String>,
    /// 归属：清单 ID 或 [`APP_OWNER`]。
    pub owner: String,
    /// 阶段内位置。
    pub placement: Placement,
}

impl Descriptor {
    /// 打印名：中间件名，命名实例为 `中间件名[实例名]`。
    pub fn display_name(&self) -> String {
        match &self.instance {
            Some(instance) => format!("{}[{instance}]", self.middleware),
            None => self.middleware.clone(),
        }
    }
}

/// 一次登记。
#[derive(Clone)]
struct Registration {
    middleware: Arc<dyn Middleware>,
    descriptor: Descriptor,
}

/// 一条顺序约束：主体 `name` 在目标之前（`before`）或之后。
#[derive(Clone)]
struct Constraint {
    name: String,
    // 主体在声明时解析成登记的下标
    subject: usize,
    target: String,
    before: bool,
}

/// 收集中间件与顺序约束，按规范排序后构建管道。
///
/// 报错的时机照 RFC-0004 排序规则第 8 条：登记时立即报的是重复登记、空的名字或归属、约束主体未登记或按名字有歧义
/// （登记方法返回 `Err`，这一项不生效，构建器照常可用）；排序时（[`Builder::describe`]、[`Builder::dump`]、[`Builder::build`]）
/// 报的是约束目标未登记、有歧义或不在同一阶段，以及约束成环。登记方法返回 `&mut Self`，可以接着 `?` 链式调用。
/// 克隆共享已登记的中间件实例（`Arc`），可以从同一份基础登记派生几条管道。
#[derive(Clone, Default)]
pub struct Builder {
    // 下标即登记顺序（排序规则第 4 条）
    registrations: Vec<Registration>,
    constraints: Vec<Constraint>,
}

impl Builder {
    /// 创建空的构建器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记中间件到 transaction 阶段末尾，归属 [`APP_OWNER`]（排序规则第 1 条）；错误同 [`Builder::use_at`]。
    pub fn use_middleware(&mut self, middleware: impl Middleware + 'static) -> std::result::Result<&mut Self, FrameworkError> {
        self.use_at(middleware, PipelineStage::Transaction, Placement::End, APP_OWNER)
    }

    /// 登记中间件到指定阶段与位置；`owner` 是清单 ID 或 [`APP_OWNER`]。
    ///
    /// 中间件名或归属为空、同一中间件的默认实例已登记过，立即返回 `KERNEL_PIPELINE_003`。
    pub fn use_at(
        &mut self,
        middleware: impl Middleware + 'static,
        stage: PipelineStage,
        placement: Placement,
        owner: &str,
    ) -> std::result::Result<&mut Self, FrameworkError> {
        self.add(Arc::new(middleware), None, stage, placement, owner)
    }

    /// 按名字登记同一中间件的另一个实例（排序规则第 7 条），打印为 `中间件名[实例名]`。
    ///
    /// 名字在中间件名内唯一；实例名、中间件名或归属为空、同名实例已登记过，立即返回 `KERNEL_PIPELINE_003`。
    pub fn use_named_instance(
        &mut self,
        instance: &str,
        middleware: impl Middleware + 'static,
        stage: PipelineStage,
        placement: Placement,
        owner: &str,
    ) -> std::result::Result<&mut Self, FrameworkError> {
        self.add(Arc::new(middleware), Some(instance.to_owned()), stage, placement, owner)
    }

    fn add(
        &mut self,
        middleware: Arc<dyn Middleware>,
        instance: Option<String>,
        stage: PipelineStage,
        placement: Placement,
        owner: &str,
    ) -> std::result::Result<&mut Self, FrameworkError> {
        let descriptor = Descriptor { stage, middleware: middleware.name().to_owned(), instance, owner: owner.to_owned(), placement };
        if descriptor.middleware.is_empty() {
            return Err(registration_error("a middleware has an empty name".to_owned()));
        }
        if descriptor.instance.as_deref() == Some("") {
            return Err(registration_error(format!("a named instance of the middleware '{}' has an empty name", descriptor.middleware)));
        }
        if descriptor.owner.is_empty() {
            return Err(registration_error(format!("the middleware '{}' has an empty owner", descriptor.display_name())));
        }
        // 同一中间件的默认实例只登记一次，命名实例的名字在中间件内唯一；登记数有限，逐个比较即可
        let duplicate = self
            .registrations
            .iter()
            .any(|registration| registration.descriptor.middleware == descriptor.middleware && registration.descriptor.instance == descriptor.instance);
        if duplicate {
            return Err(registration_error(format!("the middleware '{}' is registered more than once", descriptor.display_name())));
        }
        self.registrations.push(Registration { middleware, descriptor });
        Ok(self)
    }

    /// 约束 `middleware` 在 `other` 之前。
    ///
    /// 两端都是名字：按名字登记的实例用实例名，没有名字的登记用中间件名（RFC-0004 排序规则第 7 条）。主体 `middleware`
    /// 在声明时就必须已登记，没有或有歧义立即返回 `KERNEL_PIPELINE_003`；目标 `other` 在排序时解析。
    pub fn before(&mut self, middleware: &str, other: &str) -> std::result::Result<&mut Self, FrameworkError> {
        self.constrain(middleware, other, true)
    }

    /// 约束 `middleware` 在 `other` 之后；名字的解析与报错的时机同 [`Builder::before`]。
    pub fn after(&mut self, middleware: &str, other: &str) -> std::result::Result<&mut Self, FrameworkError> {
        self.constrain(middleware, other, false)
    }

    fn constrain(&mut self, middleware: &str, other: &str, before: bool) -> std::result::Result<&mut Self, FrameworkError> {
        let subject = self.subject(middleware)?;
        self.constraints.push(Constraint { name: middleware.to_owned(), subject, target: other.to_owned(), before });
        Ok(self)
    }

    /// 按执行顺序返回中间件描述。
    pub fn describe(&self) -> std::result::Result<Vec<Descriptor>, FrameworkError> {
        Ok(self.sorted()?.into_iter().map(|index| self.registrations[index].descriptor.clone()).collect())
    }

    /// 按 `xihan pipeline` 的格式打印：一行一个中间件，阶段名左对齐占 12 列、空一格后接打印名。
    pub fn dump(&self) -> std::result::Result<String, FrameworkError> {
        Ok(dump(&self.describe()?))
    }

    /// 构建管道；之后对构建器的改动不影响已构建的管道。
    pub fn build(&self) -> std::result::Result<Pipeline, FrameworkError> {
        let order = self.sorted()?;
        Ok(Pipeline {
            middlewares: order.iter().map(|&index| Arc::clone(&self.registrations[index].middleware)).collect(),
            descriptors: order.iter().map(|&index| self.registrations[index].descriptor.clone()).collect(),
        })
    }

    /// 按规则排序，返回登记的下标：先阶段；阶段内先位置，再归属的序数字典序，再登记顺序；最后按约束做稳定拓扑排序。
    fn sorted(&self) -> std::result::Result<Vec<usize>, FrameworkError> {
        let mut edges = Vec::with_capacity(self.constraints.len());
        for constraint in &self.constraints {
            let (subject, target) = (constraint.subject, self.target(&constraint.target)?);
            if self.registrations[subject].descriptor.stage != self.registrations[target].descriptor.stage {
                let (first, second) = if constraint.before { (&constraint.name, &constraint.target) } else { (&constraint.target, &constraint.name) };
                return Err(registration_error(format!("the ordering constraint between '{first}' and '{second}' crosses stages")));
            }
            edges.push(if constraint.before { (subject, target) } else { (target, subject) });
        }

        let mut result = Vec::with_capacity(self.registrations.len());
        for &stage in PipelineStage::ALL {
            let mut members: Vec<usize> = (0..self.registrations.len()).filter(|&index| self.registrations[index].descriptor.stage == stage).collect();
            // 基准次序（排序规则第 2–4 条）：归属按序数字典序，逐个 UTF-16 码元比较，与 .NET 的 Ordinal 一致；下标唯一，所以是全序
            members.sort_by(|&left, &right| {
                let (a, b) = (&self.registrations[left].descriptor, &self.registrations[right].descriptor);
                a.placement.cmp(&b.placement).then_with(|| a.owner.encode_utf16().cmp(b.owner.encode_utf16())).then(left.cmp(&right))
            });
            result.extend(topological_sort(stage, members, &edges)?);
        }
        Ok(result)
    }

    /// 在登记里按名字找唯一的一个：按名字登记的实例只认实例名，没有名字的登记认中间件名（RFC-0004 排序规则第 7 条）。
    ///
    /// 没有为 `Ok(None)`；对应多个（不同中间件的实例同名，或实例名与某个中间件名相同）为歧义的登记错误。
    fn find(&self, name: &str) -> std::result::Result<Option<usize>, FrameworkError> {
        let mut matches = (0..self.registrations.len()).filter(|&index| {
            let descriptor = &self.registrations[index].descriptor;
            descriptor.instance.as_deref().map_or(descriptor.middleware == name, |instance| instance == name)
        });
        match (matches.next(), matches.next()) {
            (None, _) => Ok(None),
            (Some(index), None) => Ok(Some(index)),
            _ => Err(registration_error(format!(
                "the middleware name '{name}' matches several registrations, so ordering constraints by name are ambiguous for it"
            ))),
        }
    }

    /// 解析约束的主体：声明时就必须已登记（排序规则第 7、8 条）。
    fn subject(&self, name: &str) -> std::result::Result<usize, FrameworkError> {
        self.find(name)?
            .ok_or_else(|| registration_error(format!("the middleware '{name}' is not registered, so no ordering constraint can be declared for it")))
    }

    /// 解析约束的目标：在排序时按当时的登记解析。
    fn target(&self, name: &str) -> std::result::Result<usize, FrameworkError> {
        self.find(name)?.ok_or_else(|| registration_error(format!("the ordering constraint references the unregistered middleware '{name}'")))
    }
}

impl fmt::Debug for Builder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let descriptors: Vec<&Descriptor> = self.registrations.iter().map(|registration| &registration.descriptor).collect();
        formatter.debug_struct("Builder").field("registrations", &descriptors).field("constraints", &self.constraints.len()).finish()
    }
}

/// 在基准次序上应用约束做稳定拓扑排序（Kahn，每次取基准次序里最靠前的、没有前驱的中间件）。
///
/// 约束只调整必要的相对位置，其余保持基准次序（排序规则第 5 条）；跨阶段的约束已在之前拒绝，这里只看本阶段的边。
fn topological_sort(stage: PipelineStage, members: Vec<usize>, edges: &[(usize, usize)]) -> std::result::Result<Vec<usize>, FrameworkError> {
    let in_stage: HashSet<usize> = members.iter().copied().collect();
    let mut incoming: HashMap<usize, usize> = HashMap::new();
    let mut outgoing: HashMap<usize, Vec<usize>> = HashMap::new();
    for &(first, second) in edges.iter().filter(|(first, _)| in_stage.contains(first)) {
        outgoing.entry(first).or_default().push(second);
        *incoming.entry(second).or_default() += 1;
    }
    let mut remaining = members;
    let mut ordered = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        // 剩下的每个都有前驱即成环
        let Some(position) = remaining.iter().position(|member| incoming.get(member).is_none_or(|&count| count == 0)) else {
            return Err(FrameworkError::new(ERROR_CODE_CYCLE, format!("the ordering constraints in stage '{stage}' form a cycle.")));
        };
        let node = remaining.remove(position);
        ordered.push(node);
        for successor in outgoing.get(&node).into_iter().flatten() {
            if let Some(count) = incoming.get_mut(successor) {
                *count -= 1;
            }
        }
    }
    Ok(ordered)
}

/// 按打印格式输出描述。
fn dump(descriptors: &[Descriptor]) -> String {
    descriptors.iter().map(|descriptor| format!("{:<12} {}\n", descriptor.stage.manifest_name(), descriptor.display_name())).collect()
}

fn registration_error(message: String) -> FrameworkError {
    FrameworkError::new(ERROR_CODE_REGISTRATION, message + ".")
}

/// 解析清单里的阶段名；未知阶段返回 `KERNEL_PIPELINE_002`。
pub fn parse_stage(name: &str) -> std::result::Result<PipelineStage, FrameworkError> {
    PipelineStage::parse(name).ok_or_else(|| FrameworkError::new(ERROR_CODE_UNKNOWN_STAGE, format!("'{name}' is not a pipeline stage.")))
}

/// 已排序的管道：每次调用经 [`Pipeline::run`] 依次穿过全部中间件，最后进入调用方给出的终点。
pub struct Pipeline {
    middlewares: Vec<Arc<dyn Middleware>>,
    descriptors: Vec<Descriptor>,
}

impl Pipeline {
    /// 执行一次调用：中间件按排序依次进入，`terminal` 在最后一个中间件之后（handle 阶段的终点）。
    pub fn run<'a>(&'a self, ctx: &'a Context, terminal: &'a dyn Step) -> BoxFuture<'a, Result<()>> {
        Next { rest: &self.middlewares, terminal }.run(ctx)
    }

    /// 按执行顺序的中间件描述。
    pub fn descriptors(&self) -> &[Descriptor] {
        &self.descriptors
    }

    /// 按 [`Builder::dump`] 的格式打印。
    pub fn dump(&self) -> String {
        dump(&self.descriptors)
    }
}

impl fmt::Debug for Pipeline {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Pipeline").field("descriptors", &self.descriptors).finish()
    }
}
