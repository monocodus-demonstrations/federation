use crate::consts::{INTROSPECTION_SCHEMA_FIELD_NAME, INTROSPECTION_TYPE_FIELD_NAME};
use graphql_parser::query::refs::{FieldRef, SelectionRef, SelectionSetRef};
use graphql_parser::query::*;
use graphql_parser::schema::TypeDefinition;
use graphql_parser::{query, schema, Name, Pos};
use linked_hash_map::LinkedHashMap;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::iter::FromIterator;

pub fn get_operations<'q>(query: &'q Document<'q>) -> Vec<Op<'q>> {
    query
        .definitions
        .iter()
        .filter_map(|d| match d {
            Definition::Operation(op) => Some(Op {
                kind: op.kind,
                selection_set: &op.selection_set,
            }),
            Definition::SelectionSet(ss) => Some(Op {
                kind: query::Operation::Query,
                selection_set: ss,
            }),
            _ => None,
        })
        .collect()
}

pub fn build_possible_types<'a, 'q>(
    schema: &'q schema::Document<'q>,
    types: &'a HashMap<&'q str, &'q schema::TypeDefinition<'q>>,
) -> HashMap<&'q str, Vec<&'q schema::ObjectType<'q>>> {
    let mut implementing_types: HashMap<&'q str, Vec<&'q schema::ObjectType<'q>>> = HashMap::new();

    // N.B(ran) when switching `types` to LinkedHashMap for consistent ordering,
    //  The rust compiler starts complaining about lifetimes and when adding lifetime notations,
    //  it says &context doesn't live long enough in build_query_plan,
    //  even though it's not used after creating plan nodes which do not contain borrwed values.
    //  for now, for consistent ordering, we're using the schema.
    let ordered_types: Vec<&TypeDefinition> = schema
        .definitions
        .iter()
        .filter_map(|d| match d {
            schema::Definition::Type(td) => Some(td),
            _ => None,
        })
        .collect();

    for td in ordered_types {
        match td {
            TypeDefinition::Union(ref union) => {
                let objects_for_union = union
                    .types
                    .iter()
                    .map(|o_name| letp!(TypeDefinition::Object(obj) = types[*o_name] => obj))
                    .collect();
                implementing_types.insert(union.name, objects_for_union);
            }
            TypeDefinition::Object(ref obj) => {
                implementing_types.insert(obj.name, vec![obj]);

                if !obj.implements_interfaces.is_empty() {
                    let mut queue: VecDeque<&str> =
                        VecDeque::from_iter(obj.implements_interfaces.iter().cloned());

                    while !queue.is_empty() {
                        // get iface from queue.
                        let iface = queue.pop_front().unwrap();

                        // associate iface with obj
                        implementing_types
                            .entry(iface)
                            .or_insert_with(Vec::new)
                            .push(obj);

                        letp!(
                            TypeDefinition::Interface(iface) = types[iface] =>
                                for &iface in &iface.implements_interfaces {
                                    // add them to the queue.
                                    queue.push_back(iface);
                                }
                        );
                    }
                }
            }
            _ => (),
        }
    }

    implementing_types
}

pub fn names_to_types<'q>(
    schema: &'q schema::Document<'q>,
) -> HashMap<&'q str, &'q TypeDefinition<'q>> {
    schema
        .definitions
        .iter()
        .filter_map(|d| match d {
            schema::Definition::Type(td) => Some(td),
            _ => None,
        })
        .map(|td| (td.as_name(), td))
        .collect()
}

pub fn variable_name_to_def<'q>(
    query: &'q query::Document<'q>,
) -> HashMap<&'q str, &'q VariableDefinition<'q>> {
    match query
        .definitions
        .iter()
        .find(|d| matches!(d, Definition::Operation(_)))
    {
        Some(op) => {
            let defs = letp!(Definition::Operation(op) = op => &op.variable_definitions);
            defs.iter().map(|vd| (vd.name, vd)).collect()
        }
        None => HashMap::new(),
    }
}

pub(crate) fn pos() -> Pos {
    Pos { line: 0, column: 0 }
}

pub fn span() -> (Pos, Pos) {
    (pos(), pos())
}

