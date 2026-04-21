//! Agent worker pool for GoalProcessManager.
//!
//! Manages a fixed-size pool of worker slots that can be allocated to goals,
//! assigned to tasks, and released when work completes. The pool enforces
//! saturation limits to prevent unbounded agent spawning.

use std::collections::HashMap;

/// Unique identifier for a worker slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkerId(pub u64);

/// Type of LLM backing an agent slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolLlmType {
    /// High-capability cloud model.
    Cloud,
    /// Local Ollama model.
    Local,
    /// Domain-specific fine-tuned model.
    Specialized,
}

/// State of a worker slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotState {
    /// Slot is allocated but not yet assigned to a task.
    Idle,
    /// Slot is running a specific task.
    Running { task_id: String },
    /// Slot is reserved for future use (e.g., during planning).
    Reserved,
}

/// A single worker slot in the pool.
#[derive(Debug, Clone)]
pub struct WorkerSlot {
    pub id: WorkerId,
    pub state: SlotState,
    pub llm_type: PoolLlmType,
    pub created_at: u64,
}

/// Errors from pool operations.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("pool is saturated ({0}/{0} slots in use)")]
    Saturated(usize),
    #[error("worker {0:?} not found")]
    NotFound(WorkerId),
    #[error("worker {0:?} is not idle — cannot assign task")]
    NotIdle(WorkerId),
}

/// Pool managing agent worker slots for GoalProcessManager.
///
/// The pool has a fixed maximum number of slots. Callers allocate slots
/// before spawning agents and release them when work completes.
pub struct AgentPool {
    slots: HashMap<WorkerId, WorkerSlot>,
    max_slots: usize,
    next_id: u64,
}

impl AgentPool {
    /// Create a new pool with the given maximum slot count.
    pub fn new(max_slots: usize) -> Self {
        Self {
            slots: HashMap::new(),
            max_slots,
            next_id: 1,
        }
    }

    /// Allocate a new worker slot of the given LLM type.
    ///
    /// Returns the new worker's ID, or `PoolError::Saturated` if the pool
    /// is full.
    pub fn allocate(&mut self, llm_type: PoolLlmType) -> Result<WorkerId, PoolError> {
        if self.slots.len() >= self.max_slots {
            return Err(PoolError::Saturated(self.max_slots));
        }

        let id = WorkerId(self.next_id);
        self.next_id += 1;

        let slot = WorkerSlot {
            id,
            state: SlotState::Idle,
            llm_type,
            created_at: symbiotic_core::now_unix(),
        };
        self.slots.insert(id, slot);
        Ok(id)
    }

    /// Release a worker slot, removing it from the pool.
    ///
    /// Returns `PoolError::NotFound` if the worker does not exist.
    pub fn release(&mut self, id: WorkerId) -> Result<(), PoolError> {
        self.slots
            .remove(&id)
            .ok_or(PoolError::NotFound(id))
            .map(|_| ())
    }

    /// Assign a task to an idle worker slot.
    ///
    /// Returns `PoolError::NotFound` if the worker does not exist, or
    /// `PoolError::NotIdle` if the worker is not in the `Idle` state.
    pub fn assign_task(&mut self, id: WorkerId, task_id: String) -> Result<(), PoolError> {
        let slot = self.slots.get_mut(&id).ok_or(PoolError::NotFound(id))?;

        if slot.state != SlotState::Idle {
            return Err(PoolError::NotIdle(id));
        }

        slot.state = SlotState::Running { task_id };
        Ok(())
    }

    /// Count of slots currently in the `Idle` state.
    pub fn idle_count(&self) -> usize {
        self.slots
            .values()
            .filter(|s| s.state == SlotState::Idle)
            .count()
    }

    /// Count of slots currently in the `Running` state.
    pub fn active_count(&self) -> usize {
        self.slots
            .values()
            .filter(|s| matches!(s.state, SlotState::Running { .. }))
            .count()
    }

    /// Whether the pool has reached its maximum slot count.
    pub fn is_saturated(&self) -> bool {
        self.slots.len() >= self.max_slots
    }

