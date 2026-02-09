//! A custom read write lock safe implementation

use std::{
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicBool, Ordering},
        PoisonError, RwLock as InnerRwLock, RwLockReadGuard, RwLockWriteGuard,
    },
};

/// A thin wrapper around [`std::sync::RwLock`] with an explicit locking policy.
///
/// This type exists to provide clearer, more ergonomic locking APIs while
/// preserving the semantics of `std::sync::RwLock`.
///
/// Higher-level methods on this type distinguish between:
/// - Scoped, closure-based access, which prevents lock guards from escaping
/// - Explicit guard-based access, for advanced use cases that require flexible control flow
#[derive(Debug)]
pub struct RwLock<T: ?Sized>(InnerRwLock<T>);

impl<T> RwLock<T> {
    /// Creates a new `RwLock` protecting `value`.
    pub fn new(value: T) -> Self {
        Self(InnerRwLock::new(value))
    }
}

impl<T: ?Sized> RwLock<T> {
    /// Executes `f` while holding a read lock.
    ///
    /// The lock guard cannot escape this method.
    /// Prefer this over [`read`] for small, self-contained operations.
    pub fn safe_read<F, R>(&self, f: F) -> Result<R, PoisonError<RwLockReadGuard<'_, T>>>
    where
        F: FnOnce(&T) -> R,
    {
        let guard = self.0.read()?;
        Ok(f(&*guard))
    }

    /// Executes `f` while holding a write lock.
    ///
    /// The lock guard cannot escape this method.
    /// Poisoning is propagated to the caller.
    pub fn safe_write<F, R>(&self, f: F) -> Result<R, PoisonError<RwLockWriteGuard<'_, T>>>
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut guard = self.0.write()?;
        Ok(f(&mut *guard))
    }

    /// Acquires a read lock and returns the guard directly.
    ///
    /// This is an API intended for complex control flow where
    /// closure-based locking would harm readability.
    pub fn read(&self) -> Result<ReadGuard<'_, T>, PoisonError<RwLockReadGuard<'_, T>>> {
        let guard = self.0.read()?;
        Ok(ReadGuard {
            guard,
            released: AtomicBool::new(false),
        })
    }

    /// Acquires a write lock and returns the guard directly.
    ///
    /// Callers are responsible for keeping the
    /// guard scope small and avoiding `.await` while holding it.
    pub fn write(&self) -> Result<WriteGuard<'_, T>, PoisonError<RwLockWriteGuard<'_, T>>> {
        let guard = self.0.write()?;
        Ok(WriteGuard {
            guard,
            released: AtomicBool::new(false),
        })
    }
}

/// A read lock guard returned by [`RwLock::read`].
///
/// This guard **must be explicitly released** by calling [`ReadGuard::release`].
pub struct ReadGuard<'a, T: ?Sized> {
    guard: RwLockReadGuard<'a, T>,
    released: AtomicBool,
}

impl<T: ?Sized> ReadGuard<'_, T> {
    /// Explicitly releases the read lock.
    ///
    /// failing to call this before the guard is dropped
    /// will cause a panic.
    pub fn release(self) {
        self.released.store(true, Ordering::Release);
    }
}

impl<T: ?Sized> Deref for ReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T: ?Sized> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        if !self.released.load(Ordering::Acquire) {
            panic!(
                "ReadGuard dropped without explicit release(); \
                 this is a bug. Call release() to acknowledge lock lifetime."
            );
        }
    }
}

/// A write lock guard returned by [`RwLock::write`].
///
/// This guard **must be explicitly released** by calling [`WriteGuard::release`].
pub struct WriteGuard<'a, T: ?Sized> {
    guard: RwLockWriteGuard<'a, T>,
    released: AtomicBool,
}

impl<T: ?Sized> WriteGuard<'_, T> {
    /// Explicitly releases the write lock.
    ///
    /// failing to call this before the guard is dropped
    /// will cause a panic.
    pub fn release(self) {
        self.released.store(true, Ordering::Release);
    }
}

impl<T: ?Sized> Deref for WriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T: ?Sized> DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl<T: ?Sized> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        if !self.released.load(Ordering::Acquire) {
            panic!(
                "WriteGuard dropped without explicit release(); \
                 this is a bug. Call release() to acknowledge lock lifetime."
            );
        }
    }
}
