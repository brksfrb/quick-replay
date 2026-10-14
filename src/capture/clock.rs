//! One shared timeline for audio and video, in 100 ns units (the unit Media Foundation and
//! WASAPI timestamps use), counted from when the pipeline started.

use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

pub const HNS_PER_SEC: i64 = 10_000_000;

#[derive(Clone, Copy, Debug)]
pub struct Clock {
    frequency: i64,
    epoch_hns: i64,
}

impl Clock {
    pub fn start() -> Clock {
        let mut frequency = 0i64;
        let _ = unsafe { QueryPerformanceFrequency(&mut frequency) };
        let mut clock = Clock { frequency: frequency.max(1), epoch_hns: 0 };
        clock.epoch_hns = clock.absolute_hns();
        clock
    }

    /// Same conversion WASAPI uses for its QPC positions.
    fn absolute_hns(&self) -> i64 {
        let mut counter = 0i64;
        let _ = unsafe { QueryPerformanceCounter(&mut counter) };
        (i128::from(counter) * i128::from(HNS_PER_SEC) / i128::from(self.frequency)) as i64
    }

    /// Time since the pipeline started.
    pub fn now(&self) -> i64 {
        self.absolute_hns() - self.epoch_hns
    }

    /// Converts an absolute QPC time in 100 ns units (as reported by WASAPI) to pipeline time.
    pub fn qpc_to_pipeline(&self, qpc_hns: u64) -> i64 {
        qpc_hns as i64 - self.epoch_hns
    }
}
