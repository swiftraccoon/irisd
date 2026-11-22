//! The bound on the live payload bytes alive in a viewer at once.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// The viewer-wide bound on live payload bytes: every byte a sink copies for
/// its decoder counts from the copy until the decoder frees it, across every
/// view of the viewer. A sink's queue limit bounds what waits in it; this
/// bounds what is alive anywhere downstream too.
///
/// # Example
///
/// ```
/// # use std::error::Error;
/// use std::sync::Arc;
///
/// use iris_client::live::LiveBudget;
///
/// let budget = Arc::new(LiveBudget::new(1 << 20));
/// let held = budget.reserve(600 << 10).ok_or("600 KiB of 1 MiB fit")?;
/// // What the limit leaves is less than another 600 KiB.
/// assert_eq!(budget.limit() - budget.used(), 424 << 10);
/// assert!(budget.reserve(600 << 10).is_none(), "1.2 MiB do not fit");
/// drop(held);
/// assert_eq!(budget.used(), 0);
/// # Ok::<(), Box<dyn Error>>(())
/// ```
#[derive(Debug)]
pub struct LiveBudget {
    limit: u64,
    used: AtomicU64,
}

impl LiveBudget {
    /// A budget of `limit` bytes, which every sink that holds it shares: a
    /// viewer puts it in an [`Arc`] and hands each sink a clone, since
    /// [`LiveBudget::reserve`] keeps one in each reservation. The example on
    /// [`LiveBudget`] makes one.
    #[must_use]
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
        }
    }

    /// The bytes the budget admits at once. The example on [`LiveBudget`]
    /// reads it.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.limit
    }

    /// The bytes reserved at this moment. The example on [`LiveBudget`] reads
    /// it.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    /// Reserves `bytes`, or returns `None` when they do not fit beside the
    /// bytes already reserved. The reservation is released when it is
    /// dropped. The example on [`LiveBudget`] reserves twice.
    #[must_use]
    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Option<Reservation> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes).filter(|next| *next <= self.limit)?;
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(Reservation {
                        budget: Arc::clone(self),
                        bytes,
                    });
                }
                Err(current) => used = current,
            }
        }
    }
}

/// Bytes taken from a [`LiveBudget`], returned when dropped. A sink keeps it
/// with the copy it hands its decoder, for as long as the decoder keeps that
/// copy. The example on [`LiveBudget`] takes one and drops it.
#[derive(Debug)]
#[must_use = "the bytes return to the budget when the reservation drops"]
pub struct Reservation {
    budget: Arc<LiveBudget>,
    bytes: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    #[test]
    fn a_budget_reads_its_limit() {
        assert_eq!(LiveBudget::new(10).limit(), 10);
    }

    #[test]
    fn a_budget_admits_reservations_up_to_its_limit() {
        let budget = Arc::new(LiveBudget::new(10));
        let first = budget.reserve(4).expect("4 of 10 bytes");
        let second = budget.reserve(6).expect("the last 6 bytes");
        assert_eq!(budget.used(), 10);
        drop((first, second));
    }

    /// A byte past a full budget, and more than the limit of an empty one.
    #[test]
    fn a_reservation_past_the_limit_is_refused() {
        let budget = Arc::new(LiveBudget::new(10));
        let whole = budget.reserve(10).expect("the whole budget");
        assert!(budget.reserve(1).is_none(), "a byte past the limit");
        drop(whole);
        assert!(budget.reserve(11).is_none(), "11 bytes of 10");
    }

    #[test]
    fn a_refusal_reserves_nothing() {
        let budget = Arc::new(LiveBudget::new(10));
        let _refused = budget.reserve(11);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn dropping_a_reservation_returns_its_bytes() {
        let budget = Arc::new(LiveBudget::new(10));
        let first = budget.reserve(6).unwrap();
        let second = budget.reserve(4).unwrap();
        drop(first);
        assert_eq!(budget.used(), 4);
        let third = budget
            .reserve(6)
            .expect("the returned bytes are reserved again");
        drop((second, third));
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_reservation_whose_sum_overflows_is_refused() {
        let budget = Arc::new(LiveBudget::new(u64::MAX));
        let most = budget.reserve(u64::MAX).expect("the whole range");
        assert!(budget.reserve(1).is_none(), "a sum past u64::MAX");
        drop(most);
    }

    /// Sinks on several threads draw on one budget: it admits exactly the
    /// reservations that fit, however their attempts interleave.
    #[test]
    fn reservations_from_many_threads_admit_exactly_what_fits() {
        let budget = Arc::new(LiveBudget::new(100 << 10));
        let mut held: Vec<Reservation> = Vec::new();
        thread::scope(|scope| {
            let mut workers = Vec::new();
            for _ in 0..8 {
                workers.push(scope.spawn(|| {
                    let mut taken = Vec::new();
                    for _ in 0..100 {
                        if let Some(reservation) = budget.reserve(1 << 10) {
                            taken.push(reservation);
                        }
                    }
                    taken
                }));
            }
            for worker in workers {
                held.extend(worker.join().unwrap());
            }
        });
        assert_eq!(held.len(), 100, "100 KiB in reservations of 1 KiB");
        assert_eq!(budget.used(), 100 << 10);
    }

    #[test]
    fn a_reservation_dropped_on_another_thread_returns_its_bytes() {
        let budget = Arc::new(LiveBudget::new(10));
        let held = budget.reserve(10).expect("the whole budget");
        thread::spawn(move || drop(held)).join().unwrap();
        assert_eq!(budget.used(), 0);
    }
}
