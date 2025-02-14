use arrow::array::{ArrayData, LargeListArray, ListArray, MapArray, StructArray, UnionArray};
use arrow::datatypes::{Field, Schema};
use arrow::ffi::FFI_ArrowArray;
use arrow::record_batch::RecordBatch;
use conduit::{ComponentId, ComponentType, EntityId, PrimitiveTy};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;
use polars::prelude::SerReader;
use polars::{frame::DataFrame, series::Series};
use polars_arrow::{
    array::{Array, PrimitiveArray},
    datatypes::ArrowDataType,
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::{collections::BTreeMap, fs::File, path::Path};

use crate::{
    ArchetypeId, AssetStore, Column, ColumnRef, ColumnStore, Error, HostColumn, HostStore, Table,
    World, WorldStore,
};

const ENTITY_ID_COMPONENT: ComponentId = ComponentId::new("entity_id");

#[derive(Debug, Clone)]
pub struct PolarsWorld {
    pub archetypes: BTreeMap<ArchetypeId, DataFrame>,
    pub metadata: Metadata,
    pub assets: AssetStore,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Metadata {
    pub archetypes: BTreeMap<ArchetypeId, ArchetypeMetadata>,
    pub component_map: HashMap<ComponentId, ArchetypeId>,
    pub tick: u64,
    pub entity_len: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArchetypeMetadata {
    pub columns: Vec<ColumnMetadata>,
    pub entity_map: BTreeMap<EntityId, usize>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ColumnMetadata {
    pub metadata: conduit::Metadata,
    pub asset: bool,
}

impl PolarsWorld {
    pub fn write_to_dir(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let mut metadata = File::create(path.join("metadata.json"))?;
        serde_json::to_writer(&mut metadata, &self.metadata)?;
        for (archetype_id, df) in &mut self.archetypes {
            let path = path.join(format!("{}.parquet", archetype_id.to_raw()));
            let file = std::fs::File::create(&path)?;
            let props = WriterProperties::default();
            let record_batch = df.to_record_batch()?;
            let mut writer =
                ArrowWriter::try_new(file, record_batch.record_batch().schema(), Some(props))
                    .unwrap();
            writer.write(record_batch.record_batch()).unwrap();
            writer.close().unwrap();
        }
        let path = path.join("assets.bin");
        let file = std::fs::File::create(path)?;
        postcard::to_io(&self.assets, file).unwrap();
        Ok(())
    }

    pub fn read_from_dir(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let mut archetypes = BTreeMap::new();
        let mut metadata = File::open(path.join("metadata.json"))?;
        let metadata: Metadata = serde_json::from_reader(&mut metadata)?;
        for id in metadata.archetypes.keys() {
            let path = path.join(format!("{}.parquet", id.to_raw()));
            let file = File::open(&path)?;
            let df = polars::prelude::ParquetReader::new(file).finish()?;
            archetypes.insert(*id, df);
        }
        let assets_buf = std::fs::read(path.join("assets.bin"))?;
        let assets = postcard::from_bytes(&assets_buf)?;
        Ok(Self {
            archetypes,
            metadata,
            assets,
        })
    }
}

impl World<HostStore> {
    pub fn to_polars(&self) -> Result<PolarsWorld, Error> {
        let mut archetypes = BTreeMap::new();
        let mut archetype_metadata = BTreeMap::new();
        for (id, table) in &self.archetypes {
            let (metadata, df) = table.to_polars()?;
            archetypes.insert(*id, df);
            archetype_metadata.insert(*id, metadata);
        }

        let metadata = Metadata {
            archetypes: archetype_metadata,
            component_map: self.component_map.clone(),
            tick: self.tick,
            entity_len: self.entity_len,
        };

        Ok(PolarsWorld {
            archetypes,
            metadata,
            assets: self.assets.clone(),
        })
    }
}

impl TryFrom<PolarsWorld> for World<HostStore> {
    type Error = Error;

    fn try_from(polars: PolarsWorld) -> Result<Self, Self::Error> {
        let Metadata {
            archetypes,
            component_map,
            tick,
            entity_len,
        } = polars.metadata;
        let archetypes = polars
            .archetypes
            .into_iter()
            .zip(archetypes.into_values())
            .map(|((id, df), metadata)| {
                let table = Table::from_dataframe(df, metadata)?;
                Ok((id, table))
            })
            .collect::<Result<_, Error>>()?;
        Ok(World {
            archetypes,
            component_map,
            assets: polars.assets,
            tick,
            entity_len,
        })
    }
}

impl<W: WorldStore> PartialEq for Column<W>
where
    W::Column: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.buffer == other.buffer
    }
}

impl PartialEq for Table<HostStore> {
    fn eq(&self, other: &Self) -> bool {
        self.columns == other.columns
            && self.entity_buffer == other.entity_buffer
            && self.entity_map == other.entity_map
    }
}

impl Table<HostStore> {
    pub fn from_dataframe(df: DataFrame, metadata: ArchetypeMetadata) -> Result<Self, Error> {
        let entity_id_string = ENTITY_ID_COMPONENT.0.to_string();
        let columns = metadata
            .columns
            .iter()
            .zip(df.iter().filter(|s| s.name() != entity_id_string))
            .map(|(metadata, series)| {
                let asset = metadata.asset;
                let buffer = HostColumn::from_series(
                    series,
                    metadata.metadata.component_type.clone(),
                    asset,
                )?;
                let column = Column {
                    buffer,
                    metadata: metadata.metadata.clone(),
                };
                Ok((metadata.metadata.component_id, column))
            })
            .collect::<Result<_, Error>>()?;
        let column = df
            .column(&entity_id_string)
            .map_err(|_| Error::ComponentNotFound)?;
        let entity_buffer = HostColumn::from_series(column, ComponentType::u64(), false)?;

        Ok(Self {
            columns,
            entity_buffer,
            entity_map: metadata.entity_map,
        })
    }

    pub fn to_polars(&self) -> Result<(ArchetypeMetadata, DataFrame), Error> {
        let columns = self
            .columns
            .values()
            .map(|c| ColumnMetadata {
                metadata: c.metadata.clone(),
                asset: c.buffer.asset,
            })
            .collect();
        let metadata = ArchetypeMetadata {
            columns,
            entity_map: self.entity_map.clone(),
        };

        Ok((
            metadata,
            self.columns
                .values()
                .map(|c| &c.buffer)
                .chain(std::iter::once(&self.entity_buffer))
                .map(HostColumn::to_series)
                .collect::<Result<DataFrame, Error>>()?,
        ))
    }
}

impl HostColumn {
    pub fn from_series(
        series: &Series,
        component_type: ComponentType,
        asset: bool,
    ) -> Result<Self, Error> {
        let buf = series.to_bytes();
        let len = series.len();
        let component_id: u64 = series
            .name()
            .parse()
            .map_err(|_| Error::InvalidComponentId)?;
        let component_id = ComponentId(component_id);
        Ok(Self {
            buf,
            len,
            component_id,
            component_type,
            asset,
        })
    }

    pub fn to_series(&self) -> Result<Series, Error> {
        let array = match self.component_type.primitive_ty {
            PrimitiveTy::F64 => tensor_array(&self.component_type, self.prim_array::<f64>()),
            PrimitiveTy::F32 => tensor_array(&self.component_type, self.prim_array::<f32>()),
            PrimitiveTy::U64 => tensor_array(&self.component_type, self.prim_array::<u64>()),
            PrimitiveTy::U32 => tensor_array(&self.component_type, self.prim_array::<u32>()),
            PrimitiveTy::U16 => tensor_array(&self.component_type, self.prim_array::<u16>()),
            PrimitiveTy::U8 => tensor_array(&self.component_type, self.prim_array::<u8>()),
            PrimitiveTy::I64 => tensor_array(&self.component_type, self.prim_array::<i64>()),
            PrimitiveTy::I32 => tensor_array(&self.component_type, self.prim_array::<i32>()),
            PrimitiveTy::I16 => tensor_array(&self.component_type, self.prim_array::<i16>()),
            PrimitiveTy::I8 => tensor_array(&self.component_type, self.prim_array::<i8>()),
            PrimitiveTy::Bool => todo!(),
        };
        Series::from_arrow(&self.component_id.0.to_string(), array).map_err(Error::from)
    }

    fn prim_array<T: polars_arrow::types::NativeType + nox::xla::ArrayElement>(
        &self,
    ) -> Box<dyn Array> {
        Box::new(PrimitiveArray::from_slice(self.typed_buf::<T>().unwrap()))
    }
}

fn arrow_data_type(ty: PrimitiveTy) -> ArrowDataType {
    match ty {
        PrimitiveTy::U8 => ArrowDataType::UInt8,
        PrimitiveTy::U16 => ArrowDataType::UInt16,
        PrimitiveTy::U32 => ArrowDataType::UInt32,
        PrimitiveTy::U64 => ArrowDataType::UInt64,
        PrimitiveTy::I8 => ArrowDataType::Int8,
        PrimitiveTy::I16 => ArrowDataType::Int16,
        PrimitiveTy::I32 => ArrowDataType::Int32,
        PrimitiveTy::I64 => ArrowDataType::Int64,
        PrimitiveTy::F32 => ArrowDataType::Float32,
        PrimitiveTy::F64 => ArrowDataType::Float64,
        PrimitiveTy::Bool => ArrowDataType::Boolean,
    }
}

fn tensor_array(ty: &ComponentType, inner: Box<dyn Array>) -> Box<dyn Array> {
    let data_type = arrow_data_type(ty.primitive_ty);
    if ty.shape.is_empty() {
        return inner;
    }
    let data_type = ArrowDataType::FixedSizeList(
        Box::new(polars_arrow::datatypes::Field::new(
            "inner", data_type, false,
        )),
        ty.shape.iter().product::<usize>(),
    );
    Box::new(polars_arrow::array::FixedSizeListArray::new(
        data_type, inner, None,
    ))
    // let metadata = HashMap::from_iter([(
    //     "ARROW:extension:metadata".to_string(),
    //     format!("{{ \"shape\": {:?} }}", shape),
    // )]);
    // (data_type, Some(metadata))
}

pub struct RecordBatchRef<'a> {
    phantom_data: PhantomData<&'a ()>,
    record_batch: arrow::record_batch::RecordBatch,
}

impl<'a> RecordBatchRef<'a> {
    fn record_batch<'b>(&'b self) -> &'a arrow::record_batch::RecordBatch
    where
        'b: 'a,
    {
        &self.record_batch
    }
}

