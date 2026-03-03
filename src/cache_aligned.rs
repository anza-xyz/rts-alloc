pub type CacheAlignedU16 = CacheAligned<crate::sync::AtomicU16>;
pub type CacheAlignedU32 = CacheAligned<crate::sync::AtomicU32>;

#[repr(C, align(64))]
pub struct CacheAligned<T>(pub T);

impl<T> core::ops::Deref for CacheAligned<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
