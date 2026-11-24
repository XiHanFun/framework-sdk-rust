//! 按 Schema 逐项核对清单的形状（RFC-0002 解析规则 2 第 3 步），报第一处问题的路径。
//!
//! 对象的成员按在文本里出现的顺序核对，每个成员依次查：Schema 之外的成员 → `null` → 类型 → 取值，对象与数组进到里面再查，
//! 数组项按下标顺序；一个对象的成员都查完后，按 Schema 里 `required` 的顺序查缺少的成员，路径是缺少的成员本身。
//! 文法照 Schema 的 `pattern` 按整串匹配，枚举只认小写名。`provides` 与 `requires` 的重复留给跨字段约束。

use crate::grammar::{all, digit, lower, lower_digit, lower_digit_hyphen, operation_id, positive, upper, word};
use crate::primitives::json::{Node, push_index, push_member};

/// 一个位置的规则。
enum Rule {
    /// 字符串，文本符合文法。
    Text(fn(&str) -> bool),
    /// 布尔。
    Bool,
    /// 字符串，取值是这几个名字之一。
    Name(&'static [&'static str]),
    /// 数组：至少 `min` 项，每项符合 `item`；`unique` 时不得重复，路径是第二次出现的项。
    List { item: &'static Rule, min: usize, unique: bool },
    /// 对象。
    Object(&'static Shape),
}

/// 对象的成员与必填成员，顺序照 Schema。
struct Shape {
    members: &'static [(&'static str, Rule)],
    required: &'static [&'static str],
}

const ANY_TEXT: Rule = Rule::Text(|_| true);
const TEXTS: Rule = Rule::List { item: &ANY_TEXT, min: 0, unique: false };

const CAPABILITY: Shape = Shape { members: &[("id", Rule::Text(is_capability_id)), ("version", Rule::Text(is_version))], required: &["id", "version"] };

const REQUIREMENT: Shape = Shape {
    members: &[("id", Rule::Text(is_capability_id)), ("version", Rule::Text(is_version_range)), ("optional", Rule::Bool)],
    required: &["id", "version"],
};

const BINDING: Shape =
    Shape { members: &[("name", Rule::Text(is_binding_name)), ("type", Rule::Text(is_binding_type)), ("required", Rule::Bool)], required: &["name", "type"] };

const STAGES: &[&str] =
    &["ingress", "observe", "resilience", "authenticate", "tenant", "authorize", "validate", "idempotency", "transaction", "handle", "egress"];

const PIPELINE_ITEM: Shape = Shape {
    members: &[("middleware", Rule::Text(|name| !name.is_empty())), ("stage", Rule::Name(STAGES)), ("before", TEXTS), ("after", TEXTS)],
    required: &["middleware", "stage"],
};

const CONFIG: Shape = Shape { members: &[("section", Rule::Text(is_config_section)), ("schema", ANY_TEXT)], required: &["section"] };

const CONTRIBUTIONS: Shape = Shape {
    members: &[
        ("pipeline", Rule::List { item: &Rule::Object(&PIPELINE_ITEM), min: 0, unique: false }),
        ("operations", Rule::List { item: &Rule::Text(operation_id), min: 0, unique: false }),
        ("config", Rule::Object(&CONFIG)),
        ("permissions", Rule::List { item: &Rule::Text(is_permission_code), min: 0, unique: true }),
        ("health", Rule::List { item: &Rule::Text(is_health_name), min: 0, unique: false }),
    ],
    required: &[],
};

/// 清单的根（`spec/manifest/manifest.schema.json`）。
const MANIFEST: Shape = Shape {
    members: &[
        ("$schema", ANY_TEXT),
        ("id", Rule::Text(is_manifest_id)),
        ("package", ANY_TEXT),
        ("version", Rule::Text(is_semver)),
        ("primitives", Rule::Text(is_version)),
        ("stability", Rule::Name(&["stable", "preview", "experimental", "internal", "deprecated"])),
        ("kind", Rule::Name(&["capability", "provider", "plugin", "host", "protocol"])),
        ("role", Rule::Name(&["provider", "default"])),
        ("isolation", Rule::Name(&["in-process", "wasm", "out-of-process"])),
        ("provides", Rule::List { item: &Rule::Object(&CAPABILITY), min: 0, unique: false }),
        ("requires", Rule::List { item: &Rule::Object(&REQUIREMENT), min: 0, unique: false }),
        ("bindings", Rule::List { item: &Rule::Object(&BINDING), min: 0, unique: false }),
        ("contributes", Rule::Object(&CONTRIBUTIONS)),
        ("activation", Rule::List { item: &Rule::Text(is_activation_event), min: 1, unique: false }),
        ("permissions", Rule::List { item: &Rule::Text(is_permission), min: 0, unique: false }),
        ("entry", ANY_TEXT),
    ],
    required: &["id", "version", "primitives", "stability", "kind"],
};