pub trait DataFrameConv {
    fn to_record_batch(&self) -> Result<RecordBatchRef<'_>, Error>;
}

impl DataFrameConv for DataFrame {
    fn to_record_batch(&self) -> Result<RecordBatchRef<'_>, Error> {
        let mut fields = vec![];
        let mut columns = vec![];
        for series in self.iter() {
            let name = series.name();
            // safety: `to_array_data` is unsafe because it creates a unlifetimed
            // reference to `Series`, using `RecordBatchRef` we ensure
            // that Series's lifetime is tied to the RecordBatch lifetime,
            // so the `Series` will always be alive while the `RecordBatch` is
            let array_data = unsafe { series.to_array_data() };
            let array: Arc<dyn arrow::array::Array> = match array_data.data_type() {
                arrow::datatypes::DataType::Null => {
                    Arc::new(arrow::array::NullArray::from(array_data))
                }
                arrow::datatypes::DataType::Boolean => {
                    Arc::new(arrow::array::BooleanArray::from(array_data))
                }
                arrow::datatypes::DataType::Int8 => {
                    Arc::new(arrow::array::Int8Array::from(array_data))
                }
                arrow::datatypes::DataType::Int16 => {
                    Arc::new(arrow::array::Int16Array::from(array_data))
                }
                arrow::datatypes::DataType::Int32 => {
                    Arc::new(arrow::array::Int32Array::from(array_data))
                }
                arrow::datatypes::DataType::Int64 => {
                    Arc::new(arrow::array::Int64Array::from(array_data))
                }
                arrow::datatypes::DataType::UInt8 => {
                    Arc::new(arrow::array::UInt8Array::from(array_data))
                }
                arrow::datatypes::DataType::UInt16 => {
                    Arc::new(arrow::array::UInt16Array::from(array_data))
                }
                arrow::datatypes::DataType::UInt32 => {
                    Arc::new(arrow::array::UInt32Array::from(array_data))
                }
                arrow::datatypes::DataType::UInt64 => {
                    Arc::new(arrow::array::UInt64Array::from(array_data))
                }
                arrow::datatypes::DataType::Float16 => {
                    Arc::new(arrow::array::Float16Array::from(array_data))
                }
                arrow::datatypes::DataType::Float32 => {
                    Arc::new(arrow::array::Float32Array::from(array_data))
                }
                arrow::datatypes::DataType::Float64 => {
                    Arc::new(arrow::array::Float64Array::from(array_data))
                }
                arrow::datatypes::DataType::Timestamp(_, _) => todo!(),
                arrow::datatypes::DataType::Date32 => {
                    Arc::new(arrow::array::Date32Array::from(array_data))
                }
                arrow::datatypes::DataType::Date64 => {
                    Arc::new(arrow::array::Date64Array::from(array_data))
                }
                arrow::datatypes::DataType::Time32(u) => match u {
                    arrow::datatypes::TimeUnit::Second => {
                        Arc::new(arrow::array::Time32SecondArray::from(array_data))
                    }
                    arrow::datatypes::TimeUnit::Millisecond => {
                        Arc::new(arrow::array::Time32MillisecondArray::from(array_data))
                    }
                    arrow::datatypes::TimeUnit::Microsecond => {
                        unimplemented!()
                    }
                    arrow::datatypes::TimeUnit::Nanosecond => {
                        unimplemented!()
                    }
                },
                arrow::datatypes::DataType::Time64(u) => match u {
                    arrow::datatypes::TimeUnit::Second => {
                        todo!()
                    }
                    arrow::datatypes::TimeUnit::Millisecond => {
                        todo!()
                    }
                    arrow::datatypes::TimeUnit::Microsecond => {
                        Arc::new(arrow::array::Time64MicrosecondArray::from(array_data))
                    }
                    arrow::datatypes::TimeUnit::Nanosecond => {
                        Arc::new(arrow::array::Time64NanosecondArray::from(array_data))
                    }
                },
                arrow::datatypes::DataType::Duration(_) => todo!(),
                arrow::datatypes::DataType::Interval(_) => todo!(),
                arrow::datatypes::DataType::Binary => {
                    Arc::new(arrow::array::BinaryArray::from(array_data))
                }
                arrow::datatypes::DataType::FixedSizeBinary(_) => {
                    Arc::new(arrow::array::FixedSizeBinaryArray::from(array_data))
                }
                arrow::datatypes::DataType::LargeBinary => {
                    Arc::new(arrow::array::LargeBinaryArray::from(array_data))
                }
                arrow::datatypes::DataType::Utf8 => todo!(),
                arrow::datatypes::DataType::LargeUtf8 => todo!(),
                arrow::datatypes::DataType::List(_) => Arc::new(ListArray::from(array_data)),
                arrow::datatypes::DataType::FixedSizeList(_, _) => {
                    Arc::new(arrow::array::FixedSizeListArray::from(array_data))
                }
                arrow::datatypes::DataType::LargeList(_) => {
                    Arc::new(LargeListArray::from(array_data))
                }
                arrow::datatypes::DataType::Struct(_) => Arc::new(StructArray::from(array_data)),
                arrow::datatypes::DataType::Union(_, _) => Arc::new(UnionArray::from(array_data)),
                arrow::datatypes::DataType::Dictionary(_, _) => {
                    todo!()
                }
                arrow::datatypes::DataType::Decimal128(_, _) => todo!(),
                arrow::datatypes::DataType::Decimal256(_, _) => todo!(),
                arrow::datatypes::DataType::Map(_, _) => Arc::new(MapArray::from(array_data)),
                arrow::datatypes::DataType::RunEndEncoded(_, _) => todo!(),
            };

            let field = Field::new(name, array.data_type().clone(), false);
            fields.push(field);
            columns.push(array);
        }
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema, columns)?;
        Ok(RecordBatchRef {
            phantom_data: PhantomData,
            record_batch: batch,
        })
    }
}

