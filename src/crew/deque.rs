//! A crew that uses the lock-free Chase-Lev deque implemented in `crossbeam`.
//!
//! Each worker runs this loop in a separate thread:
//!
//!   1. Try to steal a job
//!   2. If successful, perform it, and become "hot"
//!   3. If shutting down, break
//!   4. If lost a race or no jobs, "cool down"
//!   5. Possibly yield or sleep
//!
//! When a hot worker "cools down," it becomes "warm", with a retry count of
//! zero; an already-warm worker increases the retry count. Eventually, the
//! retry threshold may be exceeded, and the worker becomes "cold".
//!
//! The temperature determines the action at step five: a hot worker immediately
//! continues the loop; a warm one cooperatively yields to another thread; and
//! cold ones sleep. The goal is to allow workers to progress unhindered as long
//! as there are jobs, but reduce excessive CPU usage during periods when there
//! are little to none.
//!
//! A cold worker may also become hot again if it loses a race to steal a job,
//! since this strongly indicates that another job is ready, but remains cold
//! as long as it finds the queue empty.

use std::default::Default;
use std::thread::{sleep, yield_now};
use std::time::Duration;

#[cfg(feature = "nightly")]
use std::panic::recover;

use crossbeam::sync::chase_lev;
use crossbeam::sync::chase_lev::Steal::{Data, Abort, Empty};

use num_cpus;

use {Job, Message};
use super::{Crew, Parameters, Worker};

enum Load {
    Hot,
    Warm,
    Cold,
}

/// The `Crew` `Member` for `Deque`.
pub struct DequeWorker<J> {
    #[cfg_attr(not(feature = "nightly"), allow(dead_code))]
    id: usize,
    load: Load,
    retries: u32,
    options: Options,
    stealer: chase_lev::Stealer<Message<J>>,
}

impl<J: Job> DequeWorker<J> {
    // the worker just successfully acquired an item
    // this version uses `recover` to handle panics from jobs
    #[cfg(feature = "nightly")]
    #[cfg_attr(feature = "clippy",
               allow(boxed_local))]
    #[inline]
    fn does(&mut self, job: J) {
        recover(|| {
            job.perform();
        })
            .map_err(|e| error!("worker #{}: job panicked: {:?}", self.id, e))
            .ok();
        self.load = Load::Hot;
    }

    #[cfg(not(feature = "nightly"))]
    // the worker just successfully acquired an item
    // this version propogates panics from job
    #[inline]
    fn does(&mut self, job: J) {
        job.perform();
        self.load = Load::Hot;
    }

    // the worker just lost a race to acquire an item
    #[inline]
    fn missed(&mut self) {
        if self.options.retry_threshold == 0 {
            self.load = Load::Cold;
            return;
        }
        match self.load {
            Load::Hot => self.become_warm(),
            Load::Warm => self.become_cooler(),
            Load::Cold => self.load = Load::Hot,
        }
    }

    // the worker just found an empty work queue
    #[inline]
    fn nothing(&mut self) {
        if self.options.retry_threshold == 0 {
            self.load = Load::Cold;
            return;
        }
        match self.load {
            Load::Hot => self.become_warm(),
            Load::Warm => self.become_cooler(),
            Load::Cold => {}
        }
    }

    // continue, yield, or sleep based on the load
    #[inline]
    fn wait(&self) {
        match self.load {
            Load::Hot => {}
            Load::Warm => yield_now(),
            Load::Cold => sleep(self.options.cold_interval),
        }
    }

    #[inline]
    fn become_warm(&mut self) {
        self.load = Load::Warm;
        self.retries = 0;
    }

    #[inline]
    fn become_cooler(&mut self) {
        self.retries += 1;
        // exceeded threshold, become cold
        if self.retries >= self.options.retry_threshold {
            self.load = Load::Cold;
        }
    }
}

impl<J: Job> Worker for DequeWorker<J> {
    fn run(&mut self) {
        loop {
            match self.stealer.steal() {
                Data(Message::Work(job)) => self.does(job),
                Data(Message::Stop) => break,
                Abort => self.missed(),
                Empty => self.nothing(),
            }
            self.wait();
        }
    }
}

/// Parameters to adjust the size and behavior of the crew.
///
/// The current defaults for retry threshold and cold interval--32 and 1ms--were
/// chosen arbitrarily. Experimentation may be prudent.
#[derive(Copy, Clone)]
pub struct Options {
    /// How many times may a worker fail to acquire a job before it becomes
    /// "cold" and sleeps for `cold_interval` between subsequent attempts.
    pub retry_threshold: u32,
    /// The minimum length of time a worker will sleep when it is cold.
    pub cold_interval: Duration,
    /// The number of workers to create in the pool.
    pub num_workers: usize,
}

impl Parameters for Options {
    fn num_workers(&self) -> usize {
        self.num_workers
    }
}

impl Default for Options {
    fn default() -> Options {
        Options {
            retry_threshold: 32,
            cold_interval: Duration::from_millis(1),
            num_workers: num_cpus::get(),
        }
    }
}

/// A `Crew` that uses a lock-free Chase-Lev deque.
pub struct Deque<J> {
    next_id: usize,
    options: Options,
    sender: chase_lev::Worker<Message<J>>,
    stealer: chase_lev::Stealer<Message<J>>,
}

impl<J: Job> Crew for Deque<J> {
    type Job = J;
    type Member = DequeWorker<J>;
    type Settings = Options;

    fn new(options: Options) -> Deque<J> {
        let (sender, stealer) = chase_lev::deque();
        Deque {
            next_id: 0,
            options: options,
            sender: sender,
            stealer: stealer,
        }
    }

    fn hire(&mut self) -> DequeWorker<J> {
        let id = self.next_id;
        self.next_id += 1;
        DequeWorker {
            id: id,
            load: Load::Hot,
            retries: 0,
            options: self.options,
            stealer: self.stealer.clone(),
        }
    }

    fn give<F>(&mut self, f: F)
        where J: From<F>
    {
        self.sender.push(Message::Work(J::from(f)));
    }

    fn stop(&mut self) {
        for _ in 0..self.options.num_workers {
            self.sender.push(Message::Stop);
        }
    }
}
