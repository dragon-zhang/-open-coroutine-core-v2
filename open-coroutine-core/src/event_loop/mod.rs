use crate::coroutine::suspender::Suspender;
use crate::event_loop::event::Events;
use crate::event_loop::interest::Interest;
use crate::event_loop::join::JoinHandle;
use crate::event_loop::selector::Selector;
use crate::scheduler::{SchedulableCoroutine, Scheduler};
use once_cell::sync::{Lazy, OnceCell};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

pub mod join;

pub mod event;

pub mod interest;

mod selector;

/// 做C兼容时会用到
pub type UserFunc =
    extern "C" fn(*const Suspender<(), ()>, &'static mut c_void) -> &'static mut c_void;

#[derive(Debug, Copy, Clone)]
pub struct EventLoops {}

static mut INDEX: Lazy<AtomicUsize> = Lazy::new(|| AtomicUsize::new(0));

static mut EVENT_LOOPS: Lazy<Box<[EventLoop]>> = Lazy::new(|| {
    (0..num_cpus::get())
        .map(|_| EventLoop::new().expect("init event loop failed!"))
        .collect()
});

static EVENT_LOOP_WORKERS: OnceCell<Box<[std::thread::JoinHandle<()>]>> = OnceCell::new();

static EVENT_LOOP_STARTED: Lazy<AtomicBool> = Lazy::new(AtomicBool::default);

impl EventLoops {
    fn next() -> &'static mut EventLoop {
        unsafe {
            let index = INDEX.fetch_add(1, Ordering::SeqCst);
            if index == usize::MAX {
                INDEX.store(1, Ordering::SeqCst);
            }
            EVENT_LOOPS.get_mut(index % EVENT_LOOPS.len()).unwrap()
        }
    }

    pub(crate) fn monitor() -> &'static mut EventLoop {
        unsafe {
            let index = INDEX.fetch_add(1, Ordering::SeqCst);
            if index == usize::MAX {
                INDEX.store(1, Ordering::SeqCst);
            }
            //monitor线程的EventLoop固定
            EVENT_LOOPS.get_mut(0).unwrap()
        }
    }

    pub fn start() {
        if EVENT_LOOP_STARTED
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            //初始化event_loop线程
            _ = EVENT_LOOP_WORKERS.get_or_init(|| {
                (1..unsafe { EVENT_LOOPS.len() })
                    .map(|_| {
                        std::thread::spawn(|| {
                            let event_loop = EventLoops::next();
                            while EVENT_LOOP_STARTED.load(Ordering::Acquire) {
                                _ = event_loop.wait_event(Some(Duration::from_millis(10)));
                            }
                        })
                    })
                    .collect()
            });
        }
    }

    pub fn stop() {
        #[cfg(all(unix, feature = "preemptive-schedule"))]
        crate::monitor::Monitor::stop();
        EVENT_LOOP_STARTED.store(false, Ordering::Release);
    }

    pub fn submit(
        f: impl FnOnce(&Suspender<'_, (), ()>, ()) -> &'static mut c_void + 'static,
        stack_size: Option<usize>,
    ) -> std::io::Result<JoinHandle> {
        EventLoops::start();
        EventLoops::next().submit(f, stack_size)
    }

    pub fn try_timeout_schedule(timeout_time: u64) -> std::io::Result<u64> {
        EventLoops::next().try_timeout_schedule(timeout_time)
    }

    pub fn wait_event(timeout: Option<Duration>) -> std::io::Result<()> {
        let timeout_time = open_coroutine_timer::get_timeout_time(timeout.unwrap_or(Duration::MAX));
        let event_loop = EventLoops::next();
        loop {
            let left_time = timeout_time
                .saturating_sub(open_coroutine_timer::now())
                .min(10_000_000);
            if left_time == 0 {
                //timeout
                return Ok(());
            }
            event_loop.wait_event(Some(Duration::from_nanos(left_time)))?;
        }
    }

    pub fn wait_read_event(fd: libc::c_int, timeout: Option<Duration>) -> std::io::Result<()> {
        EventLoops::next().wait_read_event(fd, timeout)
    }

    pub fn wait_write_event(fd: libc::c_int, timeout: Option<Duration>) -> std::io::Result<()> {
        EventLoops::next().wait_write_event(fd, timeout)
    }

    pub fn del_event(fd: libc::c_int) {
        (0..unsafe { EVENT_LOOPS.len() }).for_each(|_| {
            _ = EventLoops::next().del_event(fd);
        });
    }

    pub fn del_read_event(fd: libc::c_int) {
        (0..unsafe { EVENT_LOOPS.len() }).for_each(|_| {
            _ = EventLoops::next().del_read_event(fd);
        });
    }

    pub fn del_write_event(fd: libc::c_int) {
        (0..unsafe { EVENT_LOOPS.len() }).for_each(|_| {
            _ = EventLoops::next().del_write_event(fd);
        });
    }
}

#[derive(Debug)]
pub struct EventLoop {
    selector: Selector,
    scheduler: Scheduler,
    waiting: AtomicBool,
}

static mut READABLE_RECORDS: Lazy<HashSet<libc::c_int>> = Lazy::new(HashSet::new);

static mut READABLE_TOKEN_RECORDS: Lazy<HashMap<libc::c_int, usize>> = Lazy::new(HashMap::new);

static mut WRITABLE_RECORDS: Lazy<HashSet<libc::c_int>> = Lazy::new(HashSet::new);

static mut WRITABLE_TOKEN_RECORDS: Lazy<HashMap<libc::c_int, usize>> = Lazy::new(HashMap::new);