pub fn merge_selection_sets<'q>(fields: Vec<FieldRef<'q>>) -> SelectionSetRef<'q> {
    fn merge_field_selection_sets(fields: Vec<SelectionRef>) -> Vec<SelectionRef> {
        let (field_nodes, fragment_nodes): (Vec<SelectionRef>, Vec<SelectionRef>) =
            fields.into_iter().partition(|s| s.is_field());

        let (aliased_field_nodes, non_aliased_field_nodes): (Vec<SelectionRef>, Vec<SelectionRef>) =
            field_nodes.into_iter().partition(|s| s.is_aliased_field());

        let nodes_by_same_name = group_by(non_aliased_field_nodes, |s| match s {
            SelectionRef::Ref(Selection::Field(Field { name, .. })) => *name,
            SelectionRef::Field(Field { name, .. }) => *name,
            SelectionRef::FieldRef(FieldRef { name, .. }) => *name,
            _ => unreachable!(),
        });

        let merged_field_nodes = values!(iter nodes_by_same_name).map(|nodes_with_same_name| {
            let nothing_to_do = nodes_with_same_name.len() == 1
                || nodes_with_same_name[0].no_or_empty_selection_set();

            if !nothing_to_do {
                let (head, tail) = nodes_with_same_name.head();

                let mut field_ref = match head {
                    SelectionRef::FieldRef(f) => f,
                    SelectionRef::Field(f) => field_ref!(f),
                    SelectionRef::Ref(Selection::Field(f)) => field_ref!(f),
                    _ => unreachable!(),
                };

                let head_items = std::mem::replace(&mut field_ref.selection_set.items, vec![]);

                let items = merge_field_selection_sets(
                    head_items
                        .into_iter()
                        .chain(
                            tail.into_iter()
                                .flat_map(|s| s.into_fields_selection_set())
                                .flat_map(|ss| ss.items),
                        )
                        .collect(),
                );

                field_ref.selection_set.items = items;

                SelectionRef::FieldRef(field_ref)
            } else {
                nodes_with_same_name.head().0
            }
        });

        merged_field_nodes
            .chain(aliased_field_nodes)
            .chain(fragment_nodes)
            .collect()
    }

    let selections = fields
        .into_iter()
        .map(|f| f.selection_set)
        .flat_map(|ss| ss.items)
        .collect();

    let items: Vec<SelectionRef<'q>> = merge_field_selection_sets(selections);

    SelectionSetRef {
        span: span(),
        items,
    }
}

pub fn group_by<T, K, F>(v: Vec<T>, f: F) -> LinkedHashMap<K, Vec<T>>
where
    F: Fn(&T) -> K,
    K: Hash + PartialEq + Eq,
{
    let mut map: LinkedHashMap<K, Vec<T>> = LinkedHashMap::new();
    for element in v.into_iter() {
        map.entry(f(&element)).or_insert(vec![]).push(element)
    }
    map
}

// https://github.com/graphql/graphql-js/blob/7b3241329e1ff49fb647b043b80568f0cf9e1a7c/src/type/introspection.js#L500-L509
pub fn is_introspection_type(name: &str) -> bool {
    name == "__Schema"
        || name == "__Directive"
        || name == "__DirectiveLocation"
        || name == "__Type"
        || name == "__Field"
        || name == "__InputValue"
        || name == "__EnumValue"
        || name == "__TypeKind"
}

pub fn is_not_introspection_field(selection: &SelectionRef) -> bool {
    match *selection {
        SelectionRef::FieldRef(ref field) => {
            field.name != INTROSPECTION_SCHEMA_FIELD_NAME
                && field.name != INTROSPECTION_TYPE_FIELD_NAME
        }
        SelectionRef::Field(field) | SelectionRef::Ref(Selection::Field(field)) => {
            field.name != INTROSPECTION_SCHEMA_FIELD_NAME
                && field.name != INTROSPECTION_TYPE_FIELD_NAME
        }
        _ => true,
    }
}

pub trait Head<T> {
    /// gets the head and tail of a vector
    fn head(self) -> (T, Vec<T>);
}

impl<T> Head<T> for Vec<T> {
    fn head(self) -> (T, Vec<T>) {
        if self.is_empty() {
            panic!("head must be called on a non empty Vec")
        } else {
            let mut iter = self.into_iter();
            (iter.next().unwrap(), iter.collect())
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Op<'q> {
    pub selection_set: &'q SelectionSet<'q>,
    pub kind: query::Operation,
}

pub enum NodeCollectionKind {
    Sequence,
    Parallel,
}
