use crate::P;
use crate::collections::{OrderedMap, SyncSet, new_ordered_map_with_size_hint};
use std::hash::Hash;

// The Go implementation visits each level in parallel but always picks the lowest-index result,
// so this sequential port produces the same results.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BreadthFirstSearchResult<N> {
    pub stopped: bool,
    pub path: Vec<N>,
}

struct BreadthFirstSearchJob<N: 'static> {
    node: N,
    parent: Option<P<BreadthFirstSearchJob<N>>>,
}

pub struct BreadthFirstSearchLevel<'a, K, N: 'static> {
    jobs: &'a mut OrderedMap<K, P<BreadthFirstSearchJob<N>>>,
}

impl<K: Hash + Eq, N> BreadthFirstSearchLevel<'_, K, N> {
    pub fn has(&self, key: &K) -> bool {
        self.jobs.contains_key(key)
    }

    pub fn delete(&mut self, key: &K) {
        self.jobs.shift_remove(key);
    }

    pub fn range(&self, mut f: impl FnMut(&N) -> bool) {
        self.jobs.values().all(|job| f(&job.node));
    }
}

pub struct BreadthFirstSearchOptions<'a, K: Hash + Eq, N: 'static> {
    // Visited is a set of nodes that have already been visited.
    // If nil, a new set will be created.
    pub visited: Option<&'a SyncSet<K>>,
    // PreprocessLevel is a function that, if provided, will be called
    // before each level, giving the caller an opportunity to remove nodes.
    pub preprocess_level: Option<&'a mut dyn FnMut(&mut BreadthFirstSearchLevel<'_, K, N>)>,
}

impl<K: Hash + Eq, N> Default for BreadthFirstSearchOptions<'_, K, N> {
    fn default() -> Self {
        BreadthFirstSearchOptions { visited: None, preprocess_level: None }
    }
}

// BreadthFirstSearchParallel performs a breadth-first search on a graph
// starting from the given node. It returns the path
// from the first node that satisfies the `visit` function back to the start node.
pub fn breadth_first_search_parallel<N: Clone + Hash + Eq + 'static>(
    start: N,
    neighbors: impl FnMut(&N) -> Vec<N>,
    visit: impl FnMut(&N) -> (bool, bool),
) -> BreadthFirstSearchResult<N> {
    breadth_first_search_parallel_ex(
        start,
        neighbors,
        visit,
        BreadthFirstSearchOptions::default(),
        |n: &N| n.clone(),
    )
}

// BreadthFirstSearchParallelEx is an extension of BreadthFirstSearchParallel that allows
// the caller to pass a pre-seeded set of already-visited nodes and a preprocessing function
// that can be used to remove nodes from each level before processing.
pub fn breadth_first_search_parallel_ex<K: Hash + Eq + Clone, N: Clone + 'static>(
    start: N,
    mut neighbors: impl FnMut(&N) -> Vec<N>,
    mut visit: impl FnMut(&N) -> (bool, bool),
    options: BreadthFirstSearchOptions<'_, K, N>,
    mut get_key: impl FnMut(&N) -> K,
) -> BreadthFirstSearchResult<N> {
    let own_visited = SyncSet::default();
    let visited = options.visited.unwrap_or(&own_visited);
    let mut preprocess_level = options.preprocess_level;

    let mut fallback: Option<P<BreadthFirstSearchJob<N>>> = None;

    let create_path = |job: Option<P<BreadthFirstSearchJob<N>>>| -> Vec<N> {
        let mut path = Vec::new();
        path.extend(std::iter::successors(job, |j| j.parent).map(|j| j.node.clone()));
        path
    };

    let mut level: OrderedMap<K, P<BreadthFirstSearchJob<N>>> = new_ordered_map_with_size_hint(1);
    level.insert(get_key(&start), P::new(BreadthFirstSearchJob { node: start, parent: None }));
    while !level.is_empty() {
        // processLevel
        if let Some(pre) = preprocess_level.as_mut() {
            pre(&mut BreadthFirstSearchLevel { jobs: &mut level });
        }
        let mut lowest_goal: Option<usize> = None;
        let mut lowest_fallback: Option<usize> = None;
        let mut next: Vec<Vec<P<BreadthFirstSearchJob<N>>>> = Vec::with_capacity(level.len());
        for (i, &j) in level.values().enumerate() {
            next.push(Vec::new());
            if lowest_goal.is_some() {
                break; // Stop processing if we already found a lower result
            }
            // If we have already visited this node, skip it.
            if !visited.add_if_absent(get_key(&j.node)) {
                continue;
            }
            let (is_result, stop) = visit(&j.node);
            if is_result {
                // We found a result, so we will stop at this level, but an
                // earlier job may still find a true result at a lower index.
                if stop {
                    lowest_goal = Some(i);
                    continue;
                }
                if fallback.is_none() && lowest_fallback.is_none() {
                    lowest_fallback = Some(i);
                }
            }
            // Add the next level jobs
            let neighbor_nodes = neighbors(&j.node);
            if !neighbor_nodes.is_empty() {
                next[i] = neighbor_nodes
                    .into_iter()
                    .map(|child| P::new(BreadthFirstSearchJob { node: child, parent: Some(j) }))
                    .collect();
            }
        }

        if let Some(index) = lowest_goal {
            // If we found a result, return it immediately.
            let (_, &job) = level.get_index(index).unwrap();
            return BreadthFirstSearchResult { stopped: true, path: create_path(Some(job)) };
        }
        if fallback.is_none()
            && let Some(index) = lowest_fallback
        {
            fallback = Some(*level.get_index(index).unwrap().1);
        }

        let count = next.iter().map(std::vec::Vec::len).sum();
        let mut next_jobs: OrderedMap<K, P<BreadthFirstSearchJob<N>>> =
            new_ordered_map_with_size_hint(count);
        for jobs in next {
            for j in jobs {
                let key = get_key(&j.node);
                next_jobs.entry(key).or_insert(j);
            }
        }
        level = next_jobs;
    }
    BreadthFirstSearchResult { stopped: false, path: create_path(fallback) }
}
