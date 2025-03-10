use std::collections::{hash_map::Entry, BTreeSet, HashMap};

use anymap::AnyMap;
use lookup::LookupBuf;
use value::{Kind, Value};

use crate::{parser::ast::Ident, type_def::Details, value::Collection};

/// Local environment, limited to a given scope.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LocalEnv {
    pub(crate) bindings: HashMap<Ident, Details>,
}

impl LocalEnv {
    pub(crate) fn variable_idents(&self) -> impl Iterator<Item = &Ident> + '_ {
        self.bindings.keys()
    }

    pub(crate) fn variable(&self, ident: &Ident) -> Option<&Details> {
        self.bindings.get(ident)
    }

    #[cfg(any(feature = "expr-assignment", feature = "expr-function_call"))]
    pub(crate) fn insert_variable(&mut self, ident: Ident, details: Details) {
        self.bindings.insert(ident, details);
    }

    #[cfg(feature = "expr-function_call")]
    pub(crate) fn remove_variable(&mut self, ident: &Ident) -> Option<Details> {
        self.bindings.remove(ident)
    }

    /// Any state the child scope modified that was part of the parent is copied to the parent scope
    pub(crate) fn apply_child_scope(mut self, child: Self) -> Self {
        for (ident, child_details) in child.bindings {
            if let Some(self_details) = self.bindings.get_mut(&ident) {
                *self_details = child_details;
            }
        }

        self
    }

    /// Merges two local envs together. This is useful in cases such as if statements
    /// where different `LocalEnv`'s can be created, and the result is decided at runtime.
    /// The compile-time type must be the union of the options.
    pub(crate) fn merge(mut self, other: Self) -> Self {
        for (ident, other_details) in other.bindings {
            if let Some(self_details) = self.bindings.get_mut(&ident) {
                *self_details = self_details.clone().merge(other_details);
            }
        }
        self
    }
}

/// A lexical scope within the program.
#[derive(Debug)]
pub struct ExternalEnv {
    /// The external target of the program.
    target: Details,

    /// The type of metadata
    metadata: Kind,

    read_only_paths: BTreeSet<ReadOnlyPath>,

    /// Custom context injected by the external environment
    custom: AnyMap,
}

// temporary until paths can point to metadata
#[derive(Debug, Ord, Eq, PartialEq, PartialOrd)]
pub enum PathRoot {
    Event,
    Metadata,
}

#[derive(Debug, Ord, Eq, PartialEq, PartialOrd)]
pub struct ReadOnlyPath {
    path: LookupBuf,
    recursive: bool,
    root: PathRoot,
}

impl Default for ExternalEnv {
    fn default() -> Self {
        Self::new_with_kind(
            Kind::object(Collection::any()),
            Kind::object(Collection::any()),
        )
    }
}

impl ExternalEnv {
    /// Creates a new external environment that starts with an initial given
    /// [`Kind`].
    #[must_use]
    pub fn new_with_kind(target: Kind, metadata: Kind) -> Self {
        Self {
            target: Details {
                type_def: target.into(),
                value: None,
            },
            metadata,
            custom: AnyMap::new(),
            read_only_paths: BTreeSet::new(),
        }
    }

    pub fn is_read_only_event_path(&self, path: &LookupBuf) -> bool {
        self.is_read_only_path(path, PathRoot::Event)
    }

    pub fn is_read_only_metadata_path(&self, path: &LookupBuf) -> bool {
        self.is_read_only_path(path, PathRoot::Metadata)
    }

    pub(crate) fn is_read_only_path(&self, path: &LookupBuf, root: PathRoot) -> bool {
        for read_only_path in &self.read_only_paths {
            if read_only_path.root != root {
                continue;
            }

            // any paths that are a parent of read-only paths also can't be modified
            if read_only_path.path.can_start_with(path) {
                return true;
            }

            if read_only_path.recursive {
                if path.can_start_with(&read_only_path.path) {
                    return true;
                }
            } else if path == &read_only_path.path {
                return true;
            }
        }
        false
    }

    /// Adds a path that is considered read only. Assignments to any paths that match
    /// will fail at compile time.
    pub(crate) fn set_read_only_path(&mut self, path: LookupBuf, recursive: bool, root: PathRoot) {
        self.read_only_paths.insert(ReadOnlyPath {
            path,
            recursive,
            root,
        });
    }

    pub fn set_read_only_event_path(&mut self, path: LookupBuf, recursive: bool) {
        self.set_read_only_path(path, recursive, PathRoot::Event);
    }

    pub fn set_read_only_metadata_path(&mut self, path: LookupBuf, recursive: bool) {
        self.set_read_only_path(path, recursive, PathRoot::Metadata);
    }

    pub(crate) fn target(&self) -> &Details {
        &self.target
    }

    pub(crate) fn target_mut(&mut self) -> &mut Details {
        &mut self.target
    }

    pub fn target_kind(&self) -> &Kind {
        self.target().type_def.kind()
    }

    pub fn metadata_kind(&self) -> &Kind {
        &self.metadata
    }

    #[cfg(any(feature = "expr-assignment", feature = "expr-query"))]
    pub(crate) fn update_target(&mut self, details: Details) {
        self.target = details;
    }

    pub fn update_metadata(&mut self, kind: Kind) {
        self.metadata = kind;
    }

    /// Sets the external context data for VRL functions to use.
    pub fn set_external_context<T: 'static>(&mut self, data: T) {
        self.custom.insert::<T>(data);
    }

    /// Marks everything as read only. Any mutations on read-only values will result in a
    /// compile time error.
    pub fn read_only(mut self) -> Self {
        self.set_read_only_event_path(LookupBuf::root(), true);
        self.set_read_only_metadata_path(LookupBuf::root(), true);
        self
    }

    /// Get external context data from the external environment.
    pub fn get_external_context<T: 'static>(&self) -> Option<&T> {
        self.custom.get::<T>()
    }

    /// Swap the existing external contexts with new ones, returning the old ones.
    #[must_use]
    #[cfg(feature = "expr-function_call")]
    pub(crate) fn swap_external_context(&mut self, ctx: AnyMap) -> AnyMap {
        std::mem::replace(&mut self.custom, ctx)
    }
}

/// The state used at runtime to track changes as they happen.
#[derive(Debug, Default)]
pub struct Runtime {
    /// The [`Value`] stored in each variable.
    variables: HashMap<Ident, Value>,
}

impl Runtime {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.variables.is_empty()
    }

    pub fn clear(&mut self) {
        self.variables.clear();
    }

    #[must_use]
    pub fn variable(&self, ident: &Ident) -> Option<&Value> {
        self.variables.get(ident)
    }

    pub fn variable_mut(&mut self, ident: &Ident) -> Option<&mut Value> {
        self.variables.get_mut(ident)
    }

    pub(crate) fn insert_variable(&mut self, ident: Ident, value: Value) {
        self.variables.insert(ident, value);
    }

    pub(crate) fn remove_variable(&mut self, ident: &Ident) {
        self.variables.remove(ident);
    }

    pub(crate) fn swap_variable(&mut self, ident: Ident, value: Value) -> Option<Value> {
        match self.variables.entry(ident) {
            Entry::Occupied(mut v) => Some(std::mem::replace(v.get_mut(), value)),
            Entry::Vacant(v) => {
                v.insert(value);
                None
            }
        }
    }
}
