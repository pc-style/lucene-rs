//! Thread-local recycling of large per-query scratch buffers, so a query does not pay for
//! allocating and zeroing them. (Lucene allocates these per query.)

use std::cell::RefCell;
use std::ops::{Deref, DerefMut};

const MAX_POOLED: usize = 64;

pub(crate) trait Recycle: Default + Sized + 'static {
    fn with_pool<R>(f: impl FnOnce(&mut Vec<Box<Self>>) -> R) -> Option<R>;
}

macro_rules! recyclable {
    ($t:ty) => {
        impl $crate::pool::Recycle for $t {
            fn with_pool<R>(f: impl FnOnce(&mut Vec<Box<Self>>) -> R) -> Option<R> {
                thread_local!(static POOL: std::cell::RefCell<Vec<Box<$t>>> = const { std::cell::RefCell::new(Vec::new()) });
                POOL.try_with(|p| f(&mut p.borrow_mut())).ok()
            }
        }
    };
}
pub(crate) use recyclable;

/// A boxed `T` taken from (and returned to) the current thread's pool. Contents are whatever
/// the previous user left: types must restore any invariants they rely on before release.
pub(crate) struct Pooled<T: Recycle>(Option<Box<T>>);

impl<T: Recycle> Pooled<T> {
    pub fn take() -> Self {
        Pooled(Some(T::with_pool(|p| p.pop()).flatten().unwrap_or_default()))
    }
}

impl<T: Recycle> Deref for Pooled<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T {
        self.0.as_ref().unwrap()
    }
}

impl<T: Recycle> DerefMut for Pooled<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        self.0.as_mut().unwrap()
    }
}

impl<T: Recycle> Drop for Pooled<T> {
    fn drop(&mut self) {
        if let Some(b) = self.0.take() {
            T::with_pool(|p| {
                if p.len() < MAX_POOLED {
                    p.push(b)
                }
            });
        }
    }
}

// Silence "unused" for RefCell when no type is registered in a given build.
#[allow(dead_code)]
type _Unused = RefCell<()>;
