use std::io;
use std::mem;
use std::ptr::{self, NonNull};
use std::task::Waker;

use parking_lot::Mutex;

use crate::event::Cancellation;

pub struct Completion {
    state: NonNull<Mutex<State>>,
}

unsafe impl Send for Completion { }
unsafe impl Sync for Completion { }

enum State {
    Submitted(Waker),
    Completed(i32),
    Cancelled(Cancellation),
}

impl Completion {
    pub(crate) fn new(waker: Waker) -> Completion {
        unsafe {
            let state = Box::new(Mutex::new(State::Submitted(waker)));
            Completion {
                state: NonNull::new_unchecked(Box::into_raw(state)),
            }
        }
    }

    pub(crate) fn dangling() -> Completion {
        Completion {
            state: NonNull::dangling(),
        }
    }

    pub(crate) unsafe fn deallocate(&self) {
        drop(Box::from_raw(self.state.as_ptr()));
    }

    pub(crate) fn addr(&self) -> u64 {
        self.state.as_ptr() as usize as u64
    }

    pub(crate) unsafe fn set_waker(&self, waker: Waker) {
        let mut state = self.state.as_ref().lock();
        if let State::Submitted(slot) = &mut *state {
            *slot = waker;
        }
    }

    pub(crate) unsafe fn cancel(&self, mut callback: Cancellation) {
        let mut state = self.state.as_ref().lock();
        if matches!(&*state, State::Completed(_)) {
            self.deallocate();
            callback.cancel();
        } else {
            *state = State::Cancelled(callback);
        }
    }

    pub(crate) unsafe fn check(&self) -> Option<io::Result<usize>> {
        let state = self.state.as_ref().lock();
        match *state {
            State::Completed(result)    => {
                match result >= 0 {
                    true    => Some(Ok(result as usize)),
                    false   => Some(Err(io::Error::from_raw_os_error(-result))),
                }
            }
            _                           => None,
        }
    }
}

pub unsafe fn complete(cqe: iou::CompletionQueueEvent) {
    if cqe.is_timeout() { return; }

    let completion = cqe.user_data() as *mut Mutex<State>;

    if completion != ptr::null_mut() {
        let mut state = (*completion).lock();
        match mem::replace(&mut *state, State::Completed(cqe.raw_result())) {
            State::Submitted(waker)         => waker.wake(),
            State::Cancelled(mut callback)  => {
                drop(Box::from_raw(completion));
                callback.cancel();
            }
            State::Completed(_)         => panic!()
        }
    }
}