/// 核对清单的根；通过时为 `Ok(())`，否则是第一处问题的路径（根不是对象时为 `$`）。
pub(super) fn check(root: &Node) -> Result<(), String> {
    let mut path = String::from("$");
    match root {
        Node::Object(members) => object(&MANIFEST, members, &mut path),
        _ => Err(path),
    }
}

fn object(shape: &Shape, members: &[(String, Node)], path: &mut String) -> Result<(), String> {
    let parent = path.len();
    for (name, value) in members {
        push_member(path, name);
        // 成员名区分大小写：大小写不对的成员就是 Schema 之外的成员
        let rule = shape.members.iter().find(|(known, _)| known == name).map(|(_, rule)| rule).ok_or_else(|| path.clone())?;
        value_at(rule, value, path)?;
        path.truncate(parent);
    }
    for required in shape.required {
        if !members.iter().any(|(name, _)| name == required) {
            push_member(path, required);
            return Err(path.clone());
        }
    }
    Ok(())
}

fn value_at(rule: &Rule, value: &Node, path: &mut String) -> Result<(), String> {
    let fits = match (rule, value) {
        // 可选成员缺失表示没有，写成 null 不合法，数组项同样
        (_, Node::Null) => false,
        (Rule::Text(pattern), Node::String(text)) => pattern(text),
        (Rule::Bool, Node::Bool(_)) => true,
        (Rule::Name(names), Node::String(text)) => names.contains(&text.as_str()),
        (Rule::List { item, min, unique }, Node::Array(items)) => return list(item, *min, *unique, items, path),
        (Rule::Object(shape), Node::Object(members)) => return object(shape, members, path),
        _ => false,
    };
    if fits { Ok(()) } else { Err(path.clone()) }
}

fn list(item: &Rule, min: usize, unique: bool, items: &[Node], path: &mut String) -> Result<(), String> {
    if items.len() < min {
        return Err(path.clone());
    }
    let parent = path.len();
    for (index, value) in items.iter().enumerate() {
        push_index(path, index);
        value_at(item, value, path)?;
        if unique && items[..index].contains(value) {
            return Err(path.clone());
        }
        path.truncate(parent);
    }
    Ok(())
}

/// 清单 ID `^xihan\.[a-z][a-z0-9]*\.[a-z][a-z0-9-]*$`。
pub(super) fn is_manifest_id(id: &str) -> bool {
    id.strip_prefix("xihan.").is_some_and(is_capability_id)
}

/// 能力 ID `^[a-z][a-z0-9]*\.[a-z][a-z0-9-]*$`。
pub(super) fn is_capability_id(id: &str) -> bool {
    id.split_once('.').is_some_and(|(domain, capability)| word(domain, lower, lower_digit) && word(capability, lower, lower_digit_hyphen))
}

/// 接口版本 `^[1-9][0-9]*$`，不超过 2147483647。
fn is_version(text: &str) -> bool {
    positive(text).is_some()
}

/// 依赖的版本 `^(\^)?[1-9][0-9]*$`，不超过 2147483647。
fn is_version_range(text: &str) -> bool {
    is_version(text.strip_prefix('^').unwrap_or(text))
}

