use std::any::Any;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

/// Runs session discovery off the UI thread. At most one refresh is in flight;
/// watcher triggers that arrive meanwhile collapse into one follow-up refresh.
pub struct Refresher<Req, Res> {
    tx: Sender<Req>,
    rx: Receiver<Result<Res, String>>,
    started: Option<Instant>,
    retrigger: bool,
    last_done: Instant,
    interval: Duration,
    debounce: Duration,
}

#[derive(Debug)]
pub struct WorkerGone(pub String);

impl<Req: Send + 'static, Res: Send + 'static> Refresher<Req, Res> {
    pub fn spawn(
        interval: Duration,
        debounce: Duration,
        mut work: impl FnMut(Req) -> Res + Send + 'static,
    ) -> Self {
        let (tx, worker_rx) = mpsc::channel::<Req>();
        let (worker_tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(req) = worker_rx.recv() {
                let res = panic::catch_unwind(AssertUnwindSafe(|| work(req)))
                    .map_err(|payload| panic_message(payload.as_ref()));
                let failed = res.is_err();
                if worker_tx.send(res).is_err() || failed {
                    break;
                }
            }
        });
        Self {
            tx,
            rx,
            started: None,
            retrigger: false,
            last_done: Instant::now(),
            interval,
            debounce,
        }
    }

    pub fn in_flight(&self) -> bool {
        self.started.is_some()
    }

    pub fn running_for(&self, now: Instant) -> Option<Duration> {
        self.started
            .map(|started| now.saturating_duration_since(started))
    }

    pub fn start(&mut self, now: Instant, req: Req) -> Result<(), WorkerGone> {
        self.tx.send(req).map_err(|_| gone())?;
        self.started = Some(now);
        self.retrigger = false;
        Ok(())
    }

    /// Starts a refresh when the interval has elapsed, or when a change was
    /// seen and the debounce has elapsed, unless one is already running.
    pub fn poll_start(
        &mut self,
        now: Instant,
        triggered: bool,
        req: impl FnOnce() -> Req,
    ) -> Result<bool, WorkerGone> {
        self.retrigger |= triggered;
        if self.in_flight() {
            return Ok(false);
        }
        let elapsed = now.saturating_duration_since(self.last_done);
        if elapsed < self.interval && !(self.retrigger && elapsed >= self.debounce) {
            return Ok(false);
        }
        self.start(now, req())?;
        Ok(true)
    }

    pub fn try_finish(&mut self, now: Instant) -> Result<Option<Res>, WorkerGone> {
        match self.rx.try_recv() {
            Ok(res) => self.finished(now, res).map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(gone()),
        }
    }

    pub fn wait(&mut self) -> Result<Res, WorkerGone> {
        let res = self.rx.recv().map_err(|_| gone())?;
        self.finished(Instant::now(), res)
    }

    fn finished(&mut self, now: Instant, res: Result<Res, String>) -> Result<Res, WorkerGone> {
        self.started = None;
        self.last_done = now;
        res.map_err(WorkerGone)
    }
}

fn gone() -> WorkerGone {
    WorkerGone("worker exited".to_string())
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown cause".to_string());
    format!("panicked: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERVAL: Duration = Duration::from_secs(2);
    const DEBOUNCE: Duration = Duration::from_millis(400);

    fn gated() -> (Refresher<u32, u32>, Sender<()>) {
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let refresher = Refresher::spawn(INTERVAL, DEBOUNCE, move |n| {
            gate_rx.recv().unwrap();
            n
        });
        (refresher, gate_tx)
    }

    fn finish(refresher: &mut Refresher<u32, u32>, gate: &Sender<()>, now: Instant) -> u32 {
        gate.send(()).unwrap();
        loop {
            if let Some(n) = refresher.try_finish(now).unwrap() {
                return n;
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_running_refresh_does_not_block_the_caller() {
        let (mut refresher, gate) = gated();
        let t0 = Instant::now();
        assert!(refresher.poll_start(t0 + INTERVAL, false, || 1).unwrap());
        assert!(refresher.try_finish(Instant::now()).unwrap().is_none());
        assert!(refresher.in_flight());
        assert_eq!(finish(&mut refresher, &gate, t0), 1);
        assert!(!refresher.in_flight());
    }

    #[test]
    fn triggers_during_a_refresh_coalesce_into_one_follow_up() {
        let (mut refresher, gate) = gated();
        let t0 = Instant::now();
        assert!(refresher.poll_start(t0, true, || 1).is_ok_and(|s| !s));
        assert!(refresher.poll_start(t0 + DEBOUNCE, false, || 1).unwrap());
        for _ in 0..5 {
            assert!(!refresher.poll_start(t0 + INTERVAL * 3, true, || 2).unwrap());
        }
        let done = t0 + DEBOUNCE * 2;
        assert_eq!(finish(&mut refresher, &gate, done), 1);

        assert!(!refresher.poll_start(done, false, || 3).unwrap());
        assert!(refresher.poll_start(done + DEBOUNCE, false, || 3).unwrap());
        assert_eq!(finish(&mut refresher, &gate, done + DEBOUNCE), 3);

        let idle = done + DEBOUNCE * 2;
        assert!(!refresher.poll_start(idle, false, || 4).unwrap());
    }

    #[test]
    fn an_idle_refresher_runs_on_the_interval() {
        let (mut refresher, gate) = gated();
        let t0 = Instant::now();
        assert!(
            !refresher
                .poll_start(t0 + INTERVAL / 2, false, || 1)
                .unwrap()
        );
        assert!(refresher.poll_start(t0 + INTERVAL, false, || 1).unwrap());
        assert_eq!(finish(&mut refresher, &gate, t0 + INTERVAL), 1);
    }

    #[test]
    fn running_time_is_reported_until_the_refresh_finishes() {
        let (mut refresher, gate) = gated();
        let t0 = Instant::now();
        assert_eq!(refresher.running_for(t0), None);
        refresher.start(t0, 1).unwrap();
        assert_eq!(refresher.running_for(t0 + INTERVAL * 5), Some(INTERVAL * 5));
        finish(&mut refresher, &gate, t0 + INTERVAL * 5);
        assert_eq!(refresher.running_for(t0 + INTERVAL * 6), None);
    }

    #[test]
    fn a_dead_worker_reports_why_it_died() {
        let mut refresher: Refresher<u32, u32> =
            Refresher::spawn(INTERVAL, DEBOUNCE, |_| panic!("discovery failed"));
        refresher.start(Instant::now(), 1).unwrap();
        let WorkerGone(reason) = refresher.wait().unwrap_err();
        assert!(reason.contains("discovery failed"), "{reason}");
    }
}