impl EventLoop {
    pub fn new() -> std::io::Result<Self> {
        Ok(EventLoop {
            selector: Selector::new()?,
            scheduler: Scheduler::new(),
            waiting: AtomicBool::new(false),
        })
    }

    pub fn submit(
        &self,
        f: impl FnOnce(&Suspender<'_, (), ()>, ()) -> &'static mut c_void + 'static,
        stack_size: Option<usize>,
    ) -> std::io::Result<JoinHandle> {
        self.scheduler
            .submit(f, stack_size)
            .map(|co_name| JoinHandle::new(self, co_name))
    }

    pub fn try_timeout_schedule(&self, timeout_time: u64) -> std::io::Result<u64> {
        _ = self.scheduler.try_timeout_schedule(timeout_time);
        self.wait_just(Some(Duration::ZERO))?;
        Ok(timeout_time.saturating_sub(open_coroutine_timer::now()))
    }

    #[allow(clippy::ptr_as_ptr)]
    fn token() -> usize {
        if let Some(co) = SchedulableCoroutine::current() {
            let co_name: &'static String = Box::leak(Box::from(String::from(co.get_name())));
            co_name as *const String as *const _ as *const c_void as usize
        } else {
            0
        }
    }

    pub fn add_read_event(&self, fd: libc::c_int) -> std::io::Result<()> {
        unsafe {
            if READABLE_TOKEN_RECORDS.contains_key(&fd) {
                return Ok(());
            }
        }
        let token = EventLoop::token();
        self.selector.register(fd, token, Interest::READABLE)?;
        unsafe {
            assert!(READABLE_RECORDS.insert(fd));
            assert_eq!(None, READABLE_TOKEN_RECORDS.insert(fd, token));
        }
        Ok(())
    }

    pub fn add_write_event(&self, fd: libc::c_int) -> std::io::Result<()> {
        unsafe {
            if WRITABLE_TOKEN_RECORDS.contains_key(&fd) {
                return Ok(());
            }
        }
        let token = EventLoop::token();
        self.selector.register(fd, token, Interest::WRITABLE)?;
        unsafe {
            assert!(WRITABLE_RECORDS.insert(fd));
            assert_eq!(None, WRITABLE_TOKEN_RECORDS.insert(fd, token));
        }
        Ok(())
    }

    pub fn del_event(&mut self, fd: libc::c_int) -> std::io::Result<()> {
        self.selector.deregister(fd)?;
        unsafe {
            _ = READABLE_RECORDS.remove(&fd);
            _ = READABLE_TOKEN_RECORDS.remove(&fd);
            _ = WRITABLE_RECORDS.remove(&fd);
            _ = WRITABLE_TOKEN_RECORDS.remove(&fd);
        }
        Ok(())
    }

    pub fn del_read_event(&mut self, fd: libc::c_int) -> std::io::Result<()> {
        unsafe {
            if READABLE_RECORDS.contains(&fd) {
                if WRITABLE_RECORDS.contains(&fd) {
                    //写事件不能删
                    self.selector.reregister(
                        fd,
                        WRITABLE_TOKEN_RECORDS.remove(&fd).unwrap_or(0),
                        Interest::WRITABLE,
                    )?;
                    assert!(READABLE_RECORDS.remove(&fd));
                } else {
                    self.del_event(fd)?;
                }
            }
        }
        Ok(())
    }

    pub fn del_write_event(&mut self, fd: libc::c_int) -> std::io::Result<()> {
        unsafe {
            if WRITABLE_RECORDS.contains(&fd) {
                if READABLE_RECORDS.contains(&fd) {
                    //读事件不能删
                    self.selector.reregister(
                        fd,
                        READABLE_TOKEN_RECORDS.remove(&fd).unwrap_or(0),
                        Interest::READABLE,
                    )?;
                    assert!(WRITABLE_RECORDS.remove(&fd));
                } else {
                    self.del_event(fd)?;
                }
            }
        }
        Ok(())
    }

    pub fn wait_just(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.wait(timeout, false)
    }

    pub fn wait_event(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.wait(timeout, true)
    }

    fn wait(&self, timeout: Option<Duration>, schedule_before_wait: bool) -> std::io::Result<()> {
        if self
            .waiting
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return Ok(());
        }
        let timeout = if schedule_before_wait {
            timeout.map(|time| Duration::from_nanos(self.scheduler.try_timed_schedule(time)))
        } else {
            timeout
        };
        let mut events = Events::with_capacity(1024);
        self.selector.select(&mut events, timeout).map_err(|e| {
            self.waiting.store(false, Ordering::Relaxed);
            e
        })?;
        self.waiting.store(false, Ordering::Relaxed);
        for event in events.iter() {
            let fd = event.fd();
            let token = event.token();
            self.scheduler.resume_syscall(token);
            unsafe {
                if event.is_readable() {
                    assert!(READABLE_TOKEN_RECORDS.remove(&fd).is_some());
                }
                if event.is_writable() {
                    assert!(WRITABLE_TOKEN_RECORDS.remove(&fd).is_some());
                }
            }
        }
        Ok(())
    }

    pub fn wait_read_event(
        &self,
        fd: libc::c_int,
        timeout: Option<Duration>,
    ) -> std::io::Result<()> {
        self.add_read_event(fd)?;
        self.wait_event(timeout)
    }

    pub fn wait_write_event(
        &self,
        fd: libc::c_int,
        timeout: Option<Duration>,
    ) -> std::io::Result<()> {
        self.add_write_event(fd)?;
        self.wait_event(timeout)
    }
}