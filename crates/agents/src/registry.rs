use std::collections::HashMap;
use std::sync::Arc;

use crate::skill::Skill;

#[derive(Default, Clone)]
pub struct SkillRegistry {
    pub by_name: HashMap<String, Arc<dyn Skill>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, skill: Arc<dyn Skill>) {
        self.by_name.insert(skill.name().to_string(), skill);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Skill>> {
        self.by_name.get(name).cloned()
    }
}