pub trait SeriesExt {
    fn to_bytes(&self) -> Vec<u8>;
    unsafe fn to_array_data(&self) -> ArrayData;
}

impl SeriesExt for Series {
    fn to_bytes(&self) -> Vec<u8> {
        // safety: we ensure that we only use the
        // returned `ArrayData` while `Series` is in
        // scope, so this is safe
        let data = unsafe { self.to_array_data() };
        let mut out = Vec::default();
        recurse_array_data(&data, &mut out);
        out
    }

    unsafe fn to_array_data(&self) -> ArrayData {
        let array = self.to_arrow(0, false);
        let field = self.field();
        let field = field.to_arrow(false);
        let schema = polars_arrow::ffi::export_field_to_c(&field);
        // safety: these two types have identical layouts
        // as they are both guarenteed to match the c-ffi layout
        let schema = unsafe { std::mem::transmute(schema) };
        let array = polars_arrow::ffi::export_array_to_c(array);
        // safety: these two types have identical layouts
        // as they are both guarenteed to match the c-ffi layout
        let array: FFI_ArrowArray = unsafe { std::mem::transmute(array) };
        // safety: this function requires the user ensure that `Series`
        // is alive while `ArrayData` is accessible
        unsafe { arrow::ffi::from_ffi(array, &schema) }.expect("polars arrow layout disagreement")
    }
}

