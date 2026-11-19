// AuthRefreshBackoff 的逐行移植（authRefreshBackoff.ts）。时钟和随机数可注入，便于金样复现。
pub struct Backoff {
    epoch: i64,
    retry_at: f64,
    failures: u32,
    network_failure: bool,
    quick_retry_at: Option<f64>,
    quick_failures: u32,
    quick_not_before: f64,
    quick_in_flight: bool,
    now: Box<dyn Fn() -> f64 + Send>,
    random: Box<dyn Fn() -> f64 + Send>,
}

impl Backoff {
    pub fn new(now: Box<dyn Fn() -> f64 + Send>, random: Box<dyn Fn() -> f64 + Send>) -> Self {
        Backoff { epoch: -1, retry_at: 0.0, failures: 0, network_failure: false, quick_retry_at: None,
            quick_failures: 0, quick_not_before: 0.0, quick_in_flight: false, now, random }
    }

    pub fn system() -> Self {
        Self::new(
            Box::new(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)),
            Box::new(|| {
                let mut b = [0u8; 4];
                let _ = getrandom::fill(&mut b);
                u32::from_le_bytes(b) as f64 / 4294967296.0
            }),
        )
    }

    pub fn remaining(&self, epoch: i64) -> f64 {
        if self.epoch == epoch { (self.retry_at - (self.now)()).max(0.0) } else { 0.0 }
    }

    pub fn server_rejected_token(&mut self, epoch: i64) -> bool {
        if self.epoch != epoch || !self.network_failure || self.quick_retry_at.is_some() {
            return false;
        }
        let candidate = self.quick_not_before.max((self.now)() + ((self.random)() * 3000.0).floor());
        if self.quick_in_flight {
            self.quick_retry_at = Some(candidate);
            return false;
        }
        if candidate >= self.retry_at {
            return false;
        }
        self.quick_retry_at = Some(candidate);
        true
    }

    pub fn start_runtime_attempt(&mut self, epoch: i64) {
        if self.epoch != epoch {
            return;
        }
        self.quick_in_flight = self.quick_retry_at.is_some();
        self.quick_retry_at = None;
    }

    pub fn runtime_remaining(&self, epoch: i64) -> f64 {
        if self.epoch != epoch {
            return 0.0;
        }
        match self.quick_retry_at {
            None => self.remaining(epoch),
            Some(q) => (q - (self.now)()).max(0.0),
        }
    }

    pub fn defer(&mut self, epoch: i64, server_retry_at: Option<f64>, network_failure: bool) -> f64 {
        if self.epoch != epoch {
            self.clear();
        }
        self.network_failure = network_failure && server_retry_at.is_none();
        let was_quick = self.quick_in_flight;
        self.quick_in_flight = false;
        if self.network_failure && was_quick {
            let floor = (3000.0 * 2f64.powi(self.quick_failures as i32)).min(60000.0) / 2.0;
            self.quick_failures = (self.quick_failures + 1).min(5);
            self.quick_not_before = (self.now)() + floor + ((self.random)() * floor).floor();
            if let Some(q) = self.quick_retry_at {
                let candidate = self.quick_not_before.max(q);
                self.quick_retry_at = if candidate < self.retry_at { Some(candidate) } else { None };
            }
            return self.runtime_remaining(epoch);
        }
        self.quick_retry_at = None;
        if !self.network_failure {
            self.quick_failures = 0;
            self.quick_not_before = 0.0;
        }
        self.epoch = epoch;
        let floor = (60000.0 * 2f64.powi(self.failures as i32)).min(300000.0);
        self.failures = (self.failures + 1).min(3);
        let server = server_retry_at.filter(|v| v.is_finite()).unwrap_or(0.0);
        self.retry_at = ((self.now)() + floor).max(server) + ((self.random)() * floor).floor();
        self.remaining(epoch)
    }

    pub fn release(&mut self, epoch: i64) {
        if self.epoch == epoch {
            self.retry_at = 0.0;
        }
    }

    pub fn clear(&mut self) {
        self.epoch = -1;
        self.retry_at = 0.0;
        self.failures = 0;
        self.network_failure = false;
        self.quick_retry_at = None;
        self.quick_failures = 0;
        self.quick_not_before = 0.0;
        self.quick_in_flight = false;
    }
}
