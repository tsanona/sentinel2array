use gdal::Metadata as GdalMetadata;
use rasters::prelude::{transform_from_gdal, PixelTransform, RasterPathReader};
use std::{
    collections::{hash_map::Entry, HashMap},
    path::PathBuf,
    rc::Rc,
};

use super::{Sentinel2ArrayError, Result};

#[derive(Debug)]
pub struct BandGroup {
    gdal_dataset_path: PathBuf,
    pub crs: String,
    pub geo_transform: PixelTransform,
}

impl BandGroup {
    pub fn new(dataset: &gdal::Dataset) -> Result<Self> {
        let gdal_dataset_path = dataset.description()?.into();
        let crs = dataset.projection();
        dataset
            .geo_transform()
            .map(|geo_transform| Self {
                gdal_dataset_path,
                crs,
                geo_transform: transform_from_gdal(&geo_transform),
            })
            .map_err(Sentinel2ArrayError::GdalError)
    }

    fn band_reader(&self, band_index: usize) -> RasterPathReader<PathBuf> {
        RasterPathReader(&self.gdal_dataset_path, band_index)
    }
}

#[derive(Debug)]
pub struct BandInfo<BM> {
    index: usize,
    group: Rc<BandGroup>,
    metadata: BM,
}

impl<BM> BandInfo<BM> {
    pub fn new(group: Rc<BandGroup>, index: usize, metadata: BM) -> Self {
        Self {
            index,
            group,
            metadata,
        }
    }
    pub fn resolution(&self) -> u8 {
        self.group.geo_transform.m11 as u8
    }

    pub fn reader(&self) -> RasterPathReader<PathBuf> {
        self.group.band_reader(self.index)
    }
}

#[derive(Debug, Default)]
pub struct Bands<BM>(HashMap<String, BandInfo<BM>>) where BM: Default;

impl<BM> Bands<BM> where BM: Default {
    pub fn get(&self, band_name: &String) -> Result<&BandInfo<BM>> {
        self.0
            .get(band_name)
            .ok_or(Sentinel2ArrayError::BandNotFound(band_name.clone()))
    }
    pub fn names(&self) -> Vec<&String> {
        let mut names = self.0.keys().collect::<Vec<&String>>();
        names.sort();
        names
    }

    fn insert(mut self, band_name: String, band_info: BandInfo<BM>) -> Self {
        match self.0.entry(band_name) {
            Entry::Occupied(entry) if entry.get().resolution() < band_info.resolution() => entry,
            entry => entry.insert_entry(band_info),
        };
        self
    }
}

impl<BM> FromIterator<(String, BandInfo<BM>)> for Bands<BM> where BM: Default {
    fn from_iter<T: IntoIterator<Item = (String, BandInfo<BM>)>>(iter: T) -> Self {
        iter.into_iter()
            .fold(Bands::default(), |bands, (band_name, band_info)| {
                bands.insert(band_name, band_info)
            })
    }
}