pub fn recurse_array_data(array_data: &ArrayData, out: &mut Vec<u8>) {
    for child in array_data.child_data() {
        recurse_array_data(child, out)
    }
    for buffer in array_data.buffers() {
        out.extend_from_slice(buffer.as_slice())
    }
}

pub struct PolarsColumnRef<'a> {
    entity_series: &'a Series,
    buf: &'a Series,
}

impl<'a> ColumnStore for &'a PolarsWorld {
    type Column<'b> = PolarsColumnRef<'b> where Self: 'b;

    fn transfer_column(&mut self, _id: ComponentId) -> Result<(), Error> {
        Ok(())
    }

    fn column(&self, id: ComponentId) -> Result<Self::Column<'_>, Error> {
        let entity_id_string = ENTITY_ID_COMPONENT.0.to_string();
        let archetype = self
            .metadata
            .component_map
            .get(&id)
            .ok_or(Error::ComponentNotFound)?;
        let table = self
            .archetypes
            .get(archetype)
            .ok_or(Error::ComponentNotFound)?;
        Ok(PolarsColumnRef {
            entity_series: table.column(&entity_id_string)?,
            buf: table.column(&id.0.to_string())?, // TODO(sphw): add a map to metadata between component id and series offset
        })
    }

    fn assets(&self) -> Option<&AssetStore> {
        None
    }

    fn tick(&self) -> u64 {
        self.metadata.tick
    }
}

