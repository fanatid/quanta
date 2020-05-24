use crate::Clock;
use std::{
    fmt, io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

static GLOBAL_UPKEEP_RUNNING: AtomicBool = AtomicBool::new(false);

/// Ultra-low-overhead access to slightly-delayed time.
///
/// In some applications, there can be a need to check the current time very often, so much so that
/// the overhead of checking the time can begin to eat up measurable overhead. For some of these
/// cases, the time may need to be accessed often but does not necessarily need to be incredibly
/// accurate: one millisecond granularity could be entirely acceptable.
///
/// For these cases, we provide a slightly-delayed version of the time to callers via
/// [`Clock::recent`], which is updated by a background upkeep thread.  That thread is configured
/// and spanwed via [`Upkeep`].
///
/// [`Upkeep`] can construct a new clock (or be passed an existing clock to use), and given an
/// update interval, and it will faithfully attempt to update the global recent time on the
/// specified interval.  There is a trade-off to be struck in terms of how often the time is
/// updated versus the required accuracy.  Checking the time and updating the global reference is
/// itself not zero-cost, and so care must be taken to analyze the number of readers in order to
/// ensure that, given a particular update interval, the upkeep thread is saving more CPU time than
/// would be spent otherwise by directly querying the current time.
///
/// The recent time is read and written atomically.  It is global to an application, so if another
/// codepath creates the upkeep thread, the interval chosen by that instantiation will be the one
/// that all callers of [`Clock::recent`] end up using.
///
/// Multiple upkeep threads cannot exist at the same time.  A new upkeep thread can be started if
/// the old one is dropped and returns.
///
/// In terms of performance, reading the recent time can be up to two to three times as fast as
/// reading the current time in the optimized case of using the Time Stamp Counter source.  In
/// practice, while a caller might expect to take 12-14ns to read the TSC and scale it to reference
/// time, the recent time can be read in 4-5ns, with no reference scale conversion required.
#[derive(Debug)]
pub struct Upkeep {
    interval: Duration,
    clock: Clock,
}

/// Handle to a running upkeep thread.
///
/// If a handle is dropped, the upkeep thread will be stopped, and the recent time will cease to
/// update.  The upkeep thread can be started again to resume updating the recent time.
#[derive(Debug)]
pub struct Handle {
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// Errors thrown during the creation/spawning of the upkeep thread.
#[derive(Debug)]
pub enum Error {
    /// An upkeep thread is already running in this process.
    UpkeepRunning,
    /// An error occurred when trying to spawn the upkeep thread.
    FailedToSpawnUpkeepThread(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::UpkeepRunning => write!(f, "upkeep thread already running"),
            Error::FailedToSpawnUpkeepThread(e) => {
                write!(f, "failed to spanw upkeep thread: {}", e)
            }
        }
    }
}

impl Upkeep {
    /// Creates a new [`Upkeep`].
    ///
    /// This creates a new internal clock for acquiring the current time.  If you have an existing
    /// [`Clock`] that is already calibrated, it is slightly faster to clone it and construct the
    /// builder with [`new_with_clock`](Upkeep::new_with_clock) to avoid recalibrating.
    pub fn new(interval: Duration) -> Upkeep {
        Self::new_with_clock(interval, Clock::new())
    }

    /// Creates a new [`Upkeep`] with the specified [`Clock`] instance.
    pub fn new_with_clock(interval: Duration, clock: Clock) -> Upkeep {
        Upkeep { interval, clock }
    }

    /// Start the upkeep thread, periodically updating the global coarse time.
    ///
    /// If the return value is [`Ok(handle)`], then the thread was spawned successfully and can be
    /// stopped by dropping the returned handle.  Otherwise, [`Err`] contains the error that was
    /// returned when trying to spawn the thread.
    pub fn start(self) -> Result<Handle, Error> {
        // If another upkeep thread is running, inform the caller.
        if !GLOBAL_UPKEEP_RUNNING.compare_and_swap(false, true, Ordering::SeqCst) {
            return Err(Error::UpkeepRunning);
        }

        let interval = self.interval;
        let mut clock = self.clock;

        let done = Arc::new(AtomicBool::new(false));
        let their_done = done.clone();

        let handle = thread::Builder::new()
            .name("quanta-upkeep".to_string())
            .spawn(move || {
                while !their_done.load(Ordering::Acquire) {
                    let now = clock.now();
                    Clock::upkeep(now);

                    thread::sleep(interval);
                }

                GLOBAL_UPKEEP_RUNNING.store(false, Ordering::SeqCst);
            })
            .map_err(Error::FailedToSpawnUpkeepThread)?;

        Ok(Handle {
            done,
            handle: Some(handle),
        })
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);

        if let Some(handle) = self.handle.take() {
            let _ = handle
                .join()
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "failed to stop upkeep thread"));
        }
    }
}