    /// Look up a worker slot by ID.
    pub fn get(&self, id: WorkerId) -> Option<&WorkerSlot> {
        self.slots.get(&id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocate_and_release() {
        let mut pool = AgentPool::new(4);
        let id = pool.allocate(PoolLlmType::Cloud).unwrap();

        assert_eq!(pool.idle_count(), 1);
        assert_eq!(pool.active_count(), 0);

        let slot = pool.get(id).unwrap();
        assert_eq!(slot.llm_type, PoolLlmType::Cloud);
        assert_eq!(slot.state, SlotState::Idle);

        pool.release(id).unwrap();
        assert_eq!(pool.idle_count(), 0);
        assert!(pool.get(id).is_none());
    }

    #[test]
    fn saturation_detection() {
        let mut pool = AgentPool::new(2);

        let _id1 = pool.allocate(PoolLlmType::Cloud).unwrap();
        assert!(!pool.is_saturated());

        let _id2 = pool.allocate(PoolLlmType::Local).unwrap();
        assert!(pool.is_saturated());

        let result = pool.allocate(PoolLlmType::Specialized);
        assert!(matches!(result, Err(PoolError::Saturated(2))));
    }

    #[test]
    fn task_assignment() {
        let mut pool = AgentPool::new(4);
        let id = pool.allocate(PoolLlmType::Cloud).unwrap();

        pool.assign_task(id, "goal-alpha:task-1".to_string())
            .unwrap();

        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.active_count(), 1);

        let slot = pool.get(id).unwrap();
        assert_eq!(
            slot.state,
            SlotState::Running {
                task_id: "goal-alpha:task-1".to_string()
            }
        );
    }

    #[test]
    fn assign_task_to_non_idle_fails() {
        let mut pool = AgentPool::new(4);
        let id = pool.allocate(PoolLlmType::Cloud).unwrap();

        pool.assign_task(id, "task-1".to_string()).unwrap();
        let result = pool.assign_task(id, "task-2".to_string());
        assert!(matches!(result, Err(PoolError::NotIdle(_))));
    }

    #[test]
    fn double_release_error() {
        let mut pool = AgentPool::new(4);
        let id = pool.allocate(PoolLlmType::Cloud).unwrap();

        pool.release(id).unwrap();
        let result = pool.release(id);
        assert!(matches!(result, Err(PoolError::NotFound(_))));
    }

    #[test]
    fn not_found_error() {
        let mut pool = AgentPool::new(4);
        let bogus = WorkerId(999);

        assert!(matches!(pool.release(bogus), Err(PoolError::NotFound(_))));
        assert!(matches!(
            pool.assign_task(bogus, "task".to_string()),
            Err(PoolError::NotFound(_))
        ));
        assert!(pool.get(bogus).is_none());
    }

    #[test]
    fn multiple_workers_lifecycle() {
        let mut pool = AgentPool::new(3);

        let w1 = pool.allocate(PoolLlmType::Cloud).unwrap();
        let w2 = pool.allocate(PoolLlmType::Local).unwrap();
        let w3 = pool.allocate(PoolLlmType::Specialized).unwrap();

        assert!(pool.is_saturated());
        assert_eq!(pool.idle_count(), 3);

        pool.assign_task(w1, "task-a".to_string()).unwrap();
        pool.assign_task(w2, "task-b".to_string()).unwrap();

        assert_eq!(pool.idle_count(), 1);
        assert_eq!(pool.active_count(), 2);

        // Release a running worker to free a slot.
        pool.release(w1).unwrap();
        assert!(!pool.is_saturated());
        assert_eq!(pool.active_count(), 1);

        // Allocate into the freed slot.
        let w4 = pool.allocate(PoolLlmType::Cloud).unwrap();
        assert!(pool.is_saturated());
        assert_ne!(w4, w1); // IDs are unique, never reused.

        pool.release(w2).unwrap();
        pool.release(w3).unwrap();
        pool.release(w4).unwrap();

        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.active_count(), 0);
        assert!(!pool.is_saturated());
    }

    #[test]
    fn worker_ids_are_unique() {
        let mut pool = AgentPool::new(10);
        let id1 = pool.allocate(PoolLlmType::Cloud).unwrap();
        let id2 = pool.allocate(PoolLlmType::Cloud).unwrap();
        let id3 = pool.allocate(PoolLlmType::Cloud).unwrap();

        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }
}
