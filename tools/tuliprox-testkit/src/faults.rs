use crate::TestkitError;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginFault {
    CloseBeforeFirstByte,
    CloseAfterBytes(u64),
    StallAfterBytes(u64),
    CorruptNextFrame,
    UnexpectedMarker(u32),
}

#[derive(Debug, Default)]
pub struct FaultSchedule {
    faults: HashMap<String, OriginFault>,
}

impl FaultSchedule {
    pub fn insert(&mut self, fault_id: String, fault: OriginFault) -> Result<(), TestkitError> {
        if self.faults.contains_key(&fault_id) {
            return Err(TestkitError::Configuration(format!("fault ID {fault_id} already exists")));
        }
        self.faults.insert(fault_id, fault);
        Ok(())
    }

    pub fn take(&mut self, fault_id: &str) -> Option<OriginFault> { self.faults.remove(fault_id) }

    pub fn clear(&mut self, fault_id: &str) -> Result<(), TestkitError> {
        self.faults
            .remove(fault_id)
            .map(|_| ())
            .ok_or_else(|| TestkitError::Configuration(format!("fault ID {fault_id} does not exist")))
    }

    pub fn clear_all(&mut self) { self.faults.clear(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fault_is_consumed_only_once() {
        let mut schedule = FaultSchedule::default();
        schedule.insert("close".to_owned(), OriginFault::CloseBeforeFirstByte).unwrap();
        assert_eq!(schedule.take("close"), Some(OriginFault::CloseBeforeFirstByte));
        assert_eq!(schedule.take("close"), None);
    }
}
