use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub struct MediaQueue<T> {
    capacity: usize,
    state: Mutex<(VecDeque<T>, bool)>,
    ready: Condvar,
}

impl<T> MediaQueue<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            state: Mutex::new((VecDeque::new(), false)),
            ready: Condvar::new(),
        }
    }

    pub fn push(&self, value: T) -> Option<T> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.1 {
            return Some(value);
        }
        let dropped = if state.0.len() == self.capacity {
            state.0.pop_front()
        } else {
            None
        };
        state.0.push_back(value);
        self.ready.notify_one();
        dropped
    }

    pub fn pop_timeout(&self, timeout: Duration) -> Option<T> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (mut state, _) = self
            .ready
            .wait_timeout_while(state, timeout, |state| state.0.is_empty() && !state.1)
            .unwrap_or_else(|e| e.into_inner());
        state.0.pop_front()
    }

    pub fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.1 = true;
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capacity_and_close_are_bounded() {
        let queue = MediaQueue::new(2);
        assert_eq!(queue.push(1), None);
        queue.push(2);
        assert_eq!(queue.push(3), Some(1));
        queue.close();
        assert_eq!(queue.push(4), Some(4));
        assert_eq!(queue.pop_timeout(Duration::ZERO), Some(2));
        assert_eq!(queue.pop_timeout(Duration::ZERO), Some(3));
        assert_eq!(queue.pop_timeout(Duration::from_secs(10)), None);
    }
}
