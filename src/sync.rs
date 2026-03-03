#[allow(unused_imports)]
#[cfg(feature = "shuttle")]
pub(crate) use shuttle::sync::atomic::{
    AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};

#[allow(unused_imports)]
#[cfg(not(feature = "shuttle"))]
pub(crate) use core::sync::atomic::{
    AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering,
};
