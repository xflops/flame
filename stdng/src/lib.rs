/*
Copyright 2025 The Flame Authors.
Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at
    http://www.apache.org/licenses/LICENSE-2.0
Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::sync::Arc;

use thiserror::Error;

pub mod collections;
pub mod logs;
pub mod rand;
pub mod tls;

#[derive(Error, Debug)]
pub enum Error {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Internal(String),

    #[error("{0}")]
    Network(String),
}

/// Shared synchronous mutex. It does not poison, so locking cannot fail; do not
/// hold the guard across an `.await` (use [`AsyncPtr`] for that).
pub type MutexPtr<T> = Arc<parking_lot::Mutex<T>>;

pub fn new_ptr<T>(t: T) -> MutexPtr<T> {
    Arc::new(parking_lot::Mutex::new(t))
}

/// Lock a [`MutexPtr`]. The `Result` is kept so call sites can keep using `?`.
#[macro_export]
macro_rules! lock_ptr {
    ( $mutex_arc:expr ) => {
        ::std::result::Result::<_, $crate::Error>::Ok($mutex_arc.lock())
    };
}

/// Shared asynchronous mutex, whose guard may be held across an `.await`.
pub type AsyncPtr<T> = Arc<tokio::sync::Mutex<T>>;

pub fn async_ptr<T>(t: T) -> AsyncPtr<T> {
    Arc::new(tokio::sync::Mutex::new(t))
}

/// Await the lock of an [`AsyncPtr`].
#[macro_export]
macro_rules! await_ptr {
    ( $async_arc:expr ) => {
        $async_arc.lock().await
    };
}

/// Shared synchronous read/write lock. It does not poison, so locking cannot
/// fail; do not hold a guard across an `.await` (use [`AsyncRWPtr`] for that).
pub type RWPtr<T> = Arc<parking_lot::RwLock<T>>;

pub fn rw_ptr<T>(t: T) -> RWPtr<T> {
    Arc::new(parking_lot::RwLock::new(t))
}

/// Read-lock a [`RWPtr`]. The `Result` is kept so call sites can use `?`.
#[macro_export]
macro_rules! read_ptr {
    ( $rw_arc:expr ) => {
        ::std::result::Result::<_, $crate::Error>::Ok($rw_arc.read())
    };
}

/// Write-lock a [`RWPtr`]. The `Result` is kept so call sites can use `?`.
#[macro_export]
macro_rules! write_ptr {
    ( $rw_arc:expr ) => {
        ::std::result::Result::<_, $crate::Error>::Ok($rw_arc.write())
    };
}

/// Shared asynchronous read/write lock, whose guards may be held across an `.await`.
pub type AsyncRWPtr<T> = Arc<tokio::sync::RwLock<T>>;

pub fn async_rw_ptr<T>(t: T) -> AsyncRWPtr<T> {
    Arc::new(tokio::sync::RwLock::new(t))
}

/// Await the read lock of an [`AsyncRWPtr`].
#[macro_export]
macro_rules! read_async {
    ( $async_rw_arc:expr ) => {
        $async_rw_arc.read().await
    };
}

/// Await the write lock of an [`AsyncRWPtr`].
#[macro_export]
macro_rules! write_async {
    ( $async_rw_arc:expr ) => {
        $async_rw_arc.write().await
    };
}

/// Evaluates to `Ok(())` if `cond` holds, else to `Err(err)`; `err` is only evaluated
/// on failure. The caller adds `?`.
#[macro_export]
macro_rules! ensure {
    ($cond:expr, $err:expr $(,)?) => {
        if $cond { Ok(()) } else { Err($err) }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_is_ok_when_true_and_skips_the_error() {
        let evaluated = std::cell::Cell::new(false);
        let result: Result<(), String> = ensure!(true, {
            evaluated.set(true);
            "unused".to_string()
        });
        assert_eq!(result, Ok(()));
        assert!(!evaluated.get());
    }

    #[test]
    fn ensure_is_the_given_error_when_false() {
        let result: Result<(), String> = ensure!(1 > 2, "boom".to_string());
        assert_eq!(result, Err("boom".to_string()));
    }

    #[test]
    fn mutex_ptr_locks_and_shares() {
        let ptr = new_ptr(1);
        let other = ptr.clone();
        *lock_ptr!(ptr).unwrap() += 1;
        assert_eq!(*lock_ptr!(other).unwrap(), 2);
    }

    #[test]
    fn rw_ptr_allows_shared_reads_and_exclusive_writes() {
        let ptr = rw_ptr(vec![1]);
        {
            let first = read_ptr!(ptr).unwrap();
            let second = read_ptr!(ptr).unwrap();
            assert_eq!(first.len() + second.len(), 2);
        }
        write_ptr!(ptr).unwrap().push(2);
        assert_eq!(read_ptr!(ptr).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn async_ptrs_can_be_held_across_await() {
        let mutex = async_ptr(0);
        {
            let mut guard = await_ptr!(mutex);
            tokio::task::yield_now().await;
            *guard += 1;
        }
        assert_eq!(*await_ptr!(mutex), 1);

        let rw = async_rw_ptr(String::from("a"));
        {
            let mut guard = write_async!(rw);
            tokio::task::yield_now().await;
            guard.push('b');
        }
        assert_eq!(*read_async!(rw), "ab");
    }
}
