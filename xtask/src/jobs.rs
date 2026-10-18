//! How many documents a corpus gate works on at once, and running them that way.
//!
//! The documents are independent, so the only reasons to hold back are the machine's cores and its
//! memory.
//! One cloudflare compile peaks above 9 GiB, and four at once would push a laptop into swap.
//! The default is therefore the smallest of a fixed cap, the cores, and what the free memory covers
//! at a per-document budget, worked out once at the start rather than by a scheduler.
//! Results come back in the order the documents were given, so a report, a snapshot or a summary
//! reads the same whatever the job count.

use std::num::NonZeroUsize;

use color_eyre::eyre;
use rayon::prelude::*;

/// The most documents a gate runs at once unless the caller asks for more.
const DEFAULT_CAP: usize = 4;

const GIB: u64 = 1024 * 1024 * 1024;

/// What one document can cost at peak, which sizes the default job count.
#[derive(Debug, Clone, Copy)]
pub enum Budget {
    /// Reading and generating a document: cloudflare, the largest, peaks at 3.2 GB.
    Generate,
    /// Compiling a generated crate as well: cloudflare's hand-written crate peaks at 9.3 GiB.
    Compile,
    /// Compiling one through the serde derive, which costs more: cloudflare's crate peaks at
    /// 14.7 GiB.
    CompileDerive,
}

impl Budget {
    fn bytes(self) -> u64 {
        match self {
            Self::Generate => 4 * GIB,
            Self::Compile => 10 * GIB,
            Self::CompileDerive => 15 * GIB,
        }
    }
}

/// The job count a gate runs with: the caller's, or the default for this budget.
///
/// Where the free memory cannot be read, the default is one job, because the cap that memory sets
/// is the one that keeps a small machine out of swap.
pub fn resolve(requested: Option<usize>, budget: Budget) -> usize {
    if let Some(jobs) = requested {
        return jobs.max(1);
    }
    let cores = std::thread::available_parallelism().map_or(1, NonZeroUsize::get);
    let memory = crate::bench::measure::available_memory().map_or(1, |available| {
        usize::try_from(available / budget.bytes()).unwrap_or(usize::MAX)
    });
    default_jobs(cores, memory)
}

/// The smallest of the cap, the cores and what memory allows, and never below one.
fn default_jobs(cores: usize, memory: usize) -> usize {
    DEFAULT_CAP.min(cores).min(memory).max(1)
}

/// Run `work` over `items` on a pool of `jobs` threads and return the results in `items`' order.
///
/// Each call also gets the slot of the thread running it, below `jobs`, for work that needs a
/// resource of its own per thread, such as a cargo target directory, which cargo locks for a
/// whole build.
///
/// # Errors
///
/// Returns the reason when the thread pool cannot be started.
pub fn map_ordered<T, R>(
    items: &[T],
    jobs: usize,
    work: impl Fn(&T, usize) -> R + Sync,
) -> eyre::Result<Vec<R>>
where
    T: Sync,
    R: Send,
{
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()?;
    Ok(pool.install(|| {
        items
            .par_iter()
            .map(|item| work(item, rayon::current_thread_index().unwrap_or(0)))
            .collect()
    }))
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;

    use super::{default_jobs, map_ordered};

    /// The default never exceeds four, the cores or what memory covers, and is never zero.
    #[test]
    fn the_default_is_the_smallest_bound_and_at_least_one() {
        assert_eq!(default_jobs(48, 6), 4);
        assert_eq!(default_jobs(2, 6), 2);
        assert_eq!(default_jobs(48, 1), 1);
        assert_eq!(default_jobs(48, 0), 1);
    }

    /// Results come back in the order the items were given, however many threads ran them.
    #[test_util::test]
    fn results_keep_the_order_of_their_items() {
        let items: Vec<u64> = (0..50).collect();
        for jobs in [1, 3, 8] {
            let squares = map_ordered(&items, jobs, |&item, slot| {
                assert!(slot < jobs);
                item * item
            })?;
            assert_eq!(
                squares,
                items.iter().map(|item| item * item).collect::<Vec<_>>()
            );
        }
    }
}
