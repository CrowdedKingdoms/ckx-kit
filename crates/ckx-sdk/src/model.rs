//! Typed state for a hub: what the legacy game models kept in the database, kept in the
//! hub instead. Containers of declared types hold typed properties with defaults, edges relate
//! them, and each property's visibility decides what a player may read.
//!
//! ```ignore
//! let schema = Schema::new()
//!     .container("Character", |c| {
//!         c.prop("hp", PropKind::Int, 100, Visibility::Public)
//!             .prop("gold", PropKind::Int, 0, Visibility::Owner)
//!             .prop("weakness", PropKind::Str, "", Visibility::Hidden)
//!     })
//!     .container("Item", |c| c.prop("name", PropKind::Str, "", Visibility::Public));
//! let hero = self.model.ensure(&schema, "Character", &player.to_string(), Some(player))?;
//! self.model.add(&schema, hero, "hp", -12)?;
//! self.model.publish_changes(ctx, &schema, "world");
//! ```
//!
//! The model is plain data: keep it in the hub's state and it is snapshotted with the rest.
//! The schema is code, rebuilt with the module, so a property added in a new version reads as
//! its default in containers created before it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Ctx, Error, Result};

/// What a property holds. `Json` holds any JSON value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PropKind {
    Bool,
    Int,
    Float,
    Str,
    Json,
}

impl PropKind {
    fn accepts(self, v: &Value) -> bool {
        match self {
            PropKind::Bool => v.is_boolean(),
            PropKind::Int => v.is_i64() || v.is_u64(),
            PropKind::Float => v.is_number(),
            PropKind::Str => v.is_string(),
            PropKind::Json => true,
        }
    }
}

/// Who may read a property: anyone who may read the container, only its owner, or nobody but
/// the hub itself (secret stats, seeds).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Public,
    Owner,
    Hidden,
}

/// Who a [`Model::view`] is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewer {
    /// The hub, another instance or a developer: every property.
    Server,
    /// A player: public properties, and owner ones on containers the player owns.
    Player(u64),
    /// Anyone: public properties only, as [`Model::publish_changes`] sends them.
    Public,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Prop {
    pub kind: PropKind,
    pub default: Value,
    pub visibility: Visibility,
}

/// The declared container types and their properties.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    types: BTreeMap<String, BTreeMap<String, Prop>>,
}

/// Declares one container type's properties; see [`Schema::container`].
#[derive(Default)]
pub struct ContainerDef {
    props: BTreeMap<String, Prop>,
}

impl ContainerDef {
    /// A property with its kind, its default, and who may read it. A default of the wrong kind
    /// panics when the schema is built, which is where a typo belongs.
    pub fn prop(mut self, key: &str, kind: PropKind, default: impl Into<Value>, visibility: Visibility) -> Self {
        let default = default.into();
        assert!(kind.accepts(&default), "default of `{key}` is not a {kind:?}");
        self.props.insert(key.to_owned(), Prop { kind, default, visibility });
        self
    }
}

impl Schema {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn container(mut self, name: &str, define: impl FnOnce(ContainerDef) -> ContainerDef) -> Self {
        self.types.insert(name.to_owned(), define(ContainerDef::default()).props);
        self
    }

    fn props(&self, type_name: &str) -> Result<&BTreeMap<String, Prop>> {
        self.types.get(type_name).ok_or_else(|| Error(format!("no container type `{type_name}`")))
    }