/// 语义化版本 `^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$`（与 Schema 一致，允许前导零）。
pub(super) fn is_semver(version: &str) -> bool {
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

/// 激活事件 `^(onStartup|onOperation:[a-z0-9.*-]+|onEvent:[a-z0-9._*-]+)$`（RFC-0002 解析规则 2 第 3 步，`plugin/lifecycle.md` §3）。
///
/// `onOperation:` 的模式是操作 ID 的字符加 `*`，`onEvent:` 的模式是主题的字符加 `*`：主题可以带下划线（RFC-0009），
/// 所以只有 `onEvent:` 的模式收下划线（ADR-0276）。
pub(super) fn is_activation_event(event: &str) -> bool {
    let operations = |text: &str| all(text, |byte| lower_digit(byte) || matches!(byte, b'.' | b'*' | b'-'));
    let topics = |text: &str| all(text, |byte| lower_digit(byte) || matches!(byte, b'.' | b'_' | b'*' | b'-'));
    event == super::ACTIVATION_ON_STARTUP || event.strip_prefix("onOperation:").is_some_and(operations) || event.strip_prefix("onEvent:").is_some_and(topics)
}

/// 插件权限 `^(net\.outbound|fs\.read|fs\.write|capability):.+$`；`.` 不匹配换行。
pub(super) fn is_permission(permission: &str) -> bool {
    permission
        .split_once(':')
        .is_some_and(|(scope, target)| matches!(scope, "net.outbound" | "fs.read" | "fs.write" | "capability") && !target.is_empty() && !target.contains('\n'))
}

/// 绑定名 `^[a-z][a-z0-9-]*$`。
fn is_binding_name(name: &str) -> bool {
    word(name, lower, lower_digit_hyphen)
}

/// 资源类型 `^[a-z]+(:[a-z]+)?$`。
pub(super) fn is_binding_type(resource_type: &str) -> bool {
    match resource_type.split_once(':') {
        Some((kind, detail)) => all(kind, lower) && all(detail, lower),
        None => all(resource_type, lower),
    }
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

/// 健康项 `^[a-z][a-z0-9.-]*$`。
fn is_health_name(name: &str) -> bool {
    word(name, lower, |byte| lower_digit_hyphen(byte) || byte == b'.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammars_match_the_schema() {
        for version in ["1.0.0", "5.0.0-preview.1", "1.2.3+build.7", "1.2.3-rc-1+a.b", "01.2.3"] {
            assert!(is_semver(version), "{version}");
        }
        for version in ["", "1.0", "1.0.0.0", "1.0.0-", "1.0.0+", "v1.0.0", "1.0.0-é", "1..0", "1.0.0\n"] {
            assert!(!is_semver(version), "{version}");
        }
        // 只有 onEvent: 的模式收下划线：主题有这个字符，操作 ID 没有
        for event in ["onStartup", "onOperation:orders.*", "onEvent:a-b.c", "onEvent:order_events.*", "onEvent:_", "onEvent:*"] {
            assert!(is_activation_event(event), "{event}");
        }
        for event in [
            "",
            "onBoot",
            "onOperation:",
            "onEvent:",
            "onEvent:Orders",
            "onEvent:Order_Placed",
            "onOperation:order_lines.*",
            "onEvent:order events",
            "onEvent:order_events\n",
            "onStartup:x",
            "onStartup\n",
        ] {
            assert!(!is_activation_event(event), "{event:?}");
        }
        for permission in ["net.outbound:api.example.com", "fs.read:/tmp", "capability:kernel.pipeline", "fs.write::", "fs.read:a\rb"] {
            assert!(is_permission(permission), "{permission}");
        }
        for permission in ["sudo", "capability:", "net.inbound:x", "fs.read:a\nb", "fs.read:a\n"] {
            assert!(!is_permission(permission), "{permission}");
        }
        for resource_type in ["queue", "connection:relational"] {
            assert!(is_binding_type(resource_type), "{resource_type}");
        }
        for resource_type in ["", "a:", ":a", "a:b:c", "Queue", "cache-x"] {
            assert!(!is_binding_type(resource_type), "{resource_type}");
        }
        for version in ["1", "2147483647"] {
            assert!(is_version(version) && is_version_range(&format!("^{version}")), "{version}");
        }
        for version in ["0", "01", "2147483648", "^1", "+1", " 1", "1\n"] {
            assert!(!is_version(version), "{version}");
        }
        assert!(!is_version_range("^^1") && !is_version_range("^01"));
        assert!(is_health_name("orders.core.store") && !is_health_name("Bad") && !is_health_name(""));
    }
}
