use std::{collections::BTreeSet, mem};

use indexmap::IndexMap;
use schemars::{
    gen::{SchemaGenerator, SchemaSettings},
    schema::{
        ArrayValidation, InstanceType, NumberValidation, ObjectValidation, RootSchema, Schema,
        SchemaObject, SingleOrVec, SubschemaValidation,
    },
};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::{num::ConfigurableNumber, Configurable, CustomAttribute, Metadata};

/// Finalizes the schema by ensuring all metadata is applied and registering it in the generator.
///
/// As many configuration types are reused often, such as nearly all sinks allowing configuration of batching
/// behavior via `BatchConfig`, we utilize JSONSchema's ability to define a named schema and then
/// reference it via a short identifier whenever we want to apply that schema to a particular field.
/// This promotes a more concise schema and allows effectively exposing the discrete configuration
/// types such that they can be surfaced by tools using the schema.
///
/// Since we don't utilize the typical flow of generating schemas via `schemars`, we're forced to
/// manually determine when we should register a schema as a referencable schema within the schema
/// generator. As well, we need to handle applying metadata to these schemas such that we preserve
/// the intended behavior.
pub fn finalize_schema<T>(
    gen: &mut SchemaGenerator,
    schema: &mut SchemaObject,
    metadata: Metadata<T>,
) where
    T: Configurable + Serialize,
{
    // If the type that this schema represents is referencable, check to see if it's been defined
    // before, and if not, then go ahead and define it.
    if let Some(ref_name) = T::referencable_name() {
        if !gen.definitions().contains_key(ref_name) {
            // We specifically apply the metadata of `T` itself, and not the `metadata` we've been
            // given, as we do not want to apply field-level metadata e.g. field-specific default
            // values. We do, however, apply the given `metadata` to the schema reference itself.
            apply_metadata(schema, T::metadata());
            gen.definitions_mut()
                .insert(ref_name.to_string(), Schema::Object(schema.clone()));
        }

        // Replace the mutable reference to the original schema with an actual "reference" schema
        // that points the caller towards the stored definition for the given schema, which is
        // represented in the JSONSchema output by the usage of `"$ref": "<ref_name>"`.
        let ref_path = format!("{}{}", gen.settings().definitions_path, ref_name);
        *schema = SchemaObject::new_ref(ref_path);
    }

    apply_metadata(schema, metadata);
}

