mod domain;
mod hazard;
mod pointers;
mod retire;
mod thread;

pub use hazard::HazardPointer;
pub use thread::set_counts_between_flush;

use std::thread_local;

use domain::Domain;
pub use pointers::Tagged;
pub use thread::Thread;

pub static DEFAULT_DOMAIN: Domain = Domain::new();

thread_local! {
    pub static DEFAULT_THREAD: Box<Thread> = Box::new(Thread::new(&DEFAULT_DOMAIN));
}

#[inline]
pub fn with_thread<F, R>(f: F) -> R
where
    F: FnOnce(&Thread) -> R,
{
    DEFAULT_THREAD.with(|t| f(&**t))
}
