//! Derive minimal dependencies for reactive scopes.
//!
//! Port of `DeriveMinimalDependenciesHIR.ts` from upstream React Compiler.
//! Copyright (c) Meta Platforms, Inc. and affiliates. Licensed under MIT.
//!
//! Computes the minimal set of dependencies for a reactive scope by building
//! a trie of property paths and joining with the set of hoistable objects
//! (paths that are safe to evaluate unconditionally).

use std::collections::HashMap;

use super::types::*;

// ---------------------------------------------------------------------------
// Property access type
// ---------------------------------------------------------------------------

/// Represents the access type of a single property on a parent object.
///
/// Two independent axes:
/// - Optional / Unconditional: whether this is an optional load
/// - Access / Dependency: whether we need to track changes for this property
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PropertyAccessType {
    OptionalAccess,
    UnconditionalAccess,
    OptionalDependency,
    UnconditionalDependency,
}

impl PropertyAccessType {
    pub fn is_optional(self) -> bool {
        matches!(
            self,
            PropertyAccessType::OptionalAccess | PropertyAccessType::OptionalDependency
        )
    }

    pub fn is_dependency(self) -> bool {
        matches!(
            self,
            PropertyAccessType::OptionalDependency | PropertyAccessType::UnconditionalDependency
        )
    }

    pub fn merge(a: Self, b: Self) -> Self {
        let result_is_unconditional = !(a.is_optional() && b.is_optional());
        let result_is_dependency = a.is_dependency() || b.is_dependency();

        if result_is_unconditional {
            if result_is_dependency {
                PropertyAccessType::UnconditionalDependency
            } else {
                PropertyAccessType::UnconditionalAccess
            }
        } else if result_is_dependency {
            PropertyAccessType::OptionalDependency
        } else {
            PropertyAccessType::OptionalAccess
        }
    }
}

// ---------------------------------------------------------------------------
// Hoistable access type
// ---------------------------------------------------------------------------

/// Access type for hoistable objects. Simpler than `PropertyAccessType` because
/// hoistable objects only distinguish optional vs non-null.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoistableAccessType {
    Optional,
    NonNull,
}

// ---------------------------------------------------------------------------
// Tree node types
// ---------------------------------------------------------------------------

/// A node in the hoistable object trie.
#[derive(Debug, Clone)]
struct HoistableNode {
    properties: HashMap<String, HoistableNode>,
    access_type: HoistableAccessType,
}

/// A node in the dependency trie.
#[derive(Debug, Clone)]
struct DependencyNode {
    properties: HashMap<String, DependencyNode>,
    access_type: PropertyAccessType,
}

// ---------------------------------------------------------------------------
// ReactiveScopeDependencyTreeHIR
// ---------------------------------------------------------------------------

/// Builds a trie of scope dependencies and derives the minimal set.
///
/// This is the main entry point for computing minimal dependencies.
/// It takes a set of "hoistable objects" (paths that are safe to evaluate
/// unconditionally) and allows adding dependencies that are then minimized.
///
/// The minimization works by:
/// 1. Truncating each dependency to its maximal safe-to-evaluate subpath
///    (joined with the hoistable objects tree)
/// 2. Collecting only the minimal set: if we depend on `x`, we don't also
///    need to depend on `x.foo`.
pub struct ReactiveScopeDependencyTreeHIR {
    /// Paths from which we can hoist PropertyLoads.
    hoistable_objects: HashMap<IdentifierId, (HoistableNode, bool)>,
    /// The dependency trie. Keyed by IdentifierId, stores (trie_root, reactive, identifier).
    deps: HashMap<IdentifierId, (DependencyNode, bool, Identifier)>,
}

