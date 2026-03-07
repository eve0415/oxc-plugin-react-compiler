//! Environment and global type shapes.
//!
//! Port of `Environment.ts` from upstream.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use crate::hir::types::{Identifier, IdentifierId, SourceLocation, make_temporary_identifier};
use crate::options::EnvironmentConfig;

/// The environment tracks known global types, React API shapes, and ID counters.
#[derive(Debug, Clone)]
pub struct Environment {
    inner: Rc<EnvironmentInner>,
}

#[derive(Debug)]
struct EnvironmentInner {
    config: EnvironmentConfig,
    next_block_id: Cell<u32>,
    next_identifier_id: Cell<u32>,
    has_inferred_effect: Cell<bool>,
    has_fire_rewrite: Cell<bool>,
    inferred_effect_locations: RefCell<HashSet<SourceLocation>>,
}

impl Environment {
    pub fn new(config: EnvironmentConfig) -> Self {
        Self {
            inner: Rc::new(EnvironmentInner {
                config,
                next_block_id: Cell::new(0),
                next_identifier_id: Cell::new(0),
                has_inferred_effect: Cell::new(false),
                has_fire_rewrite: Cell::new(false),
                inferred_effect_locations: RefCell::new(HashSet::new()),
            }),
        }
    }

    pub fn config(&self) -> &EnvironmentConfig {
        &self.inner.config
    }

    pub fn next_block_id(&self) -> u32 {
        let id = self.inner.next_block_id.get();
        self.inner.next_block_id.set(id + 1);
        id
    }

    pub fn set_next_block_id(&self, id: u32) {
        self.inner.next_block_id.set(id);
    }

    pub fn current_next_block_id(&self) -> u32 {
        self.inner.next_block_id.get()
    }

    pub fn next_identifier_id(&self) -> u32 {
        let id = self.inner.next_identifier_id.get();
        self.inner.next_identifier_id.set(id + 1);
        id
    }

    pub fn set_next_identifier_id(&self, id: u32) {
        self.inner.next_identifier_id.set(id);
    }

    pub fn current_next_identifier_id(&self) -> u32 {
        self.inner.next_identifier_id.get()
    }

    pub fn has_inferred_effect(&self) -> bool {
        self.inner.has_inferred_effect.get()
    }

    pub fn set_has_inferred_effect(&self, value: bool) {
        self.inner.has_inferred_effect.set(value);
    }

    pub fn has_fire_rewrite(&self) -> bool {
        self.inner.has_fire_rewrite.get()
    }

    pub fn set_has_fire_rewrite(&self, value: bool) {
        self.inner.has_fire_rewrite.set(value);
    }

    pub fn add_inferred_effect_location(&self, loc: SourceLocation) {
        self.inner
            .inferred_effect_locations
            .borrow_mut()
            .insert(loc);
    }

    pub fn make_temporary_identifier(&self, loc: SourceLocation) -> Identifier {
        let id = self.next_identifier_id();
        make_temporary_identifier(IdentifierId::new(id), loc)
    }

    /// Check if a name follows React hook naming convention.
    ///
    /// Returns `true` if the name is exactly `"use"` or starts with `"use"` followed
    /// by an uppercase ASCII letter (e.g. `"useState"`, `"useEffect"`).
    pub fn is_hook_name(name: &str) -> bool {
        if name == "use" {
            return true;
        }
        if let Some(rest) = name.strip_prefix("use") {
            rest.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        } else {
            false
        }
    }

    /// Check if a name follows React component naming convention.
    ///
    /// Returns `true` if the name starts with an uppercase ASCII letter.
    pub fn is_component_name(name: &str) -> bool {
        name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
    }

    /// Check if the given name matches the configured hook pattern, falling back
    /// to the standard [`Self::is_hook_name`] check when no pattern is configured.
    pub fn matches_hook_pattern(&self, name: &str) -> bool {
        if let Some(ref pattern) = self.inner.config.hook_pattern {
            // Simple prefix matching for now; will be replaced with regex
            // when the hook_pattern feature is fully ported.
            name.starts_with(pattern.as_str())
        } else {
            Self::is_hook_name(name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_hook_name() {
        // Positive cases
        assert!(Environment::is_hook_name("use"));
        assert!(Environment::is_hook_name("useState"));
        assert!(Environment::is_hook_name("useEffect"));
        assert!(Environment::is_hook_name("useMyCustomHook"));
        assert!(Environment::is_hook_name("useRef"));

        // Negative cases
        assert!(!Environment::is_hook_name("used"));
        assert!(!Environment::is_hook_name("user"));
        assert!(!Environment::is_hook_name("useless"));
        assert!(!Environment::is_hook_name("foo"));
        assert!(!Environment::is_hook_name(""));
        assert!(!Environment::is_hook_name("Use")); // starts with uppercase U, not "use"
    }

    #[test]
    fn test_is_component_name() {
        // Positive cases
        assert!(Environment::is_component_name("App"));
        assert!(Environment::is_component_name("MyComponent"));
        assert!(Environment::is_component_name("Button"));

        // Negative cases
        assert!(!Environment::is_component_name("app"));
        assert!(!Environment::is_component_name("myComponent"));
        assert!(!Environment::is_component_name(""));
        assert!(!Environment::is_component_name("123"));
    }

    #[test]
    fn test_matches_hook_pattern_default() {
        let env = Environment::new(EnvironmentConfig::default());
        assert!(env.matches_hook_pattern("useState"));
        assert!(env.matches_hook_pattern("use"));
        assert!(!env.matches_hook_pattern("foo"));
    }

    #[test]
    fn test_matches_hook_pattern_custom() {
        let mut config = EnvironmentConfig::default();
        config.hook_pattern = Some("React$".to_string());
        let env = Environment::new(config);

        assert!(env.matches_hook_pattern("React$useState"));
        assert!(env.matches_hook_pattern("React$useEffect"));
        assert!(!env.matches_hook_pattern("useState"));
        assert!(!env.matches_hook_pattern("use"));
    }
}
