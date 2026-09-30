//! Presentation addresses only: no item assembly, lifecycle validation or emission.
use std::collections::HashMap;

use crate::executor::error::{ExecutorError, ExecutorResult};
use crate::types::agent::AgentIdentity;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::executor) struct AgentRoundId {
    pub agent: AgentIdentity,
    pub round: usize,
}

#[derive(Default)]
pub(super) struct SourceProjection {
    indexes: HashMap<AgentRoundId, HashMap<u64, usize>>,
    items: HashMap<String, HashMap<String, usize>>,
    next_index: usize,
}

impl SourceProjection {
    pub fn added(&mut self, source: &AgentRoundId, local: u64, id: &str) -> ExecutorResult<usize> {
        if self
            .indexes
            .get(source)
            .is_some_and(|indexes| indexes.contains_key(&local))
            || self
                .items
                .get(source.agent.as_str())
                .is_some_and(|items| items.contains_key(id))
        {
            return Err(ExecutorError::StreamError(
                "duplicate source presentation address".into(),
            ));
        }
        let index = self.reserve()?;
        self.indexes.entry(source.clone()).or_default().insert(local, index);
        self.items
            .entry(source.agent.to_string())
            .or_default()
            .insert(id.to_owned(), index);
        Ok(index)
    }

    pub fn index(&self, source: &AgentRoundId, local: u64) -> ExecutorResult<usize> {
        self.indexes
            .get(source)
            .and_then(|indexes| indexes.get(&local))
            .copied()
            .ok_or_else(|| ExecutorError::StreamError("source event has no public index".into()))
    }

    pub fn take_item(&mut self, agent: &str, id: &str) -> Option<usize> {
        self.items.get_mut(agent)?.remove(id)
    }

    pub fn finish_source(&mut self, source: &AgentRoundId) {
        self.indexes.remove(source);
        if self.items.get(source.agent.as_str()).is_some_and(HashMap::is_empty) {
            self.items.remove(source.agent.as_str());
        }
    }

    pub fn has_live_items(&self, agent: &AgentIdentity) -> bool {
        self.items.get(agent.as_str()).is_some_and(|items| !items.is_empty())
    }

    pub fn reserve(&mut self) -> ExecutorResult<usize> {
        let index = self.next_index;
        self.next_index = index
            .checked_add(1)
            .ok_or_else(|| ExecutorError::StreamError("public output index overflow".into()))?;
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_addresses_do_not_replace_or_consume_public_indexes() {
        let mut projection = SourceProjection::default();
        let source = AgentRoundId {
            agent: AgentIdentity::root(),
            round: 0,
        };
        assert_eq!(projection.added(&source, 0, "a").unwrap(), 0);
        assert!(projection.added(&source, 0, "b").is_err());
        assert!(projection.added(&source, 1, "a").is_err());
        assert_eq!(projection.index(&source, 0).unwrap(), 0);
        assert_eq!(projection.added(&source, 1, "b").unwrap(), 1);
        assert_eq!(projection.take_item("/root", "a"), Some(0));
        assert_eq!(projection.take_item("/root", "b"), Some(1));
        projection.finish_source(&source);
        assert!(projection.indexes.is_empty() && projection.items.is_empty());
    }
}
