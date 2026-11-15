use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 类型化标识（RFC-0001 TypedId）：`Tag` 只用于区分类型，不承载数据；线上编码为底层值的编码。
///
/// ```
/// use xihan_framework::primitives::TypedId;
///
/// struct Order;
/// type OrderId = TypedId<Order, String>;
///
/// let id = OrderId::new("o1".to_owned());
/// assert_eq!(serde_json::to_string(&id).unwrap(), r#""o1""#);
/// ```
pub struct TypedId<Tag, V> {
    value: V,
    // fn() -> Tag 让标识的 Send、Sync 与变型只取决于底层值，与标签类型无关
    tag: PhantomData<fn() -> Tag>,
}

impl<Tag, V> TypedId<Tag, V> {
    /// 用底层值创建。
    pub const fn new(value: V) -> Self {
        Self { value, tag: PhantomData }
    }

    /// 底层值。
    pub const fn value(&self) -> &V {
        &self.value
    }

    /// 取出底层值。
    pub fn into_value(self) -> V {
        self.value
    }
}

// 以下实现只约束底层值：标签类型不必实现任何 trait

impl<Tag, V: Clone> Clone for TypedId<Tag, V> {
    fn clone(&self) -> Self {
        Self::new(self.value.clone())
    }
}

impl<Tag, V: Copy> Copy for TypedId<Tag, V> {}

impl<Tag, V: PartialEq> PartialEq for TypedId<Tag, V> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<Tag, V: Eq> Eq for TypedId<Tag, V> {}

impl<Tag, V: PartialOrd> PartialOrd for TypedId<Tag, V> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.value.partial_cmp(&other.value)
    }
}

impl<Tag, V: Ord> Ord for TypedId<Tag, V> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.value.cmp(&other.value)
    }
}

impl<Tag, V: Hash> Hash for TypedId<Tag, V> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl<Tag, V: fmt::Debug> fmt::Debug for TypedId<Tag, V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("TypedId").field(&self.value).finish()
    }
}

impl<Tag, V: fmt::Display> fmt::Display for TypedId<Tag, V> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.fmt(formatter)
    }
}

impl<Tag, V: Serialize> Serialize for TypedId<Tag, V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de, Tag, V: Deserialize<'de>> Deserialize<'de> for TypedId<Tag, V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        V::deserialize(deserializer).map(Self::new)
    }
}
