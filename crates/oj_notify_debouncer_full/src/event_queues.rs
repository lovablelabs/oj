use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap, VecDeque},
    path::PathBuf,
    time::{Duration, Instant},
};

use notify::{
    Event, EventKind,
    event::{EventAttributes, ModifyKind, RemoveKind, RenameMode},
};
use rustc_hash::FxBuildHasher;

use crate::{DebouncedEvent, time::now};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Queue {
    /// Events must be stored in the following order:
    /// 1. `remove` or `move out` event
    /// 2. `rename` event
    /// 3. Other events
    pub(crate) events: VecDeque<DebouncedEvent>,
}

/// Event queues keyed by path.
///
/// Keeping queue processing on this non-generic type lets all file ID cache implementations share
/// the same machine code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct EventQueues {
    by_path: HashMap<PathBuf, Queue, FxBuildHasher>,
}

impl FromIterator<(PathBuf, Queue)> for EventQueues {
    fn from_iter<I: IntoIterator<Item = (PathBuf, Queue)>>(iter: I) -> Self {
        Self {
            by_path: iter.into_iter().collect(),
        }
    }
}

impl Queue {
    fn was_created(&self) -> bool {
        self.events.front().is_some_and(|event| {
            matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To))
            )
        })
    }

    fn was_removed(&self) -> bool {
        self.events.front().is_some_and(|event| {
            matches!(
                event.kind,
                EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From))
            )
        })
    }
}

impl EventQueues {
    /// No event queued for any path. `debounced_events` drops a queue it
    /// empties, and nothing inserts an empty one.
    pub(super) fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    #[inline(never)]
    pub(super) fn debounced_events(
        &mut self,
        rescan_event: &mut Option<DebouncedEvent>,
        timeout: Duration,
    ) -> Vec<DebouncedEvent> {
        let now = now();
        let mut events_expired = Vec::with_capacity(self.by_path.len());

        if let Some(event) = rescan_event.take() {
            if now.saturating_duration_since(event.time) >= timeout {
                tracing::trace!("debounce candidate rescan event: {event:?}");
                events_expired.push(event);
            } else {
                *rescan_event = Some(event);
            }
        }

        // Visit each queue in place and remove only the ones that become empty.
        self.by_path
            .extract_if(|_, queue| {
                let mut kind_index = HashMap::<EventKind, usize, FxBuildHasher>::default();
                let mut queue_expired = Vec::new();

                while let Some(event) = queue.events.pop_front() {
                    if now.saturating_duration_since(event.time) >= timeout {
                        tracing::trace!("debounce candidate event: {event:?}");

                        if let Some(idx) = kind_index.insert(event.kind, queue_expired.len()) {
                            tracing::trace!("removed candidate event: {:?}", queue_expired[idx]);
                            queue_expired[idx] = None;
                        }

                        queue_expired.push(Some(event));
                    } else {
                        if let Some(&idx) = kind_index.get(&event.kind) {
                            tracing::trace!("removed candidate event: {:?}", queue_expired[idx]);
                            queue_expired[idx] = None;
                        }
                        queue.events.push_front(event);
                        break;
                    }
                }

                events_expired.extend(queue_expired.into_iter().flatten());

                queue.events.is_empty()
            })
            .for_each(drop);

        sort_events(events_expired)
    }

    #[inline(never)]
    pub(super) fn push_rename_event(&mut self, path: PathBuf, event: Event, time: Instant) {
        let mut source_queue = self.by_path.remove(&path).unwrap_or_default();

        // remove rename `from` event
        source_queue.events.pop_back();

        // remove existing rename event
        let (remove_index, original_path, original_time) = source_queue
            .events
            .iter()
            .enumerate()
            .find_map(|(index, e)| {
                if matches!(
                    e.kind,
                    EventKind::Modify(ModifyKind::Name(RenameMode::Both))
                ) {
                    Some((Some(index), e.paths[0].clone(), e.time))
                } else {
                    None
                }
            })
            .unwrap_or((None, path, time));

        if let Some(remove_index) = remove_index {
            source_queue.events.remove(remove_index);
        }

        // split off remove or move out event and add it back to the events map
        if source_queue.was_removed() {
            let event = source_queue.events.pop_front().unwrap();

            self.by_path.insert(
                event.paths[0].clone(),
                Queue {
                    events: [event].into(),
                },
            );
        }

        // update paths
        for e in &mut source_queue.events {
            e.paths = vec![event.paths[0].clone()];
        }

        // insert rename event at the front, unless the file was just created
        if !source_queue.was_created() {
            source_queue.events.push_front(DebouncedEvent {
                event: Event {
                    kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                    paths: vec![original_path, event.paths[0].clone()],
                    attrs: event.attrs,
                },
                time: original_time,
            });
        }

        if let Some(target_queue) = self.by_path.get_mut(&event.paths[0]) {
            if !target_queue.was_created() {
                let mut remove_event = DebouncedEvent {
                    event: Event {
                        kind: EventKind::Remove(RemoveKind::Any),
                        paths: vec![event.paths[0].clone()],
                        attrs: EventAttributes::default(),
                    },
                    time: original_time,
                };
                if !target_queue.was_removed() {
                    remove_event.event = remove_event.event.set_info("override");
                }
                source_queue.events.push_front(remove_event);
            }
            *target_queue = source_queue;
        } else {
            self.by_path.insert(event.paths[0].clone(), source_queue);
        }
    }

