use std::collections::HashSet;
use std::sync::{Condvar, Mutex};

use crate::grid::Grid;
use crate::queue::{Key, WaitQueue};

/// The four stages vanilla leaves on the serial worldgen thread
/// (ChunkPyramid.java, 26.3-pre-1). Values are stable across the JNI
/// boundary; never reorder.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Stage {
    StructureStarts = 0,
    StructureReferences = 1,
    Features = 2,
    Spawn = 3,
}

impl Stage {
    pub fn from_u8(v: u8) -> Option<Stage> {
        match v {
            0 => Some(Stage::StructureStarts),
            1 => Some(Stage::StructureReferences),
            2 => Some(Stage::Features),
            3 => Some(Stage::Spawn),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Job {
    pub stage: Stage,
    pub cx: i32,
    pub cz: i32,
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Max FEATURES jobs running at once. Vanilla's pool is
    /// clamp(cores - 1, 1, 255) (Util.java:236); we take a slice of it.
    pub max_features: usize,
    /// Max jobs of any stage running at once (memory bound: each holds a
    /// 3x3 of ProtoChunks).
    pub max_inflight: usize,
}

impl Limits {
    pub fn for_cores(cores: usize) -> Limits {
        let features = cores.saturating_sub(1).clamp(1, 16);
        Limits { max_features: features, max_inflight: features * 2 }
    }
}

/// Ticket handed back by `acquire`; pass it to `release`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ticket {
    job: Job,
    seq: u64,
}

impl Ticket {
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Rebuild a ticket from the parts that crossed the JNI boundary.
    pub fn from_parts(job: Job, seq: u64) -> Ticket {
        Ticket { job, seq }
    }
}

/// Result of a non-blocking `submit`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Submitted {
    pub seq: u64,
    pub granted: bool,
}

#[derive(Default)]
struct State {
    grid: Grid,
    waiting: WaitQueue,
    /// Seqs of blocking `acquire` callers, so grants for them go to
    /// `granted` and the condvar instead of the caller's return value.
    blocking: HashSet<u64>,
    granted: HashSet<u64>,
    next_seq: u64,
    /// Grant order since creation, for tests and the probe.
    grant_log: Vec<u64>,
}

pub struct Scheduler {
    limits: Limits,
    state: Mutex<State>,
    wake: Condvar,
}

impl Scheduler {
    pub fn new(limits: Limits) -> Scheduler {
        assert!(limits.max_features >= 1 && limits.max_inflight >= 1);
        Scheduler { limits, state: Mutex::new(State::default()), wake: Condvar::new() }
    }

    /// Block until the job may run. `level` is the chunk's ticket level;
    /// lower runs first.
    pub fn acquire(&self, job: Job, level: i32) -> Ticket {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let seq = st.next_seq;
        st.next_seq += 1;
        let key = Key { level, seq };
        st.waiting.push(key, job);
        st.blocking.insert(seq);
        self.grant_pass(&mut st);
        while !st.granted.contains(&seq) {
            st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.granted.remove(&seq);
        Ticket { job, seq }
    }

    /// Register the job and return whether it may run now. If not, it
    /// stays queued and its seq comes back from a later `release`.
    pub fn submit(&self, job: Job, level: i32) -> Submitted {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let seq = st.next_seq;
        st.next_seq += 1;
        st.waiting.push(Key { level, seq }, job);
        let granted = self.grant_pass(&mut st).contains(&seq);
        Submitted { seq, granted }
    }

    /// Non-blocking variant: Some if granted now, None if it would wait
    /// (and nothing is queued).
    pub fn try_acquire(&self, job: Job, level: i32) -> Option<Ticket> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let seq = st.next_seq;
        st.next_seq += 1;
        let key = Key { level, seq };
        st.waiting.push(key, job);
        if self.grant_pass(&mut st).contains(&seq) {
            Some(Ticket { job, seq })
        } else {
            st.waiting.remove(&key);
            None
        }
    }

    /// Free the job's zone. Returns the seqs of submitted jobs that may
    /// run now; blocking acquirers are woken instead of listed.
    pub fn release(&self, t: Ticket) -> Vec<u64> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let removed = match t.job.stage {
            Stage::Features => st.grid.remove_features(t.job.cx, t.job.cz),
            s => st.grid.remove_center(s as u8, t.job.cx, t.job.cz),
        };
        debug_assert!(removed, "release without acquire: {:?}", t.job);
        let granted = self.grant_pass(&mut st);
        self.wake.notify_all();
        granted
    }