impl ReactiveScopeDependencyTreeHIR {
    /// Create a new dependency tree with the given hoistable objects.
    ///
    /// `hoistable_objects` is the set of paths from which it is safe to
    /// evaluate PropertyLoads unconditionally.
    pub fn new(hoistable_objects: impl IntoIterator<Item = ReactiveScopeDependency>) -> Self {
        let mut hoistable_map: HashMap<IdentifierId, (HoistableNode, bool)> = HashMap::new();

        for dep in hoistable_objects {
            let reactive = dep.identifier.scope.is_some();
            let default_access = if !dep.path.is_empty() && dep.path[0].optional {
                HoistableAccessType::Optional
            } else {
                HoistableAccessType::NonNull
            };

            let root = hoistable_map.entry(dep.identifier.id).or_insert_with(|| {
                (
                    HoistableNode {
                        properties: HashMap::new(),
                        access_type: default_access,
                    },
                    reactive,
                )
            });

            let mut curr_node = &mut root.0;

            for (i, entry) in dep.path.iter().enumerate() {
                let access_type = if i + 1 < dep.path.len() && dep.path[i + 1].optional {
                    HoistableAccessType::Optional
                } else {
                    HoistableAccessType::NonNull
                };

                curr_node = curr_node
                    .properties
                    .entry(entry.property.clone())
                    .or_insert_with(|| HoistableNode {
                        properties: HashMap::new(),
                        access_type,
                    });
            }
        }

        Self {
            hoistable_objects: hoistable_map,
            deps: HashMap::new(),
        }
    }

    /// Add a dependency to be tracked.
    ///
    /// The dependency is joined with the hoistable objects tree to determine
    /// the maximal safe-to-evaluate subpath.
    pub fn add_dependency(&mut self, dep: &ReactiveScopeDependency) {
        let identifier_id = dep.identifier.id;
        let reactive = dep.identifier.scope.is_some();

        // Get or create the root dep node
        self.deps.entry(identifier_id).or_insert_with(|| {
            (
                DependencyNode {
                    properties: HashMap::new(),
                    access_type: PropertyAccessType::UnconditionalAccess,
                },
                reactive,
                dep.identifier.clone(),
            )
        });

        let dep_entry = self.deps.get_mut(&identifier_id).unwrap();
        let mut dep_cursor = &mut dep_entry.0;

        // Get the hoistable cursor if available
        let hoistable_root = self.hoistable_objects.get(&identifier_id);
        let mut hoistable_cursor: Option<&HoistableNode> = hoistable_root.map(|(n, _)| n);

        for entry in &dep.path {
            let next_hoistable_cursor: Option<&HoistableNode>;

            if entry.optional {
                // No need to check access type since we can match both optional
                // or non-optionals in the hoistable tree.
                next_hoistable_cursor =
                    hoistable_cursor.and_then(|h| h.properties.get(&entry.property));

                let access_type = if hoistable_cursor
                    .is_some_and(|h| h.access_type == HoistableAccessType::NonNull)
                {
                    // If the hoistable tree says this is non-null, we can treat
                    // the optional access as unconditional.
                    PropertyAccessType::UnconditionalAccess
                } else {
                    // Optional load never throws, so it's safe to evaluate.
                    PropertyAccessType::OptionalAccess
                };

                make_or_merge_property(dep_cursor, &entry.property, access_type);
                dep_cursor = dep_cursor.properties.get_mut(&entry.property).unwrap();
            } else if hoistable_cursor
                .is_some_and(|h| h.access_type == HoistableAccessType::NonNull)
            {
                next_hoistable_cursor =
                    hoistable_cursor.and_then(|h| h.properties.get(&entry.property));
                make_or_merge_property(
                    dep_cursor,
                    &entry.property,
                    PropertyAccessType::UnconditionalAccess,
                );
                dep_cursor = dep_cursor.properties.get_mut(&entry.property).unwrap();
            } else {
                // Break: truncate the dependency at its first non-optional entry
                // that PropertyLoads are not hoistable from.
                break;
            }

            hoistable_cursor = next_hoistable_cursor;
        }

        // Mark the final node as a dependency
        dep_cursor.access_type = PropertyAccessType::merge(
            dep_cursor.access_type,
            PropertyAccessType::OptionalDependency,
        );
    }

    /// Derive the minimal set of dependencies.
    ///
    /// Walks the dependency trie and collects dependencies at the highest
    /// level possible. If a node is marked as a dependency, its subtree is
    /// not traversed (because the parent dependency subsumes any child
    /// dependencies).
    pub fn derive_minimal_dependencies(&self) -> Vec<ReactiveScopeDependency> {
        let mut results = Vec::new();

        // Sort by IdentifierId for deterministic ordering (upstream JS Map preserves insertion order)
        let mut ids: Vec<_> = self.deps.keys().copied().collect();
        ids.sort();
        for id in ids {
            let (root_node, _reactive, identifier) = &self.deps[&id];
            collect_minimal_dependencies_in_subtree(root_node, identifier, &[], &mut results);
        }

        results
    }
}

