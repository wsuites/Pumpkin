//! Server-wide index from scoreboard tag to the ids of the entities carrying it.
//!
//! Datapacks select entities by tag every tick (`@e[tag=...]`). Without an index each
//! of those selectors walks every entity in the world, so a datapack that does it once
//! per vehicle costs vehicles × entities per tick. The index lets a selector visit only
//! the entities that can match. It is a lookup aid: callers still check the tag on the
//! entity itself, so a stale id (an entity that already left its world) is harmless.

use std::collections::BTreeSet;
use std::sync::{LazyLock, Mutex, PoisonError};

use rustc_hash::FxHashMap;

static INDEX: LazyLock<Mutex<FxHashMap<String, BTreeSet<i32>>>> =
    LazyLock::new(|| Mutex::new(FxHashMap::default()));

pub(crate) fn insert(tag: &str, entity_id: i32) {
    let mut index = INDEX.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(ids) = index.get_mut(tag) {
        ids.insert(entity_id);
    } else {
        index.insert(tag.to_owned(), BTreeSet::from([entity_id]));
    }
}

pub(crate) fn remove(tag: &str, entity_id: i32) {
    let mut index = INDEX.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(ids) = index.get_mut(tag) {
        ids.remove(&entity_id);
        if ids.is_empty() {
            index.remove(tag);
        }
    }
}

/// Ids of the entities that carry `tag`, in ascending (spawn) order.
#[must_use]
pub fn entities_with(tag: &str) -> Vec<i32> {
    INDEX
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(tag)
        .map(|ids| ids.iter().copied().collect())
        .unwrap_or_default()
}

/// Whether the entity with `entity_id` carries `tag`, according to the index.
#[must_use]
pub fn has(tag: &str, entity_id: i32) -> bool {
    INDEX
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(tag)
        .is_some_and(|ids| ids.contains(&entity_id))
}

/// Number of entities that carry `tag`.
#[must_use]
pub fn count(tag: &str) -> usize {
    INDEX
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(tag)
        .map_or(0, BTreeSet::len)
}
