//! A turn-scoped inbox. Hosts retain durable pending items until input is applied.
use futures::{future::poll_fn, task::AtomicWaker};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub share: bool,
    pub id: String,
    pub text: String,
}
#[derive(Debug)]
struct Inbox {
    queue: Mutex<VecDeque<Input>>,
    waker: AtomicWaker,
    sharing: std::sync::atomic::AtomicBool,
}
impl Default for Inbox {
    fn default() -> Self {
        Self {
            queue: Mutex::default(),
            waker: AtomicWaker::new(),
            sharing: std::sync::atomic::AtomicBool::new(true),
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct Inputs(Arc<Inbox>);
impl Inputs {
    pub fn push(&self, input: Input) {
        if !input.share {
            self.0
                .sharing
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        self.0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(input);
        self.0.waker.wake();
    }
    pub fn drain(&self) -> Vec<Input> {
        self.0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect()
    }
    #[must_use]
    pub fn sharing(&self) -> bool {
        self.0.sharing.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn pending(&self) -> bool {
        !self
            .0
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }
    pub async fn next(&self) -> Input {
        poll_fn(|cx| {
            self.0.waker.register(cx.waker());
            self.0
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .map_or(std::task::Poll::Pending, std::task::Poll::Ready)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn turning_sharing_off_cannot_be_reenabled_by_another_update_in_the_same_turn() {
        let inputs = Inputs::default();
        assert!(inputs.sharing());
        inputs.push(Input {
            id: "a".into(),
            text: "private".into(),
            share: false,
        });
        inputs.drain();
        inputs.push(Input {
            id: "b".into(),
            text: "still private".into(),
            share: true,
        });
        assert!(!inputs.sharing());
    }
}
