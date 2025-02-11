/*
 * Copyright 2022-2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Defines structures for entity type and action id information used by the
//! validator. The contents of these structures should be populated from and schema
//! with a few transformations applied to the data. Specifically, the
//! `member_of` relation from the schema is reversed and the transitive closure is
//! computed to obtain a `descendants` relation.

use std::collections::{hash_map::Entry, HashMap, HashSet};

use cedar_policy_core::{
    ast::{Eid, EntityType, EntityUID, Id, Name},
    entities::JSONValue,
    parser::{err::ParseError, parse_name, parse_namespace},
    transitive_closure::{compute_tc, TCNode},
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use smol_str::SmolStr;

use crate::{
    schema_file_format,
    types::{AttributeType, Attributes, EntityRecordKind, Type},
    ActionEntityUID, ActionType, SchemaFragment, SchemaType, SchemaTypeVariant, TypeOfAttribute,
    SCHEMA_TYPE_VARIANT_TAGS,
};

use super::err::*;
use super::NamespaceDefinition;

/// The current schema format specification does not include multiple action entity
/// types. All action entities are required to use a single `Action` entity
/// type. However, the action entity type may be namespaced, so an action entity
/// may have a fully qualified entity type `My::Namespace::Action`.
pub(crate) static ACTION_ENTITY_TYPE: &str = "Action";

/// Return true when an entity type is an action entity type. This compares the
/// base name for the type, so this will return true for any entity type named
/// `Action` regardless of namespaces.
pub(crate) fn is_action_entity_type(ty: &Name) -> bool {
    ty.basename().as_ref() == ACTION_ENTITY_TYPE
}

// We do not have a dafny model for action attributes, so we disable them by defualt.
#[derive(Eq, PartialEq, Copy, Clone, Default)]
pub enum ActionBehavior {
    /// Action entities cannot have attributes. Attempting to declare attributes
    /// will result in a error when constructing the schema.
    #[default]
    ProhibitAttributes,
    /// Action entities may have attributes.
    PermitAttributes,
}

/// A single namespace definition from the schema json processed into a form
/// which is closer to that used by the validator. The processing includes
/// detection of some errors, for example, parse errors in entity type names or
/// entity type which are declared multiple times. This does not detect
/// references to undeclared entity types because any entity type may be
/// declared in a different fragment that will only be known about when building
/// the complete `ValidatorSchema`.
#[derive(Debug)]
pub struct ValidatorNamespaceDef {
    /// The namespace declared for the schema fragment. We track a namespace for
    /// fragments because they have at most one namespace that is applied
    /// everywhere. It would be less useful to track all namespaces for a
    /// complete schema.
    namespace: Option<Name>,
    /// Preprocessed common type definitions which can be used to define entity
    /// type attributes and action contexts.
    type_defs: TypeDefs,
    /// The preprocessed entity type declarations from the schema fragment json.
    entity_types: EntityTypesDef,
    /// The preprocessed action declarations from the schema fragment json.
    actions: ActionsDef,
}

/// Holds a map from `Name`s of common type definitions to their corresponding
/// `Type`.
#[derive(Debug)]
pub struct TypeDefs {
    type_defs: HashMap<Name, Type>,
}

/// Entity type declarations held in a `ValidatorNamespaceDef`. Entity type
/// children and attributes may reference undeclared entity types.
#[derive(Debug)]
pub struct EntityTypesDef {
    /// Entity type attributes and children are tracked separately because a
    /// child of an entity type may be declared in a fragment without also
    /// declaring the entity type and its attributes. Attribute types are
    /// wrapped in a `WithUnresolvedTypeDefs` because types may contain
    /// typedefs which are not defined in this schema fragment. All
    /// entity type `Name` keys in this map are declared in this schema fragment.
    attributes: HashMap<Name, WithUnresolvedTypeDefs<Type>>,
    /// `Name`s which are keys in this map appear inside `memberOf` lists, so
    /// they might not declared in this fragment. We will check if they are
    /// declared in any fragment when constructing a `ValidatorSchema`. The
    /// children are taken from the entity type declaration where the `memberOf`
    /// list appeared, so we know that they were declared in this fragment.
    /// This map contains children rather than descendants because the
    /// transitive closure has not yet been computed. We need all child edges
    /// between entity types from all fragments before we can compute the
    /// transitive closure.
    children: HashMap<Name, HashSet<Name>>,
}

/// Action declarations held in a `ValidatorNamespaceDef`. Entity types
/// referenced here do not need to be declared in the schema.
#[derive(Debug)]
pub struct ActionsDef {
    /// Action declaration components are tracked separately for the same reasons
    /// as entity types. This map holds attributes and apply-specs because these
    /// are always fully defined in the schema fragment containing the action
    /// declarations. The attribute types are wrapped in a `WithUnresolvedTypeDefs` because they
    /// may refer to common types which are not defined in this fragment. The `EntityUID` keys in
    /// this map again were definitely declared in this fragment.
    context_applies_to: HashMap<EntityUID, (WithUnresolvedTypeDefs<Type>, ValidatorApplySpec)>,
    /// `EntityUID` keys in this map appear inside action `memberOf` lists, so
    /// they might not be declared in this fragment while the entries in the
    /// values hash set are taken directly from declared actions.
    children: HashMap<EntityUID, HashSet<EntityUID>>,
    /// Action attributes
    attributes: HashMap<EntityUID, Attributes>,
}

type ResolveFunc<T> = dyn FnOnce(&HashMap<Name, Type>) -> Result<T>;
/// Represent a type that might be defined in terms of some type definitions
/// which are not necessarily available in the current namespace.
pub enum WithUnresolvedTypeDefs<T> {
    WithUnresolved(Box<ResolveFunc<T>>),
    WithoutUnresolved(T),
}

impl<T: 'static> WithUnresolvedTypeDefs<T> {
    pub fn new(f: impl FnOnce(&HashMap<Name, Type>) -> Result<T> + 'static) -> Self {
        Self::WithUnresolved(Box::new(f))
    }

    pub fn map<U: 'static>(self, f: impl FnOnce(T) -> U + 'static) -> WithUnresolvedTypeDefs<U> {
        match self {
            Self::WithUnresolved(_) => {
                WithUnresolvedTypeDefs::new(|type_defs| self.resolve_type_defs(type_defs).map(f))
            }
            Self::WithoutUnresolved(v) => WithUnresolvedTypeDefs::WithoutUnresolved(f(v)),
        }
    }

    /// Instantiate any names referencing types with the definition of the type
    /// from the input HashMap.
    pub fn resolve_type_defs(self, type_defs: &HashMap<Name, Type>) -> Result<T> {
        match self {
            WithUnresolvedTypeDefs::WithUnresolved(f) => f(type_defs),
            WithUnresolvedTypeDefs::WithoutUnresolved(v) => Ok(v),
        }
    }
}

impl<T: 'static> From<T> for WithUnresolvedTypeDefs<T> {
    fn from(value: T) -> Self {
        Self::WithoutUnresolved(value)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for WithUnresolvedTypeDefs<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WithUnresolvedTypeDefs::WithUnresolved(_) => f.debug_tuple("WithUnresolved").finish(),
            WithUnresolvedTypeDefs::WithoutUnresolved(v) => {
                f.debug_tuple("WithoutUnresolved").field(v).finish()
            }
        }
    }
}

impl TryInto<ValidatorNamespaceDef> for NamespaceDefinition {
    type Error = SchemaError;

    fn try_into(self) -> Result<ValidatorNamespaceDef> {
        ValidatorNamespaceDef::from_namespace_definition(None, self, ActionBehavior::default())
    }
}

impl ValidatorNamespaceDef {
    // We need to treat this as if it had `pub(crate)` visibility to avoid sharing
    // the file format. However, our fuzzing library currently needs it to be public.
    /// Construct a new `ValidatorSchema` from the underlying `SchemaFragment`.
    pub fn from_namespace_definition(
        namespace: Option<SmolStr>,
        namespace_def: NamespaceDefinition,
        action_behavior: ActionBehavior,
    ) -> Result<ValidatorNamespaceDef> {
        // Check that each entity types and action is only declared once.
        let mut e_types_ids: HashSet<SmolStr> = HashSet::new();
        for name in namespace_def.entity_types.keys() {
            if !e_types_ids.insert(name.clone()) {
                // insert returns false for duplicates
                return Err(SchemaError::DuplicateEntityType(name.to_string()));
            }
        }
        let mut a_name_eids: HashSet<SmolStr> = HashSet::new();
        for name in namespace_def.actions.keys() {
            if !a_name_eids.insert(name.clone()) {
                // insert returns false for duplicates
                return Err(SchemaError::DuplicateAction(name.to_string()));
            }
        }

        let schema_namespace = namespace
            .as_ref()
            .map(|ns| parse_namespace(ns).map_err(SchemaError::NamespaceParseError))
            .transpose()?
            .unwrap_or_default();

        // Return early with an error if actions cannot be in groups or have
        // attributes, but the schema contains action groups or attributes.
        Self::check_action_behavior(&namespace_def, action_behavior)?;

        // Convert the type defs, actions and entity types from the schema file
        // into the representation used by the validator.
        let type_defs =
            Self::build_type_defs(namespace_def.common_types, schema_namespace.as_slice())?;
        let actions = Self::build_action_ids(namespace_def.actions, schema_namespace.as_slice())?;
        let entity_types =
            Self::build_entity_types(namespace_def.entity_types, schema_namespace.as_slice())?;

        Ok(ValidatorNamespaceDef {
            namespace: {
                let mut schema_namespace = schema_namespace;
                schema_namespace
                    .pop()
                    .map(|last| Name::new(last, schema_namespace))
            },
            type_defs,
            entity_types,
            actions,
        })
    }

    fn is_builtin_type_name(name: &SmolStr) -> bool {
        SCHEMA_TYPE_VARIANT_TAGS
            .iter()
            .any(|type_name| name == type_name)
    }

    fn build_type_defs(
        schema_file_type_def: HashMap<SmolStr, SchemaType>,
        schema_namespace: &[Id],
    ) -> Result<TypeDefs> {
        let type_defs = schema_file_type_def
            .into_iter()
            .map(|(name_str, schema_ty)| -> Result<_> {
                if Self::is_builtin_type_name(&name_str) {
                    return Err(SchemaError::DuplicateCommonType(name_str.to_string()));
                }
                let name = Self::parse_unqualified_name_with_namespace(
                    &name_str,
                    schema_namespace.to_vec(),
                )
                .map_err(SchemaError::CommonTypeParseError)?;
                let ty = Self::try_schema_type_into_validator_type(schema_namespace, schema_ty)?
                    .resolve_type_defs(&HashMap::new())?;
                Ok((name, ty))
            })
            .collect::<Result<HashMap<_, _>>>()?;
        Ok(TypeDefs { type_defs })
    }

    // Transform the schema data structures for entity types into the structures
    // used internally by the validator. This is mostly accomplished by directly
    // copying data between fields. For the `descendants` this field, we first
    // reverse the direction of the `member_of` relation and then compute the
    // transitive closure.
    fn build_entity_types(
        schema_files_types: HashMap<SmolStr, schema_file_format::EntityType>,
        schema_namespace: &[Id],
    ) -> Result<EntityTypesDef> {
        // Invert the `member_of` relationship, associating each entity type
        // with its set of children instead of parents.
        let mut children: HashMap<Name, HashSet<Name>> = HashMap::new();
        for (name, e) in &schema_files_types {
            for parent in &e.member_of_types {
                let parent_type_name = Self::parse_possibly_qualified_name_with_default_namespace(
                    parent,
                    schema_namespace,
                )
                .map_err(SchemaError::EntityTypeParseError)?;
                children
                    .entry(parent_type_name)
                    .or_insert_with(HashSet::new)
                    .insert(
                        Self::parse_unqualified_name_with_namespace(
                            name,
                            schema_namespace.to_vec(),
                        )
                        .map_err(SchemaError::EntityTypeParseError)?,
                    );
            }
        }

        let attributes = schema_files_types
            .into_iter()
            .map(|(name, e)| -> Result<_> {
                let name: Name =
                    Self::parse_unqualified_name_with_namespace(&name, schema_namespace.to_vec())
                        .map_err(SchemaError::EntityTypeParseError)?;

                let attributes = Self::try_schema_type_into_validator_type(
                    schema_namespace,
                    e.shape.into_inner(),
                )?;

                Ok((name, attributes))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(EntityTypesDef {
            attributes,
            children,
        })
    }

    //Helper to get types from JSONValues
    //Currently doesn't support all JSONValue types
    fn jsonval_to_type_helper(v: &JSONValue) -> Result<Type> {
        match v {
            JSONValue::Bool(_) => Ok(Type::primitive_boolean()),
            JSONValue::Long(_) => Ok(Type::primitive_long()),
            JSONValue::String(_) => Ok(Type::primitive_string()),
            JSONValue::Record(r) => {
                let mut required_attrs: HashMap<SmolStr, Type> = HashMap::new();
                for (k, v_prime) in r {
                    let t = Self::jsonval_to_type_helper(v_prime);
                    match t {
                        Ok(ty) => required_attrs.insert(k.clone(), ty),
                        Err(e) => return Err(e),
                    };
                }
                Ok(Type::EntityOrRecord(EntityRecordKind::Record {
                    attrs: Attributes::with_required_attributes(required_attrs),
                }))
            }
            JSONValue::Set(v) => match v.get(0) {
                //sets with elements of different types will be rejected elsewhere
                None => Err(SchemaError::ActionEntityAttributeEmptySet),
                Some(element) => {
                    let element_type = Self::jsonval_to_type_helper(element);
                    match element_type {
                        Ok(t) => Ok(Type::Set {
                            element_type: Some(Box::new(t)),
                        }),
                        Err(_) => element_type,
                    }
                }
            },
            _ => Err(SchemaError::ActionEntityAttributeUnsupportedType),
        }
    }

    //Convert jsonval map to attributes
    fn convert_attr_jsonval_map_to_attributes(
        m: HashMap<SmolStr, JSONValue>,
    ) -> Result<Attributes> {
        let mut required_attrs: HashMap<SmolStr, Type> = HashMap::new();

        for (k, v) in m {
            let t = Self::jsonval_to_type_helper(&v);
            match t {
                Ok(ty) => required_attrs.insert(k.clone(), ty),
                Err(e) => return Err(e),
            };
        }
        Ok(Attributes::with_required_attributes(required_attrs))
    }

    // Transform the schema data structures for actions into the structures used
    // internally by the validator. This is mostly accomplished by directly
    // copying data between fields, except that for the `descendants` field we
    // first reverse the direction of the `member_of` relation and then compute
    // the transitive closure.
    fn build_action_ids(
        schema_file_actions: HashMap<SmolStr, ActionType>,
        schema_namespace: &[Id],
    ) -> Result<ActionsDef> {
        // Invert the `member_of` relationship, associating each entity and
        // action with it's set of children instead of parents.
        let mut children: HashMap<EntityUID, HashSet<EntityUID>> = HashMap::new();
        for (name, a) in &schema_file_actions {
            let parents = match &a.member_of {
                Some(parents) => parents,
                None => continue,
            };
            for parent in parents {
                let parent_euid =
                    Self::parse_action_id_with_namespace(parent, schema_namespace.to_vec())?;
                children
                    .entry(parent_euid)
                    .or_insert_with(HashSet::new)
                    .insert(Self::parse_action_id_with_namespace(
                        &ActionEntityUID::default_type(name.clone()),
                        schema_namespace.to_vec(),
                    )?);
            }
        }

        let context_applies_to = schema_file_actions
            .clone()
            .into_iter()
            .map(|(name, a)| -> Result<_> {
                let action_euid = Self::parse_action_id_with_namespace(
                    &ActionEntityUID::default_type(name),
                    schema_namespace.to_vec(),
                )?;

                let (principal_types, resource_types, context) = a
                    .applies_to
                    .map(|applies_to| {
                        (
                            applies_to.principal_types,
                            applies_to.resource_types,
                            applies_to.context,
                        )
                    })
                    .unwrap_or_default();

                // Convert the entries in the `appliesTo` lists into sets of
                // `EntityTypes`. If one of the lists is `None` (absent from the
                // schema), then the specification is undefined.
                let action_applies_to = ValidatorApplySpec::new(
                    Self::parse_apply_spec_type_list(principal_types, schema_namespace)?,
                    Self::parse_apply_spec_type_list(resource_types, schema_namespace)?,
                );

                let action_context = Self::try_schema_type_into_validator_type(
                    schema_namespace,
                    context.into_inner(),
                )?;

                Ok((action_euid, (action_context, action_applies_to)))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        let attributes = schema_file_actions
            .into_iter()
            .map(|(name, a)| -> Result<_> {
                let action_euid = Self::parse_action_id_with_namespace(
                    &ActionEntityUID::default_type(name),
                    schema_namespace.to_vec(),
                )?;

                let action_attributes =
                    Self::convert_attr_jsonval_map_to_attributes(a.attributes.unwrap_or_default());
                match action_attributes {
                    // We can't just use the last element of the vec without implementing `Clone` for `SchemaError`, which has some potentially very expensive variants
                    Ok(attrs) => Ok((action_euid, attrs)),
                    Err(e) => Err(e),
                }
            })
            .collect::<Result<HashMap<_, _>>>()?;

        Ok(ActionsDef {
            context_applies_to,
            children,
            attributes,
        })
    }

    // Check that `schema_file` uses actions in a way consistent with the
    // specified `action_behavior`. When the behavior specifies that actions
    // should not be used in groups and should not have attributes, then this
    // function will return `Err` if it sees any action groups or attributes
    // declared in the schema.
    fn check_action_behavior(
        schema_file: &NamespaceDefinition,
        action_behavior: ActionBehavior,
    ) -> Result<()> {
        if schema_file
            .entity_types
            .iter()
            // The `name` in an entity type declaration cannot be qualified
            // with a namespace (it always implicitly takes the schema
            // namespace), so we do this comparison directly.
            .any(|(name, _)| name == ACTION_ENTITY_TYPE)
        {
            return Err(SchemaError::ActionEntityTypeDeclared);
        }
        if action_behavior == ActionBehavior::ProhibitAttributes {
            let mut actions_with_attributes: Vec<String> = Vec::new();
            for (name, a) in &schema_file.actions {
                if a.attributes.is_some() {
                    actions_with_attributes.push(name.to_string());
                }
            }
            if !actions_with_attributes.is_empty() {
                return Err(SchemaError::ActionEntityAttributes(actions_with_attributes));
            }
        }

        Ok(())
    }

    /// Given the attributes for an entity type or action context as written in
    /// a schema file, convert the types of the attributes into the `Type` data
    /// structure used by the typechecker, and return the result as a map from
    /// attribute name to type.
    fn parse_record_attributes(
        schema_namespace: &[Id],
        attrs: impl IntoIterator<Item = (SmolStr, TypeOfAttribute)>,
    ) -> Result<WithUnresolvedTypeDefs<Attributes>> {
        let attrs_with_type_defs = attrs
            .into_iter()
            .map(|(attr, ty)| -> Result<_> {
                Ok((
                    attr,
                    (
                        Self::try_schema_type_into_validator_type(schema_namespace, ty.ty)?,
                        ty.required,
                    ),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(WithUnresolvedTypeDefs::new(|typ_defs| {
            attrs_with_type_defs
                .into_iter()
                .map(|(s, (attr_ty, is_req))| {
                    attr_ty
                        .resolve_type_defs(typ_defs)
                        .map(|ty| (s, AttributeType::new(ty, is_req)))
                })
                .collect::<Result<Vec<_>>>()
                .map(Attributes::with_attributes)
        }))
    }

    /// Take an optional list of entity type name strings from an action apply
    /// spec and parse it into a set of `Name`s for those entity types. If any
    /// of the entity type names cannot be parsed, then the `Err` case is
    /// returned, and it will indicate which name did not parse.
    fn parse_apply_spec_type_list(
        types: Option<Vec<SmolStr>>,
        namespace: &[Id],
    ) -> Result<HashSet<EntityType>> {
        types
            .map(|types| {
                types
                    .iter()
                    // Parse each type name string into a `Name`, generating an
                    // `EntityTypeParseError` when the string is not a valid
                    // name.
                    .map(|ty_str| {
                        Ok(EntityType::Concrete(
                            Self::parse_possibly_qualified_name_with_default_namespace(
                                ty_str, namespace,
                            )
                            .map_err(SchemaError::EntityTypeParseError)?,
                        ))
                    })
                    // Fail if any of the types failed.
                    .collect::<Result<HashSet<_>>>()
            })
            .unwrap_or_else(|| Ok(HashSet::from([EntityType::Unspecified])))
    }

    // Parse a `Name` from a string (possibly including namespaces). If it is
    // not qualified with any namespace, then apply the  default namespace to
    // create a qualified name.  Do not modify any existing namespace on the
    // type.
    pub(crate) fn parse_possibly_qualified_name_with_default_namespace(
        name_str: &SmolStr,
        default_namespace: &[Id],
    ) -> std::result::Result<Name, Vec<ParseError>> {
        let name = parse_name(name_str)?;

        let qualified_name =
            if name.namespace_components().next().is_none() && !default_namespace.is_empty() {
                // The name does not have a namespace, and the schema has a
                // namespace declared, so qualify the type to use the default.
                Name::new(name.basename().clone(), default_namespace.to_vec())
            } else {
                // The name is already qualified. Don't touch it.
                name
            };

        Ok(qualified_name)
    }

    /// Parse a name from a string into the `Id` (basename only).  Then
    /// initialize the namespace for this type with the provided namespace vec
    /// to create the qualified `Name`.
    fn parse_unqualified_name_with_namespace(
        type_name: &SmolStr,
        namespace: Vec<Id>,
    ) -> std::result::Result<Name, Vec<ParseError>> {
        Ok(Name::new(type_name.parse()?, namespace))
    }

    /// Take an action identifier as a string and use it to construct an
    /// EntityUID for that action. The entity type of the action will always
    /// have the base type `Action`. The type will be qualified with any
    /// namespace provided in the `namespace` argument or with the namespace
    /// inside the ActionEntityUID if one is present.
    fn parse_action_id_with_namespace(
        action_id: &ActionEntityUID,
        namespace: Vec<Id>,
    ) -> Result<EntityUID> {
        let namespaced_action_type = if let Some(action_ty) = &action_id.ty {
            action_ty
                .parse()
                .map_err(SchemaError::EntityTypeParseError)?
        } else {
            Name::new(
                ACTION_ENTITY_TYPE.parse().expect(
                    "Expected that the constant ACTION_ENTITY_TYPE would be a valid entity type.",
                ),
                namespace,
            )
        };
        Ok(EntityUID::from_components(
            namespaced_action_type,
            Eid::new(action_id.id.clone()),
        ))
    }

    /// Implemented to convert a type as written in the schema json format into the
    /// `Type` type used by the validator. Conversion can fail if an entity or
    /// record attribute name is invalid. It will also fail for some types that can
    /// be written in the schema, but are not yet implemented in the typechecking
    /// logic.
    pub(crate) fn try_schema_type_into_validator_type(
        default_namespace: &[Id],
        schema_ty: SchemaType,
    ) -> Result<WithUnresolvedTypeDefs<Type>> {
        match schema_ty {
            SchemaType::Type(SchemaTypeVariant::String) => Ok(Type::primitive_string().into()),
            SchemaType::Type(SchemaTypeVariant::Long) => Ok(Type::primitive_long().into()),
            SchemaType::Type(SchemaTypeVariant::Boolean) => Ok(Type::primitive_boolean().into()),
            SchemaType::Type(SchemaTypeVariant::Set { element }) => Ok(
                Self::try_schema_type_into_validator_type(default_namespace, *element)?
                    .map(Type::set),
            ),
            SchemaType::Type(SchemaTypeVariant::Record {
                attributes,
                additional_attributes,
            }) => {
                if additional_attributes {
                    Err(SchemaError::UnsupportedSchemaFeature(
                        UnsupportedFeature::OpenRecordsAndEntities,
                    ))
                } else {
                    Ok(
                        Self::parse_record_attributes(default_namespace, attributes)?
                            .map(Type::record_with_attributes),
                    )
                }
            }
            SchemaType::Type(SchemaTypeVariant::Entity { name }) => {
                let entity_type_name = Self::parse_possibly_qualified_name_with_default_namespace(
                    &name,
                    default_namespace,
                )
                .map_err(SchemaError::EntityTypeParseError)?;
                Ok(Type::named_entity_reference(entity_type_name).into())
            }
            SchemaType::Type(SchemaTypeVariant::Extension { name }) => {
                let extension_type_name =
                    name.parse().map_err(SchemaError::ExtensionTypeParseError)?;
                Ok(Type::extension(extension_type_name).into())
            }
            SchemaType::TypeDef { type_name } => {
                let defined_type_name = Self::parse_possibly_qualified_name_with_default_namespace(
                    &type_name,
                    default_namespace,
                )
                .map_err(SchemaError::CommonTypeParseError)?;
                Ok(WithUnresolvedTypeDefs::new(move |typ_defs| {
                    typ_defs.get(&defined_type_name).cloned().ok_or(
                        SchemaError::UndeclaredCommonType(HashSet::from([type_name.to_string()])),
                    )
                }))
            }
        }
    }

    /// Access the `Name` for the namespace of this definition.
    pub fn namespace(&self) -> &Option<Name> {
        &self.namespace
    }
}

#[derive(Debug)]
pub struct ValidatorSchemaFragment(Vec<ValidatorNamespaceDef>);

impl TryInto<ValidatorSchemaFragment> for SchemaFragment {
    type Error = SchemaError;

    fn try_into(self) -> Result<ValidatorSchemaFragment> {
        ValidatorSchemaFragment::from_schema_fragment(self, ActionBehavior::default())
    }
}

impl ValidatorSchemaFragment {
    pub fn from_namespaces(namespaces: impl IntoIterator<Item = ValidatorNamespaceDef>) -> Self {
        Self(namespaces.into_iter().collect())
    }

    pub fn from_schema_fragment(
        fragment: SchemaFragment,
        action_behavior: ActionBehavior,
    ) -> Result<Self> {
        Ok(Self(
            fragment
                .0
                .into_iter()
                .map(|(fragment_ns, ns_def)| {
                    ValidatorNamespaceDef::from_namespace_definition(
                        Some(fragment_ns),
                        ns_def,
                        action_behavior,
                    )
                })
                .collect::<Result<Vec<_>>>()?,
        ))
    }

    /// Access the `Name`s for the namespaces in this fragment.
    pub fn namespaces(&self) -> impl Iterator<Item = &Option<Name>> {
        self.0.iter().map(|d| d.namespace())
    }
}

#[serde_as]
#[derive(Clone, Debug, Serialize)]
pub struct ValidatorSchema {
    /// Map from entity type names to the ValidatorEntityType object.
    #[serde(rename = "entityTypes")]
    #[serde_as(as = "Vec<(_, _)>")]
    entity_types: HashMap<Name, ValidatorEntityType>,

    /// Map from action id names to the ValidatorActionId object.
    #[serde(rename = "actionIds")]
    #[serde_as(as = "Vec<(_, _)>")]
    action_ids: HashMap<EntityUID, ValidatorActionId>,
}

impl std::str::FromStr for ValidatorSchema {
    type Err = SchemaError;

    fn from_str(s: &str) -> Result<Self> {
        serde_json::from_str::<SchemaFragment>(s)?.try_into()
    }
}

impl TryInto<ValidatorSchema> for NamespaceDefinition {
    type Error = SchemaError;

    fn try_into(self) -> Result<ValidatorSchema> {
        ValidatorSchema::from_schema_fragments([ValidatorSchemaFragment::from_namespaces([self
            .try_into(
        )?])])
    }
}

impl TryInto<ValidatorSchema> for SchemaFragment {
    type Error = SchemaError;

    fn try_into(self) -> Result<ValidatorSchema> {
        ValidatorSchema::from_schema_fragments([self.try_into()?])
    }
}

impl ValidatorSchema {
    // Create a ValidatorSchema without any entity types or actions ids.
    pub fn empty() -> ValidatorSchema {
        Self {
            entity_types: HashMap::new(),
            action_ids: HashMap::new(),
        }
    }

    /// Construct a `ValidatorSchema` from a JSON value (which should be an
    /// object matching the `SchemaFileFormat` shape).
    pub fn from_json_value(json: serde_json::Value) -> Result<Self> {
        Self::from_schema_file(
            SchemaFragment::from_json_value(json)?,
            ActionBehavior::default(),
        )
    }

    /// Construct a `ValidatorSchema` directly from a file.
    pub fn from_file(file: impl std::io::Read) -> Result<Self> {
        Self::from_schema_file(SchemaFragment::from_file(file)?, ActionBehavior::default())
    }

    pub fn from_schema_file(
        schema_file: SchemaFragment,
        action_behavior: ActionBehavior,
    ) -> Result<ValidatorSchema> {
        Self::from_schema_fragments([ValidatorSchemaFragment::from_schema_fragment(
            schema_file,
            action_behavior,
        )?])
    }

    /// Construct a new `ValidatorSchema` from some number of schema fragments.
    pub fn from_schema_fragments(
        fragments: impl IntoIterator<Item = ValidatorSchemaFragment>,
    ) -> Result<ValidatorSchema> {
        let mut type_defs = HashMap::new();
        let mut entity_attributes = HashMap::new();
        let mut entity_children = HashMap::new();
        let mut action_context_applies_to = HashMap::new();
        let mut action_children = HashMap::new();
        let mut action_attributes = HashMap::new();

        for ns_def in fragments.into_iter().flat_map(|f| f.0.into_iter()) {
            for (name, ty) in ns_def.type_defs.type_defs {
                match type_defs.entry(name) {
                    Entry::Vacant(v) => v.insert(ty),
                    Entry::Occupied(o) => {
                        return Err(SchemaError::DuplicateCommonType(o.key().to_string()));
                    }
                };
            }

            // Build aggregate maps for the declared entity/action attributes and
            // action context/applies_to lists, checking that no action or
            // entity type is declared more than once.  Namespaces were already
            // added by the `ValidatorNamespaceDef`, so the same base type
            // name may appear multiple times so long as the namespaces are
            // different.
            for (name, attrs) in ns_def.entity_types.attributes {
                match entity_attributes.entry(name) {
                    Entry::Vacant(v) => v.insert(attrs),
                    Entry::Occupied(o) => {
                        return Err(SchemaError::DuplicateEntityType(o.key().to_string()))
                    }
                };
            }
            for (id, context_applies_to) in ns_def.actions.context_applies_to {
                match action_context_applies_to.entry(id) {
                    Entry::Vacant(v) => v.insert(context_applies_to),
                    Entry::Occupied(o) => {
                        return Err(SchemaError::DuplicateAction(o.key().to_string()))
                    }
                };
            }
            for (id, attrs) in ns_def.actions.attributes {
                match action_attributes.entry(id) {
                    Entry::Vacant(v) => v.insert(attrs),
                    Entry::Occupied(o) => {
                        return Err(SchemaError::DuplicateAction(o.key().to_string()))
                    }
                };
            }

            // Now build aggregate children maps. There may be keys duplicated
            // between the fragments if an entity type has child entity type
            // declared in multiple fragments.
            for (name, children) in ns_def.entity_types.children {
                let current_children: &mut HashSet<_> = entity_children.entry(name).or_default();
                for child in children {
                    current_children.insert(child);
                }
            }

            for (id, children) in ns_def.actions.children {
                let current_children: &mut HashSet<_> = action_children.entry(id).or_default();
                for child in children {
                    current_children.insert(child);
                }
            }
        }

        let mut entity_types = entity_attributes
            .into_iter()
            .map(|(name, attributes)| -> Result<_> {
                // Keys of the `entity_children` map were values of an
                // `memberOfTypes` list, so they might not have been declared in
                // their fragment.  By removing entries from `entity_children`
                // where the key is a declared name, we will be left with a map
                // where the keys are undeclared. These keys are used to report
                // an error when undeclared entity types are referenced inside a
                // `memberOfTypes` list. The error is reported alongside the
                // error for any other undeclared entity types by
                // `check_for_undeclared`.
                let descendants = entity_children.remove(&name).unwrap_or_default();
                Ok((
                    name.clone(),
                    ValidatorEntityType {
                        name,
                        descendants,
                        attributes: Self::record_attributes_or_error(
                            attributes.resolve_type_defs(&type_defs)?,
                        )?,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        let mut action_ids = action_context_applies_to
            .into_iter()
            .map(|(name, (context, applies_to))| -> Result<_> {
                let descendants = action_children.remove(&name).unwrap_or_default();

                let attributes = match action_attributes.get(&name) {
                    Some(t) => t.clone(),
                    None => Attributes::with_attributes([]),
                };

                Ok((
                    name.clone(),
                    ValidatorActionId {
                        name,
                        applies_to,
                        descendants,
                        context: Self::record_attributes_or_error(
                            context.resolve_type_defs(&type_defs)?,
                        )?,
                        attributes,
                    },
                ))
            })
            .collect::<Result<HashMap<_, _>>>()?;

        // We constructed entity types and actions with child maps, but we need
        // transitively closed descendants.
        compute_tc(&mut entity_types, false)?;
        // Pass `true` here so that we also check that the action hierarchy does
        // not contain cycles.
        compute_tc(&mut action_ids, true)?;

        // Return with an error if there is an undeclared entity or action
        // referenced in any fragment. `{entity,action}_children` are provided
        // for the `undeclared_parent_{entities,actions}` arguments because
        // removed keys from these maps as we encountered declarations for the
        // entity types or actions. Any keys left in the map are therefore
        // undeclared.
        Self::check_for_undeclared(
            &entity_types,
            entity_children.into_keys(),
            &action_ids,
            action_children.into_keys(),
        )?;

        Ok(ValidatorSchema {
            entity_types,
            action_ids,
        })
    }

    /// Check that all entity types and actions referenced in the schema are in
    /// the set of declared entity type or action names. Point of caution: this
    /// function assumes that all entity types are fully qualified. This is
    /// handled by the `SchemaFragment` constructor.
    fn check_for_undeclared(
        entity_types: &HashMap<Name, ValidatorEntityType>,
        undeclared_parent_entities: impl IntoIterator<Item = Name>,
        action_ids: &HashMap<EntityUID, ValidatorActionId>,
        undeclared_parent_actions: impl IntoIterator<Item = EntityUID>,
    ) -> Result<()> {
        // When we constructed `entity_types`, we removed entity types from  the
        // `entity_children` map as we encountered a declaration for that type.
        // Any entity types left in the map are therefore undeclared. These are
        // any undeclared entity types which appeared in a `memberOf` list.
        let mut undeclared_e = undeclared_parent_entities
            .into_iter()
            .map(|n| n.to_string())
            .collect::<HashSet<_>>();
        // Looking at entity types, we need to check entity references in
        // attribute types. We already know that all elements of the
        // `descendants` list were declared because the list is a result of
        // inverting the `memberOf` relationship which mapped declared entity
        // types to their parent entity types.
        for entity_type in entity_types.values() {
            for (_, attr_typ) in entity_type.attributes() {
                Self::check_undeclared_in_type(
                    &attr_typ.attr_type,
                    entity_types,
                    &mut undeclared_e,
                );
            }
        }

        // Undeclared actions in a `memberOf` list.
        let undeclared_a = undeclared_parent_actions
            .into_iter()
            .map(|n| n.to_string())
            .collect::<HashSet<_>>();
        // For actions, we check entity references in the context attribute
        // types and `appliesTo` lists. See the `entity_types` loop for why the
        // `descendants` list is not checked.
        for action in action_ids.values() {
            for (_, attr_typ) in action.context.iter() {
                Self::check_undeclared_in_type(
                    &attr_typ.attr_type,
                    entity_types,
                    &mut undeclared_e,
                );
            }

            for p_entity in action.applies_to.applicable_principal_types() {
                match p_entity {
                    EntityType::Concrete(p_entity) => {
                        if !entity_types.contains_key(p_entity) {
                            undeclared_e.insert(p_entity.to_string());
                        }
                    }
                    EntityType::Unspecified => (),
                }
            }

            for r_entity in action.applies_to.applicable_resource_types() {
                match r_entity {
                    EntityType::Concrete(r_entity) => {
                        if !entity_types.contains_key(r_entity) {
                            undeclared_e.insert(r_entity.to_string());
                        }
                    }
                    EntityType::Unspecified => (),
                }
            }
        }
        if !undeclared_e.is_empty() {
            return Err(SchemaError::UndeclaredEntityTypes(undeclared_e));
        }
        if !undeclared_a.is_empty() {
            return Err(SchemaError::UndeclaredActions(undeclared_a));
        }

        Ok(())
    }

    fn record_attributes_or_error(ty: Type) -> Result<Attributes> {
        match ty {
            Type::EntityOrRecord(EntityRecordKind::Record { attrs }) => Ok(attrs),
            _ => Err(SchemaError::ContextOrShapeNotRecord),
        }
    }

    // Check that all entity types appearing inside a type are in the set of
    // declared entity types, adding any undeclared entity types to the
    // `undeclared_types` set.
    fn check_undeclared_in_type(
        ty: &Type,
        entity_types: &HashMap<Name, ValidatorEntityType>,
        undeclared_types: &mut HashSet<String>,
    ) {
        match ty {
            Type::EntityOrRecord(EntityRecordKind::Entity(lub)) => {
                for name in lub.iter() {
                    if !entity_types.contains_key(name) {
                        undeclared_types.insert(name.to_string());
                    }
                }
            }

            Type::EntityOrRecord(EntityRecordKind::Record { attrs }) => {
                for (_, attr_ty) in attrs.iter() {
                    Self::check_undeclared_in_type(
                        &attr_ty.attr_type,
                        entity_types,
                        undeclared_types,
                    );
                }
            }

            Type::Set {
                element_type: Some(element_type),
            } => Self::check_undeclared_in_type(element_type, entity_types, undeclared_types),

            _ => (),
        }
    }

    /// Lookup the ValidatorActionId object in the schema with the given name.
    pub fn get_action_id(&self, action_id: &EntityUID) -> Option<&ValidatorActionId> {
        self.action_ids.get(action_id)
    }

    /// Lookup the ValidatorEntityType object in the schema with the given name.
    pub fn get_entity_type(&self, entity_type_id: &Name) -> Option<&ValidatorEntityType> {
        self.entity_types.get(entity_type_id)
    }

    /// Return true when the entity_type_id corresponds to a valid entity type.
    pub(crate) fn is_known_action_id(&self, action_id: &EntityUID) -> bool {
        self.action_ids.contains_key(action_id)
    }

    /// Return true when the entity_type_id corresponds to a valid entity type.
    pub(crate) fn is_known_entity_type(&self, entity_type: &Name) -> bool {
        self.entity_types.contains_key(entity_type)
    }

    /// An iterator over the action ids in the schema.
    pub(crate) fn known_action_ids(&self) -> impl Iterator<Item = &EntityUID> {
        self.action_ids.keys()
    }

    /// An iterator over the entity type names in the schema.
    pub(crate) fn known_entity_types(&self) -> impl Iterator<Item = &Name> {
        self.entity_types.keys()
    }

    /// An iterator matching the entity Types to their Validator Types
    pub fn entity_types(&self) -> impl Iterator<Item = (&Name, &ValidatorEntityType)> {
        self.entity_types.iter()
    }

    /// Get the validator entity equal to an EUID using the component for a head
    /// var kind.
    pub(crate) fn get_entity_eq<'a, H, K>(&self, var: H, euid: EntityUID) -> Option<K>
    where
        H: 'a + HeadVar<K>,
        K: 'a,
    {
        var.get_euid_component(euid)
    }

    /// Get the validator entities that are in the descendants of an EUID using
    /// the component for a head var kind.
    pub(crate) fn get_entities_in<'a, H, K>(
        &'a self,
        var: H,
        euid: EntityUID,
    ) -> impl Iterator<Item = K> + 'a
    where
        H: 'a + HeadVar<K>,
        K: 'a + Clone,
    {
        var.get_descendants_if_present(self, euid.clone())
            .into_iter()
            .flatten()
            .map(Clone::clone)
            .chain(var.get_euid_component_if_present(self, euid).into_iter())
    }

    /// Get the validator entities that are in the descendants of any of the
    /// entities in a set of EUID using the component for a head var kind.
    pub(crate) fn get_entities_in_set<'a, H, K>(
        &'a self,
        var: H,
        euids: impl IntoIterator<Item = EntityUID> + 'a,
    ) -> impl Iterator<Item = K> + 'a
    where
        H: 'a + HeadVar<K>,
        K: 'a + Clone,
    {
        euids
            .into_iter()
            .flat_map(move |e| self.get_entities_in(var, e))
    }

    /// Since different Actions have different schemas for `Context`, you must
    /// specify the `Action` in order to get a `ContextSchema`.
    ///
    /// Returns `None` if the action is not in the schema.
    pub fn get_context_schema(
        &self,
        action: &EntityUID,
    ) -> Option<impl cedar_policy_core::entities::ContextSchema> {
        self.get_action_id(action).map(|action_id| {
            crate::types::Type::record_with_attributes(
                action_id
                    .context
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone())),
            )
        })
    }
}

impl cedar_policy_core::entities::Schema for ValidatorSchema {
    fn attr_type(
        &self,
        entity_type: &cedar_policy_core::ast::EntityType,
        attr: &str,
    ) -> Option<cedar_policy_core::entities::SchemaType> {
        match entity_type {
            cedar_policy_core::ast::EntityType::Unspecified => None, // Unspecified entity does not have attributes
            cedar_policy_core::ast::EntityType::Concrete(name) => {
                let entity_type: &ValidatorEntityType = self.get_entity_type(name)?;
                let validator_type: &crate::types::Type = &entity_type.attr(attr)?.attr_type;
                let core_schema_type: cedar_policy_core::entities::SchemaType = validator_type
                    .clone()
                    .try_into()
                    .expect("failed to convert validator type into Core SchemaType");
                debug_assert!(validator_type.is_consistent_with(&core_schema_type));
                Some(core_schema_type)
            }
        }
    }

    fn required_attrs<'s>(
        &'s self,
        entity_type: &cedar_policy_core::ast::EntityType,
    ) -> Box<dyn Iterator<Item = SmolStr> + 's> {
        match entity_type {
            cedar_policy_core::ast::EntityType::Unspecified => Box::new(std::iter::empty()), // Unspecified entity does not have attributes
            cedar_policy_core::ast::EntityType::Concrete(name) => {
                match self.get_entity_type(name) {
                    None => Box::new(std::iter::empty()),
                    Some(entity_type) => Box::new(
                        entity_type
                            .attributes
                            .iter()
                            .filter(|(_, ty)| ty.is_required)
                            .map(|(attr, _)| attr.clone()),
                    ),
                }
            }
        }
    }
}

/// A `Type` contains all the information we need for a Core `ContextSchema`.
impl cedar_policy_core::entities::ContextSchema for crate::types::Type {
    fn context_type(&self) -> cedar_policy_core::entities::SchemaType {
        self.clone()
            .try_into()
            .expect("failed to convert validator type into Core SchemaType")
    }
}

/// Contains entity type information for use by the validator. The contents of
/// the struct are the same as the schema entity type structure, but the
/// `member_of` relation is reversed to instead be `descendants`.
#[derive(Clone, Debug, Serialize)]
pub struct ValidatorEntityType {
    /// The name of the entity type.
    pub(crate) name: Name,

    /// The set of entity types that can be members of this entity type. When
    /// this structure is initially constructed, the field will contain direct
    /// children, but it will be updated to contain the closure of all
    /// descendants before it is used in any validation.
    pub descendants: HashSet<Name>,

    /// The attributes associated with this entity. Keys are the attribute
    /// identifiers while the values are the type of the attribute.
    pub(crate) attributes: Attributes,
}

impl ValidatorEntityType {
    /// Get the type of the attribute with the given name, if it exists
    pub fn attr(&self, attr: &str) -> Option<&AttributeType> {
        self.attributes.get_attr(attr)
    }

    /// An iterator over the attributes of this entity
    pub fn attributes(&self) -> impl Iterator<Item = (&SmolStr, &AttributeType)> {
        self.attributes.iter()
    }
}

impl TCNode<Name> for ValidatorEntityType {
    fn get_key(&self) -> Name {
        self.name.clone()
    }

    fn add_edge_to(&mut self, k: Name) {
        self.descendants.insert(k);
    }

    fn out_edges(&self) -> Box<dyn Iterator<Item = &Name> + '_> {
        Box::new(self.descendants.iter())
    }

    fn has_edge_to(&self, e: &Name) -> bool {
        self.descendants.contains(e)
    }
}

/// Contains information about actions used by the validator.  The contents of
/// the struct are the same as the schema entity type structure, but the
/// `member_of` relation is reversed to instead be `descendants`.
#[derive(Clone, Debug, Serialize)]
pub struct ValidatorActionId {
    /// The name of the action.
    pub(crate) name: EntityUID,

    /// The principals and resources that the action can be applied to.
    #[serde(rename = "appliesTo")]
    pub(crate) applies_to: ValidatorApplySpec,

    /// The set of actions that can be members of this action. When this
    /// structure is initially constructed, the field will contain direct
    /// children, but it will be updated to contain the closure of all
    /// descendants before it is used in any validation.
    pub(crate) descendants: HashSet<EntityUID>,

    /// The context attributes associated with this action. Keys are the context
    /// attribute identifiers while the values are the type of the attribute.
    pub(crate) context: Attributes,

    /// The action attributes
    pub(crate) attributes: Attributes,
}

impl ValidatorActionId {
    /// An iterator over the attributes of this action's required context
    pub fn context(&self) -> impl Iterator<Item = (&SmolStr, &AttributeType)> {
        self.context.iter()
    }
}

impl TCNode<EntityUID> for ValidatorActionId {
    fn get_key(&self) -> EntityUID {
        self.name.clone()
    }

    fn add_edge_to(&mut self, k: EntityUID) {
        self.descendants.insert(k);
    }

    fn out_edges(&self) -> Box<dyn Iterator<Item = &EntityUID> + '_> {
        Box::new(self.descendants.iter())
    }

    fn has_edge_to(&self, e: &EntityUID) -> bool {
        self.descendants.contains(e)
    }
}

/// The principals and resources that an action can be applied to.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ValidatorApplySpec {
    /// The principal entity types the action can be applied to. This set may
    /// be a singleton set containing the unspecified entity type when the
    /// `principalTypes` list is omitted in the schema. A non-singleton set
    /// shouldn't contain the unspecified entity type, but validation will give
    /// the same success/failure result as when it is the only element of the
    /// set, perhaps with extra type errors.
    #[serde(rename = "principalApplySpec")]
    principal_apply_spec: HashSet<EntityType>,

    /// The resource entity types the action can be applied to. See comments on
    /// `principal_apply_spec` about the unspecified entity type.
    #[serde(rename = "resourceApplySpec")]
    resource_apply_spec: HashSet<EntityType>,
}

impl ValidatorApplySpec {
    /// Create an apply spec for an action that can only be applied to some
    /// specific entities.
    pub(crate) fn new(
        principal_apply_spec: HashSet<EntityType>,
        resource_apply_spec: HashSet<EntityType>,
    ) -> Self {
        Self {
            principal_apply_spec,
            resource_apply_spec,
        }
    }

    /// Get the applicable principal types for this spec.
    pub(crate) fn applicable_principal_types(&self) -> impl Iterator<Item = &EntityType> {
        self.principal_apply_spec.iter()
    }

    /// Get the applicable resource types for this spec.
    pub(crate) fn applicable_resource_types(&self) -> impl Iterator<Item = &EntityType> {
        self.resource_apply_spec.iter()
    }
}

/// This trait configures what sort of entity (principals, actions, or resources)
/// are returned by the function `get_entities_satisfying_constraint`.
pub(crate) trait HeadVar<K>: Copy {
    /// For a validator, get the known entities for this sort of head variable.
    /// This is all entity types (for principals and resources), or actions ids
    /// (for actions) that appear in the service description.
    fn get_known_vars<'a>(
        &self,
        schema: &'a ValidatorSchema,
    ) -> Box<dyn Iterator<Item = &'a K> + 'a>;

    /// Extract the relevant component of an entity uid. This is the entity type
    /// for principals and resources, and the entity id for actions.
    fn get_euid_component(&self, euid: EntityUID) -> Option<K>;

    /// Extract the relevant component of an entity uid if the entity uid is in
    /// the schema. Otherwise return None.
    fn get_euid_component_if_present(&self, schema: &ValidatorSchema, euid: EntityUID)
        -> Option<K>;

    /// Get and iterator containing the valid descendants of an entity, if that
    /// entity exists in the schema. Otherwise None.
    fn get_descendants_if_present<'a>(
        &self,
        schema: &'a ValidatorSchema,
        euid: EntityUID,
    ) -> Option<Box<dyn Iterator<Item = &'a K> + 'a>>;
}

/// Used to have `get_entities_satisfying_constraint` return the
/// `EntityTypeNames` for either principals or resources satisfying the head
/// constraints.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PrincipalOrResourceHeadVar {
    PrincipalOrResource,
}

impl HeadVar<Name> for PrincipalOrResourceHeadVar {
    fn get_known_vars<'a>(
        &self,
        schema: &'a ValidatorSchema,
    ) -> Box<dyn Iterator<Item = &'a Name> + 'a> {
        Box::new(schema.known_entity_types())
    }

    fn get_euid_component(&self, euid: EntityUID) -> Option<Name> {
        let (ty, _) = euid.components();
        match ty {
            EntityType::Unspecified => None,
            EntityType::Concrete(name) => Some(name),
        }
    }

    fn get_euid_component_if_present(
        &self,
        schema: &ValidatorSchema,
        euid: EntityUID,
    ) -> Option<Name> {
        let euid_component = self.get_euid_component(euid)?;
        if schema.is_known_entity_type(&euid_component) {
            Some(euid_component)
        } else {
            None
        }
    }

    fn get_descendants_if_present<'a>(
        &self,
        schema: &'a ValidatorSchema,
        euid: EntityUID,
    ) -> Option<Box<dyn Iterator<Item = &'a Name> + 'a>> {
        let euid_component = self.get_euid_component(euid)?;
        match schema.get_entity_type(&euid_component) {
            Some(entity_type) => Some(Box::new(entity_type.descendants.iter())),
            None => None,
        }
    }
}

/// Used to have `get_entities_satisfying_constraint` return the
/// `ActionIdNames` for actions satisfying the head constraints
#[derive(Debug, Clone, Copy)]
pub(crate) enum ActionHeadVar {
    Action,
}

impl HeadVar<EntityUID> for ActionHeadVar {
    fn get_known_vars<'a>(
        &self,
        schema: &'a ValidatorSchema,
    ) -> Box<dyn Iterator<Item = &'a EntityUID> + 'a> {
        Box::new(schema.known_action_ids())
    }

    fn get_euid_component(&self, euid: EntityUID) -> Option<EntityUID> {
        Some(euid)
    }

    fn get_euid_component_if_present(
        &self,
        schema: &ValidatorSchema,
        euid: EntityUID,
    ) -> Option<EntityUID> {
        let euid_component = self.get_euid_component(euid)?;
        if schema.is_known_action_id(&euid_component) {
            Some(euid_component)
        } else {
            None
        }
    }

    fn get_descendants_if_present<'a>(
        &self,
        schema: &'a ValidatorSchema,
        euid: EntityUID,
    ) -> Option<Box<dyn Iterator<Item = &'a EntityUID> + 'a>> {
        let euid_component = self.get_euid_component(euid)?;
        match schema.get_action_id(&euid_component) {
            Some(action_id) => Some(Box::new(action_id.descendants.iter())),
            None => None,
        }
    }
}

/// Used to write a schema implicitly overriding the default handling of action
/// groups.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub(crate) struct NamespaceDefinitionWithActionAttributes(pub(crate) NamespaceDefinition);

impl TryInto<ValidatorSchema> for NamespaceDefinitionWithActionAttributes {
    type Error = SchemaError;

    fn try_into(self) -> Result<ValidatorSchema> {
        ValidatorSchema::from_schema_fragments([ValidatorSchemaFragment::from_namespaces([
            ValidatorNamespaceDef::from_namespace_definition(
                None,
                self.0,
                crate::ActionBehavior::PermitAttributes,
            )?,
        ])])
    }
}

#[cfg(test)]
mod test {
    use std::{collections::BTreeMap, str::FromStr};

    use crate::types::Type;

    use serde_json::json;

    use super::*;

    // Well-formed schema
    #[test]
    fn test_from_schema_file() {
        let src = json!(
        {
            "entityTypes": {
                "User": {
                    "memberOfTypes": [ "Group" ]
                },
                "Group": {
                    "memberOfTypes": []
                },
                "Photo": {
                    "memberOfTypes": [ "Album" ]
                },
                "Album": {
                    "memberOfTypes": []
                }
            },
            "actions": {
                "view_photo": {
                    "appliesTo": {
                        "principalTypes": ["User", "Group"],
                        "resourceTypes": ["Photo"]
                    }
                }
            }
        });
        let schema_file: NamespaceDefinition = serde_json::from_value(src).expect("Parse Error");
        let schema: Result<ValidatorSchema> = schema_file.try_into();
        assert!(schema.is_ok());
    }

    // Duplicate entity "Photo"
    #[test]
    fn test_from_schema_file_duplicate_entity() {
        // Test written using `from_str` instead of `from_value` because the
        // `json!` macro silently ignores duplicate map keys.
        let src = r#"
        {"": {
            "entityTypes": {
                "User": {
                    "memberOfTypes": [ "Group" ]
                },
                "Group": {
                    "memberOfTypes": []
                },
                "Photo": {
                    "memberOfTypes": [ "Album" ]
                },
                "Photo": {
                    "memberOfTypes": []
                }
            },
            "actions": {
                "view_photo": {
                    "memberOf": [],
                    "appliesTo": {
                        "principalTypes": ["User", "Group"],
                        "resourceTypes": ["Photo"]
                    }
                }
            }
        }}"#;

        match ValidatorSchema::from_str(src) {
            Err(SchemaError::ParseFileFormat(_)) => (),
            _ => panic!("Expected serde error due to duplicate entity type."),
        }
    }

    // Duplicate action "view_photo"
    #[test]
    fn test_from_schema_file_duplicate_action() {
        // Test written using `from_str` instead of `from_value` because the
        // `json!` macro silently ignores duplicate map keys.
        let src = r#"
        {"": {
            "entityTypes": {
                "User": {
                    "memberOfTypes": [ "Group" ]
                },
                "Group": {
                    "memberOfTypes": []
                },
                "Photo": {
                    "memberOfTypes": []
                }
            },
            "actions": {
                "view_photo": {
                    "memberOf": [],
                    "appliesTo": {
                        "principalTypes": ["User", "Group"],
                        "resourceTypes": ["Photo"]
                    }
                },
                "view_photo": { }
            }
        }"#;
        match ValidatorSchema::from_str(src) {
            Err(SchemaError::ParseFileFormat(_)) => (),
            _ => panic!("Expected serde error due to duplicate action type."),
        }
    }

    // Undefined entity types "Grop", "Usr", "Phoot"
    #[test]
    fn test_from_schema_file_undefined_entities() {
        let src = json!(
        {
            "entityTypes": {
                "User": {
                    "memberOfTypes": [ "Grop" ]
                },
                "Group": {
                    "memberOfTypes": []
                },
                "Photo": {
                    "memberOfTypes": []
                }
            },
            "actions": {
                "view_photo": {
                    "appliesTo": {
                        "principalTypes": ["Usr", "Group"],
                        "resourceTypes": ["Phoot"]
                    }
                }
            }
        });
        let schema_file: NamespaceDefinition = serde_json::from_value(src).expect("Parse Error");
        let schema: Result<ValidatorSchema> = schema_file.try_into();
        match schema {
            Ok(_) => panic!("from_schema_file should have failed"),
            Err(SchemaError::UndeclaredEntityTypes(v)) => {
                assert_eq!(v.len(), 3)
            }
            _ => panic!("Unexpected error from from_schema_file"),
        }
    }

    #[test]
    fn undefined_entity_namespace_member_of() {
        let src = json!(
        {"Foo": {
            "entityTypes": {
                "User": {
                    "memberOfTypes": [ "Foo::Group", "Bar::Group" ]
                },
                "Group": { }
            },
            "actions": {}
        }});
        let schema_file: SchemaFragment = serde_json::from_value(src).expect("Parse Error");
        let schema: Result<ValidatorSchema> = schema_file.try_into();
        match schema {
            Ok(_) => panic!("try_into should have failed"),
            Err(SchemaError::UndeclaredEntityTypes(v)) => {
                assert_eq!(v, HashSet::from(["Bar::Group".to_string()]))
            }
            _ => panic!("Unexpected error from try_into"),
        }
    }

    #[test]
    fn undefined_entity_namespace_applies_to() {
        let src = json!(
        {"Foo": {
            "entityTypes": { "User": { }, "Photo": { } },
            "actions": {
                "view_photo": {
                    "appliesTo": {
                        "principalTypes": ["Foo::User", "Bar::User"],
                        "resourceTypes": ["Photo", "Bar::Photo"],
                    }
                }
            }
        }});
        let schema_file: SchemaFragment = serde_json::from_value(src).expect("Parse Error");
        let schema: Result<ValidatorSchema> = schema_file.try_into();
        match schema {
            Ok(_) => panic!("try_into should have failed"),
            Err(SchemaError::UndeclaredEntityTypes(v)) => {
                assert_eq!(
                    v,
                    HashSet::from(["Bar::Photo".to_string(), "Bar::User".to_string()])
                )
            }
            _ => panic!("Unexpected error from try_into"),
        }
    }

    // Undefined action "photo_actions"
    #[test]
    fn test_from_schema_file_undefined_action() {
        let src = json!(
        {
            "entityTypes": {
                "User": {
                    "memberOfTypes": [ "Group" ]
                },
                "Group": {
                    "memberOfTypes": []
                },
                "Photo": {
                    "memberOfTypes": []
                }
            },
            "actions": {
                "view_photo": {
                    "memberOf": [ {"id": "photo_action"} ],
                    "appliesTo": {
                        "principalTypes": ["User", "Group"],
                        "resourceTypes": ["Photo"]
                    }
                }
            }
        });
        let schema_file: NamespaceDefinition = serde_json::from_value(src).expect("Parse Error");
        let schema: Result<ValidatorSchema> = schema_file.try_into();
        match schema {
            Ok(_) => panic!("from_schema_file should have failed"),
            Err(SchemaError::UndeclaredActions(v)) => assert_eq!(v.len(), 1),
            _ => panic!("Unexpected error from from_schema_file"),
        }
    }

    // Trivial cycle in action hierarchy
    // view_photo -> view_photo
    #[test]
    fn test_from_schema_file_action_cycle1() {
        let src = json!(
        {
            "entityTypes": {},
            "actions": {
                "view_photo": {
                    "memberOf": [ {"id": "view_photo"} ]
                }
            }
        });
        let schema_file: NamespaceDefinition = serde_json::from_value(src).expect("Parse Error");
        let schema: Result<ValidatorSchema> = schema_file.try_into();
        match schema {
            Ok(_) => panic!("from_schema_file should have failed"),
            Err(SchemaError::CycleInActionHierarchy) => (), // expected result
            e => panic!("Unexpected error from from_schema_file: {:?}", e),
        }
    }

    // Slightly more complex cycle in action hierarchy
    // view_photo -> edit_photo -> delete_photo -> view_photo
    #[test]
    fn test_from_schema_file_action_cycle2() {
        let src = json!(
        {
            "entityTypes": {},
            "actions": {
                "view_photo": {
                    "memberOf": [ {"id": "edit_photo"} ]
                },
                "edit_photo": {
                    "memberOf": [ {"id": "delete_photo"} ]
                },
                "delete_photo": {
                    "memberOf": [ {"id": "view_photo"} ]
                },
                "other_action": {
                    "memberOf": [ {"id": "edit_photo"} ]
                }
            }
        });
        let schema_file: NamespaceDefinition = serde_json::from_value(src).expect("Parse Error");
        let schema: Result<ValidatorSchema> = schema_file.try_into();
        match schema {
            Ok(x) => {
                println!("{:?}", x);
                panic!("from_schema_file should have failed");
            }
            Err(SchemaError::CycleInActionHierarchy) => (), // expected result
            e => panic!("Unexpected error from from_schema_file: {:?}", e),
        }
    }

    #[test]
    fn namespaced_schema() {
        let src = r#"
        { "N::S": {
            "entityTypes": {
                "User": {},
                "Photo": {}
            },
            "actions": {
                "view_photo": {
                    "appliesTo": {
                        "principalTypes": ["User"],
                        "resourceTypes": ["Photo"]
                    }
                }
            }
        } }
        "#;
        let schema_file: SchemaFragment = serde_json::from_str(src).expect("Parse Error");
        let schema: ValidatorSchema = schema_file
            .try_into()
            .expect("Namespaced schema failed to convert.");
        dbg!(&schema);
        let user_entity_type = &"N::S::User"
            .parse()
            .expect("Namespaced entity type should have parsed");
        let photo_entity_type = &"N::S::Photo"
            .parse()
            .expect("Namespaced entity type should have parsed");
        assert!(
            schema.entity_types.contains_key(user_entity_type),
            "Expected and entity type User."
        );
        assert!(
            schema.entity_types.contains_key(photo_entity_type),
            "Expected an entity type Photo."
        );
        assert_eq!(
            schema.entity_types.len(),
            2,
            "Expected exactly 2 entity types."
        );
        assert!(
            schema.action_ids.contains_key(
                &"N::S::Action::\"view_photo\""
                    .parse()
                    .expect("Namespaced action should have parsed")
            ),
            "Expected an action \"view_photo\"."
        );
        assert_eq!(schema.action_ids.len(), 1, "Expected exactly 1 action.");

        let apply_spec = &schema
            .action_ids
            .values()
            .next()
            .expect("Expected Action")
            .applies_to;
        assert_eq!(
            apply_spec.applicable_principal_types().collect::<Vec<_>>(),
            vec![&EntityType::Concrete(user_entity_type.clone())]
        );
        assert_eq!(
            apply_spec.applicable_resource_types().collect::<Vec<_>>(),
            vec![&EntityType::Concrete(photo_entity_type.clone())]
        );
    }

    #[test]
    fn cant_use_namespace_in_entity_type() {
        let src = r#"
        {
            "entityTypes": { "NS::User": {} },
            "actions": {}
        }
        "#;
        let schema_file: NamespaceDefinition = serde_json::from_str(src).expect("Parse Error");
        assert!(
            matches!(TryInto::<ValidatorSchema>::try_into(schema_file), Err(SchemaError::EntityTypeParseError(_))),
            "Expected that namespace in the entity type NS::User would cause a EntityType parse error.");
    }

    #[test]
    fn entity_attribute_entity_type_with_namespace() {
        let schema_json: SchemaFragment = serde_json::from_str(
            r#"
            {"A::B": {
                "entityTypes": {
                    "Foo": {
                        "shape": {
                            "type": "Record",
                            "attributes": {
                                "name": { "type": "Entity", "name": "C::D::Foo" }
                            }
                        }
                    }
                },
                "actions": {}
              }}
            "#,
        )
        .expect("Expected valid schema");

        let schema: Result<ValidatorSchema> = schema_json.try_into();
        match schema {
            Err(SchemaError::UndeclaredEntityTypes(tys)) => {
                assert_eq!(tys, HashSet::from(["C::D::Foo".to_string()]))
            }
            _ => panic!("Schema construction should have failed due to undeclared entity type."),
        }
    }

    #[test]
    fn entity_attribute_entity_type_with_declared_namespace() {
        let schema_json: SchemaFragment = serde_json::from_str(
            r#"
            {"A::B": {
                "entityTypes": {
                    "Foo": {
                        "shape": {
                            "type": "Record",
                            "attributes": {
                                "name": { "type": "Entity", "name": "A::B::Foo" }
                            }
                        }
                    }
                },
                "actions": {}
              }}
            "#,
        )
        .expect("Expected valid schema");

        let schema: ValidatorSchema = schema_json
            .try_into()
            .expect("Expected schema to construct without error.");

        let foo_name: Name = "A::B::Foo".parse().expect("Expected entity type name");
        let foo_type = schema
            .entity_types
            .get(&foo_name)
            .expect("Expected to find entity");
        let name_type = foo_type
            .attr("name")
            .expect("Expected attribute name")
            .attr_type
            .clone();
        let expected_name_type = Type::named_entity_reference(foo_name);
        assert_eq!(name_type, expected_name_type);
    }

    #[test]
    fn cannot_declare_action_type_when_prohibited() {
        let schema_json: NamespaceDefinition = serde_json::from_str(
            r#"
            {
                "entityTypes": { "Action": {} },
                "actions": {}
              }
            "#,
        )
        .expect("Expected valid schema");

        let schema: Result<ValidatorSchema> = schema_json.try_into();
        assert!(matches!(schema, Err(SchemaError::ActionEntityTypeDeclared)));
    }

    #[test]
    fn can_declare_other_type_when_action_type_prohibited() {
        let schema_json: NamespaceDefinition = serde_json::from_str(
            r#"
            {
                "entityTypes": { "Foo": { } },
                "actions": {}
              }
            "#,
        )
        .expect("Expected valid schema");

        TryInto::<ValidatorSchema>::try_into(schema_json).expect("Did not expect any errors.");
    }

    #[test]
    fn cannot_declare_action_in_group_when_prohibited() {
        let schema_json: SchemaFragment = serde_json::from_str(
            r#"
            {"": {
                "entityTypes": {},
                "actions": {
                    "universe": { },
                    "view_photo": {
                        "attributes": {"id": "universe"}
                    },
                    "edit_photo": {
                        "attributes": {"id": "universe"}
                    },
                    "delete_photo": {
                        "attributes": {"id": "universe"}
                    }
                }
              }}
            "#,
        )
        .expect("Expected valid schema");

        let schema = ValidatorSchemaFragment::from_schema_fragment(
            schema_json,
            ActionBehavior::ProhibitAttributes,
        );
        match schema {
            Err(SchemaError::ActionEntityAttributes(actions)) => {
                assert_eq!(
                    actions.into_iter().collect::<HashSet<_>>(),
                    HashSet::from([
                        "view_photo".to_string(),
                        "edit_photo".to_string(),
                        "delete_photo".to_string(),
                    ])
                )
            }
            _ => panic!("Did not see expected error."),
        }
    }

    #[test]
    fn test_entity_type_no_namespace() {
        let src = json!({"type": "Entity", "name": "Foo"});
        let schema_ty: SchemaType = serde_json::from_value(src).expect("Parse Error");
        assert_eq!(
            schema_ty,
            SchemaType::Type(SchemaTypeVariant::Entity { name: "Foo".into() })
        );
        let ty: Type = ValidatorNamespaceDef::try_schema_type_into_validator_type(
            &parse_namespace("NS").expect("Expected namespace."),
            schema_ty,
        )
        .expect("Error converting schema type to type.")
        .resolve_type_defs(&HashMap::new())
        .unwrap();
        assert_eq!(ty, Type::named_entity_reference_from_str("NS::Foo"));
    }

    #[test]
    fn test_entity_type_namespace() {
        let src = json!({"type": "Entity", "name": "NS::Foo"});
        let schema_ty: SchemaType = serde_json::from_value(src).expect("Parse Error");
        assert_eq!(
            schema_ty,
            SchemaType::Type(SchemaTypeVariant::Entity {
                name: "NS::Foo".into()
            })
        );
        let ty: Type = ValidatorNamespaceDef::try_schema_type_into_validator_type(
            &parse_namespace("NS").expect("Expected namespace."),
            schema_ty,
        )
        .expect("Error converting schema type to type.")
        .resolve_type_defs(&HashMap::new())
        .unwrap();
        assert_eq!(ty, Type::named_entity_reference_from_str("NS::Foo"));
    }

    #[test]
    fn test_entity_type_namespace_parse_error() {
        let src = json!({"type": "Entity", "name": "::Foo"});
        let schema_ty: SchemaType = serde_json::from_value(src).expect("Parse Error");
        assert_eq!(
            schema_ty,
            SchemaType::Type(SchemaTypeVariant::Entity {
                name: "::Foo".into()
            })
        );
        match ValidatorNamespaceDef::try_schema_type_into_validator_type(
            &parse_namespace("NS").expect("Expected namespace."),
            schema_ty,
        ) {
            Err(SchemaError::EntityTypeParseError(_)) => (),
            _ => panic!("Did not see expected EntityTypeParseError."),
        }
    }

    #[test]
    fn schema_type_record_is_validator_type_record() {
        let src = json!({"type": "Record", "attributes": {}});
        let schema_ty: SchemaType = serde_json::from_value(src).expect("Parse Error");
        assert_eq!(
            schema_ty,
            SchemaType::Type(SchemaTypeVariant::Record {
                attributes: BTreeMap::new(),
                additional_attributes: false,
            }),
        );
        let ty: Type =
            ValidatorNamespaceDef::try_schema_type_into_validator_type(&Vec::new(), schema_ty)
                .expect("Error converting schema type to type.")
                .resolve_type_defs(&HashMap::new())
                .unwrap();
        assert_eq!(ty, Type::record_with_attributes(None));
    }

    #[test]
    fn get_namespaces() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "Foo::Bar::Baz": {
                "entityTypes": {},
                "actions": {}
            },
            "Foo": {
                "entityTypes": {},
                "actions": {}
            },
            "Bar": {
                "entityTypes": {},
                "actions": {}
            },
        }))
        .unwrap();

        let schema_fragment: ValidatorSchemaFragment = fragment.try_into().unwrap();
        assert_eq!(
            schema_fragment
                .0
                .iter()
                .map(|f| f.namespace())
                .collect::<HashSet<_>>(),
            HashSet::from([
                &Some("Foo::Bar::Baz".parse().unwrap()),
                &Some("Foo".parse().unwrap()),
                &Some("Bar".parse().unwrap())
            ])
        );
    }

    #[test]
    fn schema_no_fragments() {
        let schema = ValidatorSchema::from_schema_fragments([]).unwrap();
        assert!(schema.entity_types.is_empty());
        assert!(schema.action_ids.is_empty());
    }

    #[test]
    fn same_action_different_namespace() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "Foo::Bar": {
                "entityTypes": {},
                "actions": {
                    "Baz": {}
                }
            },
            "Bar::Foo": {
                "entityTypes": {},
                "actions": {
                    "Baz": { }
                }
            },
            "Biz": {
                "entityTypes": {},
                "actions": {
                    "Baz": { }
                }
            }
        }))
        .unwrap();

        let schema: ValidatorSchema = fragment.try_into().unwrap();
        assert!(schema
            .get_action_id(&"Foo::Bar::Action::\"Baz\"".parse().unwrap())
            .is_some());
        assert!(schema
            .get_action_id(&"Bar::Foo::Action::\"Baz\"".parse().unwrap())
            .is_some());
        assert!(schema
            .get_action_id(&"Biz::Action::\"Baz\"".parse().unwrap())
            .is_some());
    }

    #[test]
    fn same_type_different_namespace() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "Foo::Bar": {
                "entityTypes": {"Baz" : {}},
                "actions": { }
            },
            "Bar::Foo": {
                "entityTypes": {"Baz" : {}},
                "actions": { }
            },
            "Biz": {
                "entityTypes": {"Baz" : {}},
                "actions": { }
            }
        }))
        .unwrap();
        let schema: ValidatorSchema = fragment.try_into().unwrap();

        assert!(schema
            .get_entity_type(&"Foo::Bar::Baz".parse().unwrap())
            .is_some());
        assert!(schema
            .get_entity_type(&"Bar::Foo::Baz".parse().unwrap())
            .is_some());
        assert!(schema
            .get_entity_type(&"Biz::Baz".parse().unwrap())
            .is_some());
    }

    #[test]
    fn member_of_different_namespace() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "Bar": {
                "entityTypes": {
                    "Baz": {
                        "memberOfTypes": ["Foo::Buz"]
                    }
                },
                "actions": {}
            },
            "Foo": {
                "entityTypes": { "Buz": {} },
                "actions": { }
            }
        }))
        .unwrap();
        let schema: ValidatorSchema = fragment.try_into().unwrap();

        let buz = schema
            .get_entity_type(&"Foo::Buz".parse().unwrap())
            .unwrap();
        assert_eq!(
            buz.descendants,
            HashSet::from(["Bar::Baz".parse().unwrap()])
        );
    }

    #[test]
    fn attribute_different_namespace() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "Bar": {
                "entityTypes": {
                    "Baz": {
                        "shape": {
                            "type": "Record",
                            "attributes": {
                                "fiz": {
                                    "type": "Entity",
                                    "name": "Foo::Buz"
                                }
                            }
                        }
                    }
                },
                "actions": {}
            },
            "Foo": {
                "entityTypes": { "Buz": {} },
                "actions": { }
            }
        }))
        .unwrap();

        let schema: ValidatorSchema = fragment.try_into().unwrap();
        let baz = schema
            .get_entity_type(&"Bar::Baz".parse().unwrap())
            .unwrap();
        assert_eq!(
            baz.attr("fiz").unwrap().attr_type,
            Type::named_entity_reference_from_str("Foo::Buz"),
        );
    }

    #[test]
    fn applies_to_different_namespace() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "Foo::Bar": {
                "entityTypes": { },
                "actions": {
                    "Baz": {
                        "appliesTo": {
                            "principalTypes": [ "Fiz::Buz" ],
                            "resourceTypes": [ "Fiz::Baz" ],
                        }
                    }
                }
            },
            "Fiz": {
                "entityTypes": {
                    "Buz": {},
                    "Baz": {}
                },
                "actions": { }
            }
        }))
        .unwrap();
        let schema: ValidatorSchema = fragment.try_into().unwrap();

        let baz = schema
            .get_action_id(&"Foo::Bar::Action::\"Baz\"".parse().unwrap())
            .unwrap();
        assert_eq!(
            baz.applies_to
                .applicable_principal_types()
                .collect::<HashSet<_>>(),
            HashSet::from([&EntityType::Concrete("Fiz::Buz".parse().unwrap())])
        );
        assert_eq!(
            baz.applies_to
                .applicable_resource_types()
                .collect::<HashSet<_>>(),
            HashSet::from([&EntityType::Concrete("Fiz::Baz".parse().unwrap())])
        );
    }

    #[test]
    fn simple_defined_type() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "": {
                "commonTypes": {
                    "MyLong": {"type": "Long"}
                },
                "entityTypes": {
                    "User": {
                        "shape": {
                            "type": "Record",
                            "attributes": {
                                "a": {"type": "MyLong"}
                            }
                        }
                    }
                },
                "actions": {}
            }
        }))
        .unwrap();
        let schema: ValidatorSchema = fragment.try_into().unwrap();
        assert_eq!(
            schema.entity_types.iter().next().unwrap().1.attributes,
            Attributes::with_required_attributes([("a".into(), Type::primitive_long())])
        );
    }

    #[test]
    fn defined_record_as_attrs() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "": {
                "commonTypes": {
                    "MyRecord": {
                        "type": "Record",
                        "attributes":  {
                            "a": {"type": "Long"}
                        }
                    }
                },
                "entityTypes": {
                    "User": { "shape": { "type": "MyRecord", } }
                },
                "actions": {}
            }
        }))
        .unwrap();
        let schema: ValidatorSchema = fragment.try_into().unwrap();
        assert_eq!(
            schema.entity_types.iter().next().unwrap().1.attributes,
            Attributes::with_required_attributes([("a".into(), Type::primitive_long())])
        );
    }

    #[test]
    fn cross_namespace_type() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "A": {
                "commonTypes": {
                    "MyLong": {"type": "Long"}
                },
                "entityTypes": { },
                "actions": {}
            },
            "B": {
                "entityTypes": {
                    "User": {
                        "shape": {
                            "type": "Record",
                            "attributes": {
                                "a": {"type": "A::MyLong"}
                            }
                        }
                    }
                },
                "actions": {}
            }
        }))
        .unwrap();
        let schema: ValidatorSchema = fragment.try_into().unwrap();
        assert_eq!(
            schema.entity_types.iter().next().unwrap().1.attributes,
            Attributes::with_required_attributes([("a".into(), Type::primitive_long())])
        );
    }

    #[test]
    fn cross_fragment_type() {
        let fragment1: ValidatorSchemaFragment = serde_json::from_value::<SchemaFragment>(json!({
            "A": {
                "commonTypes": {
                    "MyLong": {"type": "Long"}
                },
                "entityTypes": { },
                "actions": {}
            }
        }))
        .unwrap()
        .try_into()
        .unwrap();
        let fragment2: ValidatorSchemaFragment = serde_json::from_value::<SchemaFragment>(json!({
            "A": {
                "entityTypes": {
                    "User": {
                        "shape": {
                            "type": "Record",
                            "attributes": {
                                "a": {"type": "MyLong"}
                            }
                        }
                    }
                },
                "actions": {}
            }
        }))
        .unwrap()
        .try_into()
        .unwrap();
        let schema = ValidatorSchema::from_schema_fragments([fragment1, fragment2]).unwrap();

        assert_eq!(
            schema.entity_types.iter().next().unwrap().1.attributes,
            Attributes::with_required_attributes([("a".into(), Type::primitive_long())])
        );
    }

    #[test]
    #[should_panic]
    fn cross_fragment_duplicate_type() {
        let fragment1: ValidatorSchemaFragment = serde_json::from_value::<SchemaFragment>(json!({
            "A": {
                "commonTypes": {
                    "MyLong": {"type": "Long"}
                },
                "entityTypes": {},
                "actions": {}
            }
        }))
        .unwrap()
        .try_into()
        .unwrap();
        let fragment2: ValidatorSchemaFragment = serde_json::from_value::<SchemaFragment>(json!({
            "A": {
                "commonTypes": {
                    "MyLong": {"type": "Long"}
                },
                "entityTypes": {},
                "actions": {}
            }
        }))
        .unwrap()
        .try_into()
        .unwrap();
        let schema = ValidatorSchema::from_schema_fragments([fragment1, fragment2]).unwrap();

        assert_eq!(
            schema.entity_types.iter().next().unwrap().1.attributes,
            Attributes::with_required_attributes([("a".into(), Type::primitive_long())])
        );
    }

    #[test]
    fn undeclared_type_in_attr() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "": {
                "commonTypes": { },
                "entityTypes": {
                    "User": {
                        "shape": {
                            "type": "Record",
                            "attributes": {
                                "a": {"type": "MyLong"}
                            }
                        }
                    }
                },
                "actions": {}
            }
        }))
        .unwrap();
        match TryInto::<ValidatorSchema>::try_into(fragment) {
            Err(SchemaError::UndeclaredCommonType(_)) => (),
            s => panic!(
                "Expected Err(SchemaError::UndeclaredCommonType), got {:?}",
                s
            ),
        }
    }

    #[test]
    fn undeclared_type_in_type_def() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "": {
                "commonTypes": {
                    "a": { "type": "b" }
                },
                "entityTypes": { },
                "actions": {}
            }
        }))
        .unwrap();
        match TryInto::<ValidatorSchema>::try_into(fragment) {
            Err(SchemaError::UndeclaredCommonType(_)) => (),
            s => panic!(
                "Expected Err(SchemaError::UndeclaredCommonType), got {:?}",
                s
            ),
        }
    }

    #[test]
    fn shape_not_record() {
        let fragment: SchemaFragment = serde_json::from_value(json!({
            "": {
                "commonTypes": {
                    "MyLong": { "type": "Long" }
                },
                "entityTypes": {
                    "User": {
                        "shape": { "type": "MyLong" }
                    }
                },
                "actions": {}
            }
        }))
        .unwrap();
        match TryInto::<ValidatorSchema>::try_into(fragment) {
            Err(SchemaError::ContextOrShapeNotRecord) => (),
            s => panic!(
                "Expected Err(SchemaError::ContextOrShapeNotRecord), got {:?}",
                s
            ),
        }
    }
}