    /// Greedy scan in priority order; admit every waiter that is
    /// conflict-free against the active set plus earlier admits this pass.
    /// Returns the seqs admitted for non-blocking submitters.
    fn grant_pass(&self, st: &mut State) -> Vec<u64> {
        let mut admit: Vec<Key> = Vec::new();
        let mut features = st.grid.active_features();
        let mut total = st.grid.active_total();
        // Borrow split: scan waiting read-only, mutate grid via a shadow
        // copy of the decisions, then apply.
        let mut shadow_features: Vec<(i32, i32)> = Vec::new();
        let mut shadow_center: Vec<(u8, i32, i32)> = Vec::new();
        for (key, job) in st.waiting.iter() {
            if total >= self.limits.max_inflight {
                break;
            }
            let ok = match job.stage {
                Stage::Features => {
                    features < self.limits.max_features
                        && st.grid.features_free(job.cx, job.cz)
                        && shadow_features
                            .iter()
                            .all(|&(ax, az)| (ax - job.cx).abs().max((az - job.cz).abs()) > crate::grid::FEATURE_EXCLUSION)
                }
                s => {
                    st.grid.center_free(s as u8, job.cx, job.cz)
                        && !shadow_center.contains(&(s as u8, job.cx, job.cz))
                }
            };
            if ok {
                match job.stage {
                    Stage::Features => {
                        features += 1;
                        shadow_features.push((job.cx, job.cz));
                    }
                    s => shadow_center.push((s as u8, job.cx, job.cz)),
                }
                total += 1;
                admit.push(*key);
            }
        }
        let mut out = Vec::new();
        for key in admit {
            let job = st.waiting.remove(&key).expect("admitted key present");
            let inserted = match job.stage {
                Stage::Features => st.grid.insert_features(job.cx, job.cz),
                s => st.grid.insert_center(s as u8, job.cx, job.cz),
            };
            debug_assert!(inserted);
            if st.blocking.remove(&key.seq) {
                st.granted.insert(key.seq);
            } else {
                out.push(key.seq);
            }
            st.grant_log.push(key.seq);
        }
        out
    }