    fn prop(&self, type_name: &str, key: &str) -> Result<&Prop> {
        self.props(type_name)?.get(key).ok_or_else(|| Error(format!("`{type_name}` has no property `{key}`")))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Container {
    pub type_name: String,
    /// The player who owns it, if one does: they read its `Owner` properties.
    pub owner: Option<u64>,
    /// The key it was [`Model::ensure`]d under, if it was.
    pub key: Option<String>,
    /// Properties set so far; the rest read as their defaults.
    props: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Edge {
    pub from: u64,
    pub relation: String,
    pub to: u64,
}

/// What changed since the last [`Model::take_changes`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub changed: Vec<u64>,
    pub removed: Vec<u64>,
}

/// One published change: a container's public view, or its removal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Published {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub props: Option<BTreeMap<String, Value>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

/// The deepest [`Model::traverse`] walks, as the legacy graph traversal.
pub const MAX_DEPTH: usize = 5;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Model {
    next_id: u64,
    containers: BTreeMap<u64, Container>,
    /// `"<type>\0<key>"` to the container ensured under it.
    keys: BTreeMap<String, u64>,
    edges: BTreeSet<Edge>,
    #[serde(skip)]
    changed: BTreeSet<u64>,
    #[serde(skip)]
    removed: BTreeSet<u64>,
}

fn key_of(type_name: &str, key: &str) -> String {
    format!("{type_name}\0{key}")
}

impl Model {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new container of `type_name`, with every property at its default.
    pub fn create(&mut self, schema: &Schema, type_name: &str, owner: Option<u64>) -> Result<u64> {
        schema.props(type_name)?;
        self.next_id += 1;
        let id = self.next_id;
        self.containers.insert(id, Container { type_name: type_name.to_owned(), owner, key: None, props: BTreeMap::new() });
        self.changed.insert(id);
        Ok(id)
    }

    /// The container of `type_name` under `key`, created the first time: a player's character,
    /// a match's scoreboard.
    pub fn ensure(&mut self, schema: &Schema, type_name: &str, key: &str, owner: Option<u64>) -> Result<u64> {
        if let Some(id) = self.find(type_name, key) {
            return Ok(id);
        }
        let id = self.create(schema, type_name, owner)?;
        self.containers.get_mut(&id).expect("just created").key = Some(key.to_owned());
        self.keys.insert(key_of(type_name, key), id);
        Ok(id)
    }

    pub fn find(&self, type_name: &str, key: &str) -> Option<u64> {
        self.keys.get(&key_of(type_name, key)).copied()
    }

    pub fn container(&self, id: u64) -> Option<&Container> {
        self.containers.get(&id)
    }

    /// Every container's id, oldest first.
    pub fn ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.containers.keys().copied()
    }

    /// Every container of `type_name`, oldest first.
    pub fn of_type<'a>(&'a self, type_name: &'a str) -> impl Iterator<Item = u64> + 'a {
        self.containers.iter().filter(move |(_, c)| c.type_name == type_name).map(|(id, _)| *id)
    }

    fn existing(&self, id: u64) -> Result<&Container> {
        self.containers.get(&id).ok_or_else(|| Error(format!("no container {id}")))
    }

    /// A property's value, or its default when it was never set.
    pub fn get(&self, schema: &Schema, id: u64, key: &str) -> Result<Value> {
        let c = self.existing(id)?;
        let prop = schema.prop(&c.type_name, key)?;
        Ok(c.props.get(key).cloned().unwrap_or_else(|| prop.default.clone()))
    }

    pub fn get_i64(&self, schema: &Schema, id: u64, key: &str) -> Result<i64> {
        self.get(schema, id, key)?.as_i64().ok_or_else(|| Error(format!("`{key}` is not an integer")))
    }

    pub fn get_f64(&self, schema: &Schema, id: u64, key: &str) -> Result<f64> {
        self.get(schema, id, key)?.as_f64().ok_or_else(|| Error(format!("`{key}` is not a number")))
    }

    pub fn get_bool(&self, schema: &Schema, id: u64, key: &str) -> Result<bool> {
        self.get(schema, id, key)?.as_bool().ok_or_else(|| Error(format!("`{key}` is not a bool")))
    }

    pub fn get_str(&self, schema: &Schema, id: u64, key: &str) -> Result<String> {
        match self.get(schema, id, key)? {
            Value::String(s) => Ok(s),
            _ => Err(Error(format!("`{key}` is not a string"))),
        }
    }

    /// Sets a property, refusing a value of the wrong kind.
    pub fn set(&mut self, schema: &Schema, id: u64, key: &str, value: impl Into<Value>) -> Result<()> {
        let value = value.into();
        let type_name = self.existing(id)?.type_name.clone();
        let prop = schema.prop(&type_name, key)?;
        if !prop.kind.accepts(&value) {
            return Err(Error(format!("`{type_name}.{key}` is a {:?}, not {value}", prop.kind)));
        }
        self.containers.get_mut(&id).expect("checked").props.insert(key.to_owned(), value);
        self.changed.insert(id);
        Ok(())
    }

    /// Adds `delta` to an integer property and returns the new value.
    pub fn add(&mut self, schema: &Schema, id: u64, key: &str, delta: i64) -> Result<i64> {
        let now = self.get_i64(schema, id, key)?.saturating_add(delta);
        self.set(schema, id, key, now)?;
        Ok(now)
    }

    /// Removes a container, its key and every edge touching it.
    pub fn delete(&mut self, id: u64) -> bool {
        let Some(c) = self.containers.remove(&id) else { return false };
        if let Some(key) = &c.key {
            self.keys.remove(&key_of(&c.type_name, key));
        }
        self.edges.retain(|e| e.from != id && e.to != id);
        self.changed.remove(&id);
        self.removed.insert(id);
        true
    }

    /// `from --relation--> to`, e.g. an inventory holding an item.
    pub fn link(&mut self, from: u64, relation: &str, to: u64) -> Result<()> {
        self.existing(from)?;
        self.existing(to)?;
        self.edges.insert(Edge { from, relation: relation.to_owned(), to });
        self.changed.insert(from);
        Ok(())
    }

    pub fn unlink(&mut self, from: u64, relation: &str, to: u64) -> bool {
        let gone = self.edges.remove(&Edge { from, relation: relation.to_owned(), to });
        if gone {
            self.changed.insert(from);
        }
        gone
    }

    /// The containers `from` links to by `relation`.
    pub fn linked(&self, from: u64, relation: &str) -> Vec<u64> {
        self.edges.iter().filter(|e| e.from == from && e.relation == relation).map(|e| e.to).collect()
    }

    /// Every container reachable from `root` by `relation`, breadth first, at most
    /// [`MAX_DEPTH`] steps away; `root` itself is not included.
    pub fn traverse(&self, root: u64, relation: &str, depth: usize) -> Vec<u64> {
        let depth = depth.min(MAX_DEPTH);
        let mut seen = BTreeSet::from([root]);
        let mut out = Vec::new();
        let mut queue = VecDeque::from([(root, 0usize)]);
        while let Some((at, d)) = queue.pop_front() {
            if d == depth {
                continue;
            }
            for next in self.linked(at, relation) {
                if seen.insert(next) {
                    out.push(next);
                    queue.push_back((next, d + 1));
                }
            }
        }
        out
    }

    /// The properties `viewer` may read, with defaults filled in: every one for the server,
    /// public ones for a player, and owner ones too on containers the player owns.
    pub fn view(&self, schema: &Schema, id: u64, viewer: Viewer) -> Result<BTreeMap<String, Value>> {
        let c = self.existing(id)?;
        let owns = matches!(viewer, Viewer::Player(p) if c.owner == Some(p));
        let mut out = BTreeMap::new();
        for (key, prop) in schema.props(&c.type_name)? {
            let visible = match (viewer, prop.visibility) {
                (Viewer::Server, _) | (_, Visibility::Public) => true,
                (Viewer::Player(_), Visibility::Owner) => owns,
                (Viewer::Player(_), Visibility::Hidden) | (Viewer::Public, _) => false,
            };
            if visible {
                out.insert(key.clone(), c.props.get(key).cloned().unwrap_or_else(|| prop.default.clone()));
            }
        }
        Ok(out)
    }

    /// The containers changed and removed since the last call.
    pub fn take_changes(&mut self) -> Changes {
        Changes {
            changed: std::mem::take(&mut self.changed).into_iter().collect(),
            removed: std::mem::take(&mut self.removed).into_iter().collect(),
        }
    }

    /// The changes since the last call as what anyone may see: each changed container's public
    /// view, and each removal.
    pub fn public_changes(&mut self, schema: &Schema) -> Vec<Published> {
        let Changes { changed, removed } = self.take_changes();
        let mut out = Vec::with_capacity(changed.len() + removed.len());
        for id in changed {
            if let (Some(c), Ok(props)) = (self.containers.get(&id), self.view(schema, id, Viewer::Public)) {
                out.push(Published { id, type_name: Some(c.type_name.clone()), props: Some(props), removed: false });
            }
        }
        out.extend(removed.into_iter().map(|id| Published { id, type_name: None, props: None, removed: true }));
        out
    }

    /// Publishes [`Model::public_changes`] on this hub's `topic`, one message per container,
    /// for subscribed clients. Owner and hidden properties are never published.
    pub fn publish_changes(&mut self, ctx: &Ctx, schema: &Schema, topic: &str) {
        for p in self.public_changes(schema) {
            if let Ok(bytes) = crate::encode(&p) {
                ctx.publish(topic, &bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Schema {
        Schema::new()
            .container("Character", |c| {
                c.prop("hp", PropKind::Int, 100, Visibility::Public)
                    .prop("gold", PropKind::Int, 0, Visibility::Owner)
                    .prop("weakness", PropKind::Str, "", Visibility::Hidden)
                    .prop("speed", PropKind::Float, 1.5, Visibility::Public)
            })
            .container("Item", |c| c.prop("name", PropKind::Str, "", Visibility::Public))
    }

    #[test]
    fn properties_default_and_are_type_checked() {
        let s = schema();
        let mut m = Model::new();
        let hero = m.create(&s, "Character", Some(7)).unwrap();
        assert_eq!(m.get_i64(&s, hero, "hp").unwrap(), 100);
        assert_eq!(m.add(&s, hero, "hp", -12).unwrap(), 88);
        assert!(m.set(&s, hero, "hp", "lots").is_err(), "a string is not an Int");
        assert!(m.set(&s, hero, "speed", 2).is_ok(), "an integer is a Float");
        assert!(m.set(&s, hero, "armor", 3).is_err(), "undeclared property");
        assert!(m.create(&s, "Dragon", None).is_err(), "undeclared type");
    }

    #[test]
    fn ensure_is_get_or_create_by_key() {
        let s = schema();
        let mut m = Model::new();
        let a = m.ensure(&s, "Character", "42", Some(42)).unwrap();
        assert_eq!(m.ensure(&s, "Character", "42", Some(42)).unwrap(), a);
        assert_ne!(m.ensure(&s, "Character", "43", Some(43)).unwrap(), a);
        assert_eq!(m.find("Character", "42"), Some(a));
        assert!(m.delete(a));
        assert_eq!(m.find("Character", "42"), None);
    }

    #[test]
    fn a_player_sees_public_properties_and_owner_ones_on_their_own() {
        let s = schema();
        let mut m = Model::new();
        let mine = m.create(&s, "Character", Some(7)).unwrap();
        m.set(&s, mine, "gold", 30).unwrap();
        m.set(&s, mine, "weakness", "fire").unwrap();
        let theirs = m.view(&s, mine, Viewer::Player(8)).unwrap();
        assert_eq!(theirs.keys().collect::<Vec<_>>(), ["hp", "speed"]);
        let own = m.view(&s, mine, Viewer::Player(7)).unwrap();
        assert_eq!(own.get("gold"), Some(&json!(30)));
        assert!(!own.contains_key("weakness"), "hidden from its owner too");
        assert_eq!(m.view(&s, mine, Viewer::Server).unwrap().get("weakness"), Some(&json!("fire")));
    }

    #[test]
    fn edges_link_traverse_and_go_with_their_containers() {
        let s = schema();
        let mut m = Model::new();
        let hero = m.create(&s, "Character", None).unwrap();
        let bag = m.create(&s, "Item", None).unwrap();
        let gem = m.create(&s, "Item", None).unwrap();
        m.link(hero, "holds", bag).unwrap();
        m.link(bag, "holds", gem).unwrap();
        m.link(gem, "holds", hero).unwrap();
        assert_eq!(m.linked(hero, "holds"), vec![bag]);
        assert_eq!(m.traverse(hero, "holds", 10), vec![bag, gem], "cycles end, root excluded");
        assert_eq!(m.traverse(hero, "holds", 1), vec![bag]);
        m.delete(bag);
        assert!(m.linked(hero, "holds").is_empty());
        assert!(m.link(hero, "holds", 999).is_err());
    }

    #[test]
    fn changes_publish_public_views_and_removals_once() {
        let s = schema();
        let mut m = Model::new();
        let hero = m.create(&s, "Character", Some(7)).unwrap();
        let item = m.create(&s, "Item", None).unwrap();
        m.set(&s, hero, "gold", 5).unwrap();
        m.take_changes();
        m.set(&s, hero, "hp", 90).unwrap();
        m.delete(item);
        let out = m.public_changes(&s);
        assert_eq!(out.len(), 2);
        let props = out[0].props.as_ref().unwrap();
        assert_eq!(props.get("hp"), Some(&json!(90)));
        assert!(!props.contains_key("gold") && !props.contains_key("weakness"));
        assert!(out[1].removed && out[1].id == item);
        assert!(m.public_changes(&s).is_empty());
    }

    #[test]
    fn a_model_survives_a_snapshot() {
        let s = schema();
        let mut m = Model::new();
        let hero = m.ensure(&s, "Character", "7", Some(7)).unwrap();
        let item = m.create(&s, "Item", None).unwrap();
        m.set(&s, hero, "hp", 3).unwrap();
        m.link(hero, "holds", item).unwrap();
        let back: Model = crate::decode(&crate::encode(&m).unwrap()).unwrap();
        assert_eq!(back.get_i64(&s, hero, "hp").unwrap(), 3);
        assert_eq!(back.find("Character", "7"), Some(hero));
        assert_eq!(back.linked(hero, "holds"), vec![item]);
        let mut back = back;
        assert_eq!(back.create(&s, "Item", None).unwrap(), item + 1, "ids keep counting");
    }
}
