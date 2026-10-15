use android_activity::AndroidAppWaker;
use gpui::{
    PlatformDispatcher, Priority, PriorityQueueReceiver, PriorityQueueSender, RunnableVariant,
};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const MIN_BACKGROUND_THREADS: usize = 2;

pub(crate) struct AndroidDispatcher {
    main_sender: PriorityQueueSender<RunnableVariant>,
    waker: AndroidAppWaker,
    background_sender: parking_lot::Mutex<Option<PriorityQueueSender<RunnableVariant>>>,
    timers: Arc<TimerQueue>,
    main_thread_id: thread::ThreadId,
    _background_threads: Vec<thread::JoinHandle<()>>,
}

// Safety: AndroidAppWaker wraps an ALooper wake fd that is explicitly designed
// to be triggered from any thread; all other fields are Send + Sync by
// construction.
unsafe impl Send for AndroidDispatcher {}
unsafe impl Sync for AndroidDispatcher {}

impl AndroidDispatcher {
    pub fn new(main_sender: PriorityQueueSender<RunnableVariant>, waker: AndroidAppWaker) -> Self {
        let (background_sender, background_receiver) = PriorityQueueReceiver::new();
        let thread_count = std::thread::available_parallelism()
            .map_or(MIN_BACKGROUND_THREADS, |i| {
                i.get().max(MIN_BACKGROUND_THREADS)
            });

        let mut background_threads = (0..thread_count)
            .map(|i| {
                let receiver: PriorityQueueReceiver<RunnableVariant> = background_receiver.clone();
                std::thread::Builder::new()
                    .name(format!("Worker-{i}"))
                    .spawn(move || {
                        for runnable in receiver.iter() {
                            runnable.run();
                        }
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();

        let timers = Arc::new(TimerQueue::default());
        let timer_queue = Arc::clone(&timers);
        let timer_thread = std::thread::Builder::new()
            .name("Timer".to_owned())
            .spawn(move || timer_queue.run())
            .unwrap();
        background_threads.push(timer_thread);

        Self {
            main_sender,
            waker,
            background_sender: parking_lot::Mutex::new(Some(background_sender)),
            timers,
            main_thread_id: thread::current().id(),
            _background_threads: background_threads,
        }
    }

    pub(crate) fn shutdown(&self) {
        self.timers.shutdown();
        let sender = self.background_sender.lock().take();
        drop(sender);
    }
}

impl Drop for AndroidDispatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl PlatformDispatcher for AndroidDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread_id
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        let rejected = {
            let sender = self.background_sender.lock();
            match sender.as_ref() {
                Some(sender) => match sender.send(priority, runnable) {
                    Ok(()) => None,
                    Err(error) => {
                        log::error!("Android background queue disconnected; cancelling task");
                        Some(error.0)
                    }
                },
                None => Some(runnable),
            }
        };
        drop(rejected);
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        match self.main_sender.send(priority, runnable) {
            Ok(()) => self.waker.wake(),
            Err(runnable) => {
                // NOTE: Runnable may wrap a Future that is !Send.
                //
                // This is usually safe because we only poll it on the main thread.
                // However if the send fails, we know that:
                // 1. the main receiver has been dropped (the app is shutting down)
                // 2. we may be on a background thread.
                // It is not safe to drop something !Send on the wrong thread, and
                // the app will exit soon anyway, so we must forget the runnable.
                std::mem::forget(runnable);
            }
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        self.timers.push(Instant::now() + duration, runnable);
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(move || {
            // SAFETY: always safe to call
            let thread_id = unsafe { libc::pthread_self() };

            let policy = libc::SCHED_FIFO;
            let sched_priority = 65;

            // SAFETY: all sched_param members are valid when initialized to zero.
            let mut sched_param =
                unsafe { MaybeUninit::<libc::sched_param>::zeroed().assume_init() };
            sched_param.sched_priority = sched_priority;
            // SAFETY: sched_param is a valid initialized structure
            let result = unsafe { libc::pthread_setschedparam(thread_id, policy, &sched_param) };
            if result != 0 {
                log::warn!("failed to set realtime thread priority");
            }

            f();
        });
    }
}

#[derive(Default)]
struct TimerQueue {
    state: parking_lot::Mutex<TimerQueueState>,
    condvar: parking_lot::Condvar,
    next_sequence: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct TimerQueueState {
    heap: BinaryHeap<TimerEntry>,
    stopped: bool,
}

impl TimerQueue {
    fn push(&self, deadline: Instant, runnable: RunnableVariant) {
        let sequence = self
            .next_sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut state = self.state.lock();
        if state.stopped {
            drop(state);
            return;
        }
        state.heap.push(TimerEntry {
            deadline,
            sequence,
            runnable,
        });
        self.condvar.notify_one();
    }

    fn shutdown(&self) {
        let pending = {
            let mut state = self.state.lock();
            state.stopped = true;
            std::mem::take(&mut state.heap)
        };
        self.condvar.notify_all();
        // Dropping tasks can dispatch more work, so do it outside the lock.
        drop(pending);
    }

    fn run(&self) {
        let mut state = self.state.lock();
        loop {
            if state.stopped {
                return;
            }
            let now = Instant::now();
            let mut due = Vec::new();
            while state.heap.peek().is_some_and(|entry| entry.deadline <= now) {
                if let Some(entry) = state.heap.pop() {
                    due.push(entry);
                }
            }
            if !due.is_empty() {
                drop(state);
                for entry in due {
                    entry.runnable.run();
                }
                state = self.state.lock();
                continue;
            }
            match state.heap.peek().map(|entry| entry.deadline) {
                Some(deadline) => {
                    self.condvar.wait_until(&mut state, deadline);
                }
                None => self.condvar.wait(&mut state),
            }
        }
    }
}

struct TimerEntry {
    deadline: Instant,
    sequence: u64,
    runnable: RunnableVariant,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.sequence == other.sequence
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; reverse so the earliest deadline is popped first.
        other
            .deadline
            .cmp(&self.deadline)
            .then(other.sequence.cmp(&self.sequence))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_stops_an_idle_timer_thread() {
        let timers = Arc::new(TimerQueue::default());
        let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
        let thread = {
            let timers = timers.clone();
            thread::spawn(move || {
                timers.run();
                finished_sender.send(()).expect("test receiver is alive");
            })
        };
        timers.shutdown();
        finished_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("timer thread should exit during shutdown");
        thread.join().expect("timer thread should not panic");
    }
}
