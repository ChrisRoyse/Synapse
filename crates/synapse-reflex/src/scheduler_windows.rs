//! Windows wake source for the reflex scheduler tick.
//!
//! # Why this is a plain blocking wait
//!
//! This module used to do three things at once: raise the scheduler thread to
//! `THREAD_PRIORITY_TIME_CRITICAL`, register it with MMCSS as `Pro Audio` at
//! `AVRT_PRIORITY_CRITICAL`, and then busy-spin `std::hint::spin_loop()` for the
//! final millisecond before every deadline. With `target_interval` at 1 ms
//! (`SchedulerConfig::default`) the spin window equalled the whole interval, so
//! `timer_wait` was *always* zero: the high-resolution waitable timer below was
//! constructed, armed once, and then never waited on. Every microsecond between
//! ticks was burned by the spin instead.
//!
//! MMCSS `Pro Audio` at `AVRT_PRIORITY_CRITICAL` promotes a thread into the
//! realtime scheduling range, above the threads that carry mouse and keyboard
//! input and above DWM. A thread that spins there does not merely consume a
//! core — it consumes a core at a priority the input stack cannot preempt, for
//! as long as any reflex is scheduled. The operator-visible result is the
//! machine feeling seized and the pointer stuttering whenever a reflex runs,
//! which is exactly what the scheduler was reported doing.
//!
//! `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (Windows 10 1803 and later) is the
//! supported mechanism for sub-millisecond wakes and needs neither a priority
//! escalation nor a spin: it resolves to roughly 0.5 ms without either. It was
//! already being created here; it is now actually used, for the full remaining
//! interval, from an ordinary-priority thread.
//!
//! The tradeoff is deliberate and stated: wake jitter is now whatever the
//! kernel timer delivers rather than whatever a realtime spin can pin down, so
//! p99 tick jitter may sit above the `REFERENCE_REFLEX_TICK_JITTER_IDLE_P99_US`
//! reference figure on some hosts. Jitter is measured and published
//! (`reflex_tick_jitter_us`, and `p99_tick_jitter_us` in health), lateness is
//! already classified by `scheduler_tick`, and no gate fails on it. Trading
//! bounded, observable jitter for an operator who can move their mouse is the
//! correct direction, and a tighter wake must never again be bought with a
//! realtime spin.

use std::time::{Duration, Instant};

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        System::Threading::{
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE,
            SetWaitableTimerEx, TIMER_ALL_ACCESS, WaitForSingleObject,
        },
    },
    core::PCWSTR,
};

pub struct WindowsHighResolutionTimer {
    timer: HANDLE,
}

impl WindowsHighResolutionTimer {
    pub fn start(target_interval: Duration) -> Result<Self, String> {
        // SAFETY: Null security attributes/name create a private unnamed timer.
        // The returned handle is owned by this guard and closed in Drop.
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .map_err(|error| format!("CreateWaitableTimerExW high-resolution failed: {error}"))?;

        // Arm once at startup so a host that cannot honour the requested
        // interval fails here, on the constructor, rather than on the first
        // tick. `wait_until` re-arms per wait.
        if let Err(error) = arm_timer(timer, target_interval) {
            // SAFETY: `timer` is the handle created above and is owned here on
            // this path; nothing else can observe it.
            let _ = unsafe { CloseHandle(timer) };
            return Err(error);
        }

        Ok(Self { timer })
    }

    pub fn wait_until(&self, deadline: Instant) -> Result<(), String> {
        let wait = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        // Already at or past the deadline: the tick is late and runs now. This
        // is the same immediate return the old spin gave on a missed deadline.
        if wait.is_zero() {
            return Ok(());
        }
        arm_timer(self.timer, wait)?;
        // SAFETY: self.timer is a live waitable timer handle owned by this guard.
        let result = unsafe { WaitForSingleObject(self.timer, INFINITE) };
        if result != WAIT_OBJECT_0 {
            return Err(format!(
                "WaitForSingleObject on scheduler timer returned {result:?}"
            ));
        }
        Ok(())
    }
}

impl Drop for WindowsHighResolutionTimer {
    fn drop(&mut self) {
        // SAFETY: the handle was acquired by this guard and is dropped once.
        let _ = unsafe { CloseHandle(self.timer) };
    }
}

fn duration_100ns(duration: Duration) -> i64 {
    let ticks = duration.as_nanos() / 100;
    i64::try_from(ticks).unwrap_or(i64::MAX)
}

fn arm_timer(timer: HANDLE, duration: Duration) -> Result<(), String> {
    let due_time = -duration_100ns(duration);
    // SAFETY: timer is a valid waitable timer handle, due_time points to a
    // live i64 for the duration of the call, and no APC callback is used.
    unsafe { SetWaitableTimerEx(timer, &raw const due_time, 0, None, None, None, 0) }
        .map_err(|error| format!("SetWaitableTimerEx one-shot failed: {error}"))
}