    #[inline(never)]
    pub(super) fn push_remove_event(&mut self, event: Event, time: Instant) {
        let path = &event.paths[0];

        // remove child queues
        self.by_path
            .retain(|p, _| !p.starts_with(path) || p == path);

        match self.by_path.get_mut(path) {
            Some(queue) => {
                queue.events = [DebouncedEvent::new(event, time)].into();
            }
            None => {
                self.push_event(event, time);
            }
        }
    }

    #[inline(never)]
    pub(super) fn push_event(&mut self, event: Event, time: Instant) {
        let path = &event.paths[0];

        if let Some(queue) = self.by_path.get_mut(path) {
            // Skip duplicate create events and modifications right after creation.
            // This code relies on backends never emitting a `Modify` event with kind other than `Name` for a rename event.
            if match event.kind {
                EventKind::Modify(
                    ModifyKind::Any
                    | ModifyKind::Data(_)
                    | ModifyKind::Metadata(_)
                    | ModifyKind::Other,
                )
                | EventKind::Create(_) => !queue.was_created(),
                _ => true,
            } {
                queue.events.push_back(DebouncedEvent::new(event, time));
            }
        } else {
            self.by_path.insert(
                path.clone(),
                Queue {
                    events: [DebouncedEvent::new(event, time)].into(),
                },
            );
        }
    }
}

#[tracing::instrument(level = "trace", ret)]
fn sort_events(events: Vec<DebouncedEvent>) -> Vec<DebouncedEvent> {
    let mut sorted = Vec::with_capacity(events.len());

    let mut groups = Vec::<(PathBuf, VecDeque<DebouncedEvent>)>::new();
    let mut group_indexes: HashMap<PathBuf, usize> = HashMap::default();
    group_indexes.reserve(events.len());
    groups.reserve(events.len());

    for event in events {
        let path = event.paths.last().cloned().unwrap_or_default();

        if let Some(&index) = group_indexes.get(&path) {
            groups[index].1.push_back(event);
        } else {
            group_indexes.insert(path.clone(), groups.len());
            groups.push((path, [event].into()));
        }
    }

    // Keep path order as the tie-breaker for identical timestamps.
    groups.sort_unstable_by(|(left_path, _), (right_path, _)| left_path.cmp(right_path));
    // push events for different paths in chronological order and keep the order of events with the same path

    let mut min_time_heap = groups
        .iter()
        .enumerate()
        .map(|(index, (_, events))| Reverse((events[0].time, index)))
        .collect::<BinaryHeap<_>>();

    while let Some(Reverse((min_time, index))) = min_time_heap.pop() {
        let events = &mut groups[index].1;

        let mut push_next = false;

        while events.front().is_some_and(|event| event.time <= min_time) {
            // unwrap is safe because `pop_front` mus return some in order to enter the loop
            let event = events.pop_front().unwrap();
            sorted.push(event);
            push_next = true;
        }

        if push_next && let Some(event) = events.front() {
            min_time_heap.push(Reverse((event.time, index)));
        }
    }

    sorted
}

#[cfg(test)]
mod tests {
    use notify::{Event, EventKind};

    use super::*;

    #[test]
    fn sort_events_ties_by_path() {
        let time = now();
        let events = vec![
            DebouncedEvent::new(
                Event::new(EventKind::Any).add_path(PathBuf::from("/watch/b")),
                time,
            ),
            DebouncedEvent::new(
                Event::new(EventKind::Any).add_path(PathBuf::from("/watch/a")),
                time,
            ),
        ];

        let sorted = sort_events(events);
        let paths = sorted
            .into_iter()
            .map(|event| event.paths[0].clone())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            vec![PathBuf::from("/watch/a"), PathBuf::from("/watch/b")]
        );
    }
}