/// Applies metadata to the given schema.
///
/// Metadata can include semantic information (title, description, etc), validation (min/max, allowable
/// patterns, etc), as well as actual arbitrary key/value data.
pub fn apply_metadata<T>(schema: &mut SchemaObject, metadata: Metadata<T>)
where
    T: Serialize,
{
    // Set the title/description of this schema.
    //
    // By default, we want to populate `description` because most things don't need a title: their property name or type
    // name is the title... which is why we enforce description being present at the very least.
    let schema_title = metadata.title().map(|s| s.to_string());
    let schema_description = metadata.description().map(|s| s.to_string());
    if schema_description.is_none() && !metadata.transparent() {
        panic!("no description provided for `{}`; all `Configurable` types must define a description or be provided one when used within another `Configurable` type", std::any::type_name::<T>());
    }

    // Set the default value for this schema, if any.
    let schema_default = metadata
        .default_value()
        .map(|v| serde_json::to_value(v).expect("default value should never fail to serialize"));

    let schema_metadata = schemars::schema::Metadata {
        title: schema_title,
        description: schema_description,
        default: schema_default,
        deprecated: metadata.deprecated(),
        ..Default::default()
    };

    // Set any custom attributes as extensions on the schema.
    let mut custom_map = Map::new();
    for attribute in metadata.custom_attributes() {
        match attribute {
            CustomAttribute::Flag(key) => {
                custom_map.insert(key.to_string(), Value::Bool(true));
            }
            CustomAttribute::KeyValue { key, value } => {
                custom_map.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
    }

    if !custom_map.is_empty() {
        schema
            .extensions
            .insert("_metadata".to_string(), Value::Object(custom_map));
    }

    // Now apply any relevant validations.
    for validation in metadata.validations() {
        validation.apply(schema);
    }

    schema.metadata = Some(Box::new(schema_metadata));
}

pub fn convert_to_flattened_schema(primary: &mut SchemaObject, mut subschemas: Vec<SchemaObject>) {
    // Now we need to extract our object validation portion into a new schema object, add it to the list of subschemas,
    // and then update the primary schema to use `allOf`. It is not valid to "extend" a schema via `allOf`, hence why we
    // have to extract the primary schema object validation first.

    // First, we replace the primary schema with an empty schema, because we need to push it the actual primary schema
    // into the list of `allOf` schemas. This is due to the fact that it's not valid to "extend" a schema using `allOf`,
    // so everything has to be in there.
    let primary_subschema = mem::take(primary);
    subschemas.insert(0, primary_subschema);

    let all_of_schemas = subschemas.into_iter().map(Schema::Object).collect();

    // Now update the primary schema to use `allOf` to bring everything together.
    primary.subschemas = Some(Box::new(SubschemaValidation {
        all_of: Some(all_of_schemas),
        ..Default::default()
    }));
}

pub fn generate_null_schema() -> SchemaObject {
    SchemaObject {
        instance_type: Some(InstanceType::Null.into()),
        ..Default::default()
    }
}

pub fn generate_bool_schema() -> SchemaObject {
    SchemaObject {
        instance_type: Some(InstanceType::Boolean.into()),
        ..Default::default()
    }
}

pub fn generate_string_schema() -> SchemaObject {
    SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        ..Default::default()
    }
}

pub fn generate_number_schema<N>() -> SchemaObject
where
    N: Configurable + ConfigurableNumber,
{
    let minimum = N::get_enforced_min_bound();
    let maximum = N::get_enforced_max_bound();

    // We always set the minimum/maximum bound to the mechanical limits. Any additional constraining as part of field
    // validators will overwrite these limits.
    let mut schema = SchemaObject {
        instance_type: Some(InstanceType::Number.into()),
        number: Some(Box::new(NumberValidation {
            minimum: Some(minimum),
            maximum: Some(maximum),
            ..Default::default()
        })),
        ..Default::default()
    };

    // If the actual numeric type we're generating the schema for is a nonzero variant, and its constraint can't be
    // represently solely by the normal minimum/maximum bounds, we explicitly add an exclusion for the appropriate zero
    // value of the given numeric type.
    if N::requires_nonzero_exclusion() {
        schema.subschemas = Some(Box::new(SubschemaValidation {
            not: Some(Box::new(Schema::Object(SchemaObject {
                const_value: Some(Value::Number(N::get_encoded_zero_value())),
                ..Default::default()
            }))),
            ..Default::default()
        }));
    }

    schema
}

pub fn generate_array_schema<T>(gen: &mut SchemaGenerator, metadata: Metadata<T>) -> SchemaObject
where
    T: Configurable,
{
    // We generate the schema for `T` itself, and then apply any of `T`'s metadata to the given schema.
    let element_schema = T::generate_schema(gen, metadata);

    SchemaObject {
        instance_type: Some(InstanceType::Array.into()),
        array: Some(Box::new(ArrayValidation {
            items: Some(SingleOrVec::Single(Box::new(element_schema.into()))),
            ..Default::default()
        })),
        ..Default::default()
    }
}

pub fn generate_set_schema<T>(gen: &mut SchemaGenerator, metadata: Metadata<T>) -> SchemaObject
where
    T: Configurable,
{
    // We generate the schema for `T` itself, and then apply any of `T`'s metadata to the given schema.
    let element_schema = T::generate_schema(gen, metadata);

    SchemaObject {
        instance_type: Some(InstanceType::Array.into()),
        array: Some(Box::new(ArrayValidation {
            items: Some(SingleOrVec::Single(Box::new(element_schema.into()))),
            unique_items: Some(true),
            ..Default::default()
        })),
        ..Default::default()
    }
}

pub fn generate_map_schema<V>(gen: &mut SchemaGenerator, metadata: Metadata<V>) -> SchemaObject
where
    V: Configurable,
{
    // We generate the schema for `V` itself, and then apply any of `V`'s metadata to the given schema.
    let element_schema = V::generate_schema(gen, metadata);

    SchemaObject {
        instance_type: Some(InstanceType::Object.into()),
        object: Some(Box::new(ObjectValidation {
            additional_properties: Some(Box::new(element_schema.into())),
            ..Default::default()
        })),
        ..Default::default()
    }
}

pub fn generate_struct_schema(
    properties: IndexMap<String, SchemaObject>,
    required: BTreeSet<String>,
    additional_properties: Option<Box<Schema>>,
) -> SchemaObject {
    let properties = properties
        .into_iter()
        .map(|(k, v)| (k, Schema::Object(v)))
        .collect();
    SchemaObject {
        instance_type: Some(InstanceType::Object.into()),
        object: Some(Box::new(ObjectValidation {
            properties,
            required,
            additional_properties,
            ..Default::default()
        })),
        ..Default::default()
    }
}

pub fn generate_optional_schema<T>(gen: &mut SchemaGenerator, metadata: Metadata<T>) -> SchemaObject
where
    T: Configurable,
{
    // We generate the schema for `T` itself, and then apply any of `T`'s metadata to the given schema.
    let mut schema = T::generate_schema(gen, metadata);

    // We do a little dance here to add an additional instance type of "null" to the schema to
    // signal it can be "X or null", achieving the functional behavior of "this is optional".
    match schema.instance_type.as_mut() {
        // If this schema has no instance type, see if it's a reference schema.  If so, then we'd simply switch to
        // generating a composite schema with this schema reference and a generic null schema.
        None => match schema.is_ref() {
            false => panic!("tried to generate optional schema, but `T` had no instance type and was not a referencable schema"),
            true => {
                let null = generate_null_schema();

                // Drop the description from our generated schema if we're here, because it's going to exist on the
                // outer field wrapping this schema, and it looks wonky to have it nested within the composite schema.
                schema.metadata().description = None;

                return generate_composite_schema(&[null, schema])
            }
        },
        Some(sov) => match sov {
            SingleOrVec::Single(ty) if **ty != InstanceType::Null => {
                *sov = vec![**ty, InstanceType::Null].into()
            }
            SingleOrVec::Vec(ty) if !ty.contains(&InstanceType::Null) => {
                ty.push(InstanceType::Null)
            }
            _ => {}
        },
    }

    schema
}

pub fn generate_composite_schema(subschemas: &[SchemaObject]) -> SchemaObject {
    let subschemas = subschemas
        .iter()
        .map(|s| Schema::Object(s.clone()))
        .collect::<Vec<_>>();

    SchemaObject {
        subschemas: Some(Box::new(SubschemaValidation {
            one_of: Some(subschemas),
            ..Default::default()
        })),
        ..Default::default()
    }
}

pub fn generate_tuple_schema(subschemas: &[SchemaObject]) -> SchemaObject {
    let subschemas = subschemas
        .iter()
        .map(|s| Schema::Object(s.clone()))
        .collect::<Vec<_>>();

    SchemaObject {
        instance_type: Some(InstanceType::Array.into()),
        array: Some(Box::new(ArrayValidation {
            items: Some(SingleOrVec::Vec(subschemas)),
            // Rust's tuples are closed -- fixed size -- so we set `additionalItems` such that any
            // items past what we have in `items` will cause schema validation to fail.
            additional_items: Some(Box::new(Schema::Bool(false))),
            ..Default::default()
        })),
        ..Default::default()
    }
}

pub fn generate_const_string_schema(value: String) -> SchemaObject {
    SchemaObject {
        const_value: Some(Value::String(value)),
        ..Default::default()
    }
}

pub fn generate_internal_tagged_variant_schema(tag: String, value: String) -> SchemaObject {
    let mut properties = IndexMap::new();
    properties.insert(tag.clone(), generate_const_string_schema(value));

    let mut required = BTreeSet::new();
    required.insert(tag);

    generate_struct_schema(properties, required, None)
}

pub fn generate_root_schema<T>() -> RootSchema
where
    T: Configurable,
{
    let mut schema_gen = SchemaSettings::draft2019_09().into_generator();

    let schema = T::generate_schema(&mut schema_gen, Metadata::default());
    RootSchema {
        meta_schema: None,
        schema,
        definitions: schema_gen.take_definitions(),
    }
}
