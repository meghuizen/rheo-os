//! The parsed JSON value tree. Strings borrow from the input when they have
//! no escapes (`Cow::Borrowed`), which is the zero-copy path the seal/grant
//! philosophy favours (ARCHITECTURE.md 0): a filled buffer read by many
//! without copies. Objects keep insertion order (a `Vec` of pairs).

use alloc::borrow::Cow;
use alloc::vec::Vec;

/// A JSON number, kept in the narrowest exact form the source implied so an
/// integer round-trips without going through `f64`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Number {
    /// A non-negative integer that fits in `u64`.
    Unsigned(u64),
    /// A negative integer that fits in `i64`.
    Signed(i64),
    /// Anything with a fraction/exponent, or an integer too big for the above.
    Float(f64),
}

impl Number {
    pub fn as_f64(&self) -> f64 {
        match *self {
            Number::Unsigned(u) => u as f64,
            Number::Signed(i) => i as f64,
            Number::Float(f) => f,
        }
    }

    /// The value as `i64` when it is an integer that fits, else `None`.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Number::Unsigned(u) => i64::try_from(u).ok(),
            Number::Signed(i) => Some(i),
            Number::Float(_) => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Number::Unsigned(u) => Some(u),
            _ => None,
        }
    }
}

/// A parsed JSON value borrowing from the input for the lifetime `'a`.
#[derive(Clone, PartialEq, Debug)]
pub enum Value<'a> {
    Null,
    Bool(bool),
    Number(Number),
    String(Cow<'a, str>),
    Array(Vec<Value<'a>>),
    /// Object members in source order.
    Object(Vec<(Cow<'a, str>, Value<'a>)>),
}

impl<'a> Value<'a> {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<&Number> {
        match self {
            Value::Number(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        self.as_number().map(Number::as_f64)
    }

    pub fn as_i64(&self) -> Option<i64> {
        self.as_number().and_then(Number::as_i64)
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_number().and_then(Number::as_u64)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value<'a>]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    /// The member named `key` of an object, or `None`.
    pub fn get(&self, key: &str) -> Option<&Value<'a>> {
        match self {
            Value::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The `i`-th element of an array, or `None`.
    pub fn at(&self, i: usize) -> Option<&Value<'a>> {
        self.as_array().and_then(|a| a.get(i))
    }
}