    pub fn active(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).grid.active_total()
    }

    pub fn waiting(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).waiting.len()
    }

    #[doc(hidden)]
    pub fn grant_log(&self) -> Vec<u64> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).grant_log.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn feat(cx: i32, cz: i32) -> Job {
        Job { stage: Stage::Features, cx, cz }
    }

    fn lim(f: usize, t: usize) -> Limits {
        Limits { max_features: f, max_inflight: t }
    }

    #[test]
    fn overlapping_features_wait_and_then_run() {
        let s = Scheduler::new(lim(8, 16));
        let a = s.try_acquire(feat(0, 0), 33).expect("first grant");
        assert!(s.try_acquire(feat(2, 0), 33).is_none(), "distance 2 blocks");
        let b = s.try_acquire(feat(6, 0), 33).expect("distance 6 clear");
        s.release(a);
        let c = s.try_acquire(feat(2, 0), 33).expect("clear after release");
        s.release(b);
        s.release(c);
        assert_eq!(s.active(), 0);
    }

    #[test]
    fn center_stage_same_chunk_excluded_other_stage_not() {
        let s = Scheduler::new(lim(8, 16));
        let j = Job { stage: Stage::StructureStarts, cx: 1, cz: 1 };
        let a = s.try_acquire(j, 33).unwrap();
        assert!(s.try_acquire(j, 33).is_none());
        let b = s.try_acquire(Job { stage: Stage::Spawn, cx: 1, cz: 1 }, 33).unwrap();
        s.release(a);
        s.release(b);
    }

    #[test]
    fn limits_hold() {
        let s = Scheduler::new(lim(2, 3));
        let a = s.try_acquire(feat(0, 0), 33).unwrap();
        let b = s.try_acquire(feat(10, 0), 33).unwrap();
        assert!(s.try_acquire(feat(20, 0), 33).is_none(), "max_features");
        let c = s.try_acquire(Job { stage: Stage::Spawn, cx: 0, cz: 0 }, 33).unwrap();
        assert!(s.try_acquire(Job { stage: Stage::Spawn, cx: 9, cz: 9 }, 33).is_none(), "max_inflight");
        s.release(a);
        s.release(b);
        s.release(c);
    }

    #[test]
    fn lower_level_admitted_first_after_release() {
        // One job holds (0,0) with max_features 1. Two waiters queue while
        // it runs: level 40 far away, level 30 adjacent. On release both
        // are clear; the level-30 job must be granted first.
        let s = Arc::new(Scheduler::new(lim(1, 16)));
        let order = Arc::new(Mutex::new(Vec::<i32>::new()));
        let hold = s.try_acquire(feat(0, 0), 33).unwrap();
        let mut handles = Vec::new();
        for (job, level) in [(feat(10, 10), 40), (feat(1, 1), 30)] {
            let sc = Arc::clone(&s);
            let order = Arc::clone(&order);
            handles.push(thread::spawn(move || {
                let t = sc.acquire(job, level);
                order.lock().unwrap().push(level);
                sc.release(t);
            }));
            while s.waiting() < handles.len() {
                thread::yield_now();
            }
        }
        s.release(hold);
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![30, 40]);
        assert_eq!(s.grant_log().len(), 3);
    }

    #[test]
    fn submit_parks_conflicts_and_release_hands_them_back() {
        let s = Scheduler::new(lim(4, 8));
        let a = s.submit(feat(0, 0), 30);
        assert!(a.granted);
        let b = s.submit(feat(1, 0), 30);
        assert!(!b.granted, "distance 1 must park");
        let c = s.submit(feat(9, 0), 30);
        assert!(c.granted, "far away runs at once");
        assert_eq!(s.waiting(), 1);
        let freed = s.release(Ticket::from_parts(feat(0, 0), a.seq));
        assert_eq!(freed, vec![b.seq]);
        assert_eq!(s.waiting(), 0);
        assert!(s.release(Ticket::from_parts(feat(1, 0), b.seq)).is_empty());
        assert!(s.release(Ticket::from_parts(feat(9, 0), c.seq)).is_empty());
        assert_eq!(s.active(), 0);
    }

    #[test]
    fn release_grants_are_conflict_free_among_themselves() {
        let s = Scheduler::new(lim(4, 8));
        let a = s.submit(feat(0, 0), 30);
        let parked: Vec<Submitted> = [1, 2, -1, -2].iter().map(|&x| s.submit(feat(x, 0), 30)).collect();
        assert!(parked.iter().all(|p| !p.granted), "all within 2 of (0,0)");
        let freed = s.release(Ticket::from_parts(feat(0, 0), a.seq));
        // Greedy in seq order: (1,0) first, then only (-2,0) is 3 away from it.
        assert_eq!(freed, vec![parked[0].seq, parked[3].seq]);
    }

    #[test]
    fn stress_no_overlap_and_no_deadlock() {
        let s = Arc::new(Scheduler::new(Limits::for_cores(8)));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let violations = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(Mutex::new(Vec::<(i32, i32)>::new()));
        let mut handles = Vec::new();
        for t in 0..8 {
            let s = Arc::clone(&s);
            let active = Arc::clone(&active);
            let violations = Arc::clone(&violations);
            let max_seen = Arc::clone(&max_seen);
            handles.push(thread::spawn(move || {
                let mut x = (t as u32 + 1).wrapping_mul(2654435761);
                for i in 0..400 {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    let cx = (x % 12) as i32;
                    let cz = ((x >> 8) % 12) as i32;
                    let job = feat(cx, cz);
                    let tk = s.acquire(job, i % 5 + 30);
                    {
                        let mut a = active.lock().unwrap();
                        for &(ax, az) in a.iter() {
                            if (ax - cx).abs().max((az - cz).abs()) <= 2 {
                                violations.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        a.push((cx, cz));
                        max_seen.fetch_max(a.len(), Ordering::Relaxed);
                    }
                    thread::yield_now();
                    {
                        let mut a = active.lock().unwrap();
                        let pos = a.iter().position(|&p| p == (cx, cz)).unwrap();
                        a.swap_remove(pos);
                    }
                    s.release(tk);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(violations.load(Ordering::Relaxed), 0);
        assert!(max_seen.load(Ordering::Relaxed) <= 7, "max_features for 8 cores is 7");
        assert_eq!(s.active(), 0);
        assert_eq!(s.waiting(), 0);
    }
}