impl ColumnRef for PolarsColumnRef<'_> {
    fn len(&self) -> usize {
        self.entity_series.len()
    }

    fn entity_buf(&self) -> std::borrow::Cow<'_, [u8]> {
        Cow::Owned(self.entity_series.to_bytes())
    }

    fn value_buf(&self) -> std::borrow::Cow<'_, [u8]> {
        Cow::Owned(self.buf.to_bytes())
    }

    fn is_asset(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        six_dof::{Body, Force, Inertia, WorldAccel, WorldVel},
        WorldPos,
    };
    use conduit::{
        well_known::{Material, Mesh, Pbr},
        ComponentId,
    };
    use nox::{
        nalgebra::{self, vector},
        SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform,
    };
    use polars::prelude::*;
    use polars_arrow::array::Float64Array;

    use super::*;

    #[test]
    fn test_convert_to_df() {
        let mut world = World::default();
        let pbr = world.insert_asset(Pbr::Bundle {
            mesh: Mesh::sphere(0.1, 36, 18),
            material: Material::color(1.0, 1.0, 1.0),
        });

        world.spawn(Body {
            pos: WorldPos(SpatialTransform {
                inner: vector![1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0].into(),
            }),
            vel: WorldVel(SpatialMotion {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 1.0].into(),
            }),
            accel: WorldAccel(SpatialMotion {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 0.0].into(),
            }),
            pbr,
            force: Force(SpatialForce {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 0.0].into(),
            }),
            mass: Inertia(SpatialInertia {
                inner: vector![1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0].into(),
            }),
        });
        let polars = world.to_polars().unwrap();
        let df = polars.archetypes[&ArchetypeId::of::<Body>()].clone();
        let out = df
            .lazy()
            .select(&[col(&ComponentId::new("world_pos").0.to_string())])
            .collect()
            .unwrap();
        let pos = out
            .iter()
            .next()
            .unwrap()
            .array()
            .unwrap()
            .get(0)
            .unwrap()
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap()
            .iter()
            .filter_map(|f| f.copied())
            .collect::<Vec<_>>();
        assert_eq!(pos, &[1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn test_write_read_file() {
        let mut world = World::default();
        let pbr = world.insert_asset(Pbr::Bundle {
            mesh: Mesh::sphere(0.1, 36, 18),
            material: Material::color(1.0, 1.0, 1.0),
        });
        world.spawn(Body {
            pos: WorldPos(SpatialTransform {
                inner: vector![1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0].into(),
            }),
            vel: WorldVel(SpatialMotion {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 1.0].into(),
            }),
            accel: WorldAccel(SpatialMotion {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 0.0].into(),
            }),
            pbr,
            force: Force(SpatialForce {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 0.0].into(),
            }),
            mass: Inertia(SpatialInertia {
                inner: vector![1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0].into(),
            }),
        });
        let mut polars = world.to_polars().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        polars.write_to_dir(&dir).unwrap();
        let new_polars = PolarsWorld::read_from_dir(&dir).unwrap();
        assert_eq!(polars.archetypes, new_polars.archetypes);
    }

    #[test]
    fn test_to_world() {
        let mut world = World::default();
        let pbr = world.insert_asset(Pbr::Bundle {
            mesh: Mesh::sphere(0.1, 36, 18),
            material: Material::color(1.0, 1.0, 1.0),
        });

        world.spawn(Body {
            pos: WorldPos(SpatialTransform {
                inner: vector![1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0].into(),
            }),
            vel: WorldVel(SpatialMotion {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 1.0].into(),
            }),
            accel: WorldAccel(SpatialMotion {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 0.0].into(),
            }),
            pbr,
            force: Force(SpatialForce {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 0.0].into(),
            }),
            mass: Inertia(SpatialInertia {
                inner: vector![1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0].into(),
            }),
        });
        let polars = world.to_polars().unwrap();
        let new_world = World::try_from(polars).unwrap();
        assert_eq!(new_world.archetypes, world.archetypes);
    }

    #[test]
    fn test_write_read_world() {
        let mut world = World::default();
        let pbr = world.insert_asset(Pbr::Bundle {
            mesh: Mesh::sphere(0.1, 36, 18),
            material: Material::color(1.0, 1.0, 1.0),
        });
        world.spawn(Body {
            pos: WorldPos(SpatialTransform {
                inner: vector![1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0].into(),
            }),
            vel: WorldVel(SpatialMotion {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 1.0].into(),
            }),
            accel: WorldAccel(SpatialMotion {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 0.0].into(),
            }),
            pbr,
            force: Force(SpatialForce {
                inner: vector![0.0, 0.0, 0.0, 0.0, 0.0, 0.0].into(),
            }),
            mass: Inertia(SpatialInertia {
                inner: vector![1.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0].into(),
            }),
        });
        let mut polars = world.to_polars().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        polars.write_to_dir(&dir).unwrap();
        let new_polars = PolarsWorld::read_from_dir(&dir).unwrap();
        let new_world = World::try_from(new_polars).unwrap();
        assert_eq!(new_world.archetypes, world.archetypes);
    }
}