/// Recursively collect minimal dependencies from a subtree.
fn collect_minimal_dependencies_in_subtree(
    node: &DependencyNode,
    root_identifier: &Identifier,
    path: &[DependencyPathEntry],
    results: &mut Vec<ReactiveScopeDependency>,
) {
    if node.access_type.is_dependency() {
        results.push(ReactiveScopeDependency {
            identifier: root_identifier.clone(),
            path: path.to_vec(),
        });
    } else {
        for (child_name, child_node) in &node.properties {
            let mut child_path = path.to_vec();
            child_path.push(DependencyPathEntry {
                property: child_name.clone(),
                optional: child_node.access_type.is_optional(),
            });
            collect_minimal_dependencies_in_subtree(
                child_node,
                root_identifier,
                &child_path,
                results,
            );
        }
    }
}

/// Get or create a child property node, merging access types if it already exists.
fn make_or_merge_property(
    node: &mut DependencyNode,
    property: &str,
    access_type: PropertyAccessType,
) {
    let child = node
        .properties
        .entry(property.to_string())
        .or_insert_with(|| DependencyNode {
            properties: HashMap::new(),
            access_type,
        });

    // If it already existed, merge the access type
    if child.access_type != access_type {
        child.access_type = PropertyAccessType::merge(child.access_type, access_type);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_id(id: u32) -> Identifier {
        make_temporary_identifier(IdentifierId::new(id), SourceLocation::Generated)
    }

    fn make_dep(id: u32, path: Vec<(&str, bool)>) -> ReactiveScopeDependency {
        ReactiveScopeDependency {
            identifier: make_id(id),
            path: path
                .into_iter()
                .map(|(p, opt)| DependencyPathEntry {
                    property: p.to_string(),
                    optional: opt,
                })
                .collect(),
        }
    }

    fn make_hoistable(id: u32, path: Vec<(&str, bool)>) -> ReactiveScopeDependency {
        make_dep(id, path)
    }

    #[test]
    fn test_single_dependency() {
        let mut tree = ReactiveScopeDependencyTreeHIR::new(vec![make_hoistable(1, vec![])]);

        tree.add_dependency(&make_dep(1, vec![]));

        let deps = tree.derive_minimal_dependencies();
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn test_subsumption() {
        // If we depend on x and x.a, the minimal set should just be x.
        let hoistables = vec![make_hoistable(1, vec![("a", false)])];

        let mut tree = ReactiveScopeDependencyTreeHIR::new(hoistables);

        tree.add_dependency(&make_dep(1, vec![]));
        tree.add_dependency(&make_dep(1, vec![("a", false)]));

        let deps = tree.derive_minimal_dependencies();
        // x subsumes x.a, so we should only have x
        assert_eq!(deps.len(), 1);
        assert!(deps[0].path.is_empty());
    }

    #[test]
    fn test_sibling_properties() {
        // Depending on x.a and x.b should produce both.
        let hoistables = vec![
            make_hoistable(1, vec![("a", false)]),
            make_hoistable(1, vec![("b", false)]),
        ];

        let mut tree = ReactiveScopeDependencyTreeHIR::new(hoistables);

        tree.add_dependency(&make_dep(1, vec![("a", false)]));
        tree.add_dependency(&make_dep(1, vec![("b", false)]));

        let deps = tree.derive_minimal_dependencies();
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn test_optional_access_merge() {
        // Test that optional + unconditional merge produces unconditional
        assert_eq!(
            PropertyAccessType::merge(
                PropertyAccessType::OptionalAccess,
                PropertyAccessType::UnconditionalAccess
            ),
            PropertyAccessType::UnconditionalAccess
        );

        // Test that optional + optional stays optional
        assert_eq!(
            PropertyAccessType::merge(
                PropertyAccessType::OptionalAccess,
                PropertyAccessType::OptionalAccess
            ),
            PropertyAccessType::OptionalAccess
        );

        // Test that access + dependency produces dependency
        assert_eq!(
            PropertyAccessType::merge(
                PropertyAccessType::UnconditionalAccess,
                PropertyAccessType::OptionalDependency
            ),
            PropertyAccessType::UnconditionalDependency
        );
    }
}
