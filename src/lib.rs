#![allow(dead_code)]

use gdal::{errors::GdalError, Dataset, Metadata, MetadataEntry};
use nalgebra::Point2;
use ndarray::{Array2, Array3, ShapeError};
use proj::ProjCreateError;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use rasters::prelude::{
    transform_from_gdal, transform_window, ChunkReader, DatasetReader, PixelTransform,
};

pub type Result<T> = std::result::Result<T, RasterError>;

#[derive(thiserror::Error, Debug)]
pub enum RasterError {
    #[error(transparent)]
    GdalError(#[from] GdalError),
    #[error(transparent)]
    ProjError(#[from] ProjCreateError),
    #[error(transparent)]
    RastersError(#[from] rasters::Error),
    #[error(transparent)]
    ShapeError(#[from] ShapeError),
    #[error("Band `{0}` not found.")]
    BandNotFound(String),
    #[error("Band `{0}` has a non inverteble geo transform.")]
    BandTransformNotInvertible(String),
    #[error("Couldn't find metadata key {key} in dataset {dataset_path}.")]
    MetadataKeyNotFound { dataset_path: String, key: String },
    #[error("Dataset {0} contains bands with different projections.")]
    MultipleProjectionsInDataset(String)
}

type RasterMetadata = HashMap<String, String>;
type BandName = String;
type BandsInfo = HashMap<BandName, BandInfo>;

#[derive(Debug)]
pub struct Raster {
    path: PathBuf,
    bands_info: BandsInfo,
    pub metadata: RasterMetadata,
    pub proj: String,
    highest_resolution_transform: PixelTransform
}

type BandMetadata = HashMap<String, String>;

#[derive(Debug)]
struct BandInfo {
    index: usize,
    path: PathBuf,
    metadata: BandMetadata,
    proj: String,
    geo_transform: PixelTransform,
}

impl BandInfo {
    fn dataset(&self) -> Result<Dataset> {
        Dataset::open(&self.path).map_err(RasterError::GdalError)
    }

    fn reader(&self) -> Result<DatasetReader> {
        Ok(DatasetReader(self.dataset()?, self.index))
    }
}

type RasterSubDatasets = Vec<Dataset>;

impl Raster {
    fn parse_dataset(dataset: &Dataset) -> Result<(RasterMetadata, RasterSubDatasets)> {
        let mut raster_metadata = RasterMetadata::new();
        let mut raster_subdataset_paths = RasterSubDatasets::new();
        for MetadataEntry { domain, key, value } in dataset.metadata() {
            match domain.as_str() {
                "" => {
                    raster_metadata.insert(key, value);
                }
                "SUBDATASETS" if key.contains("NAME") => {
                    raster_subdataset_paths.push(Dataset::open(value)?)
                }
                _ => continue,
            }
        }
        Ok((raster_metadata, raster_subdataset_paths))
    }

    const BANDNAME_KEY: &'static str = "BANDNAME";

    fn parse_subdataset(dataset: Dataset) -> Result<Vec<(String, BandInfo)>> {
        let mut bands_info = Vec::new();
        let dataset_path = dataset.description()?;
        let geo_transform = transform_from_gdal(&dataset.geo_transform()?);
        let proj = dataset.projection();
        for (idx, raster_band) in dataset.rasterbands().enumerate() {
            let mut metadata = BandMetadata::new();
            for MetadataEntry { domain, key, value } in raster_band?.metadata() {
                match domain.as_str() {
                    "" => {
                        metadata.insert(key, value);
                    }
                    _ => continue,
                }
            }
            bands_info.push((
                metadata
                    .get(Self::BANDNAME_KEY)
                    .ok_or(RasterError::MetadataKeyNotFound {
                        key: String::from(Self::BANDNAME_KEY),
                        dataset_path: dataset_path.clone(),
                    })?
                    .to_string(),
                BandInfo {
                    index: idx + 1,
                    path: dataset_path.clone().into(),
                    metadata,
                    proj: proj.clone(),
                    geo_transform,
                }
            ));
        }
        dataset.close()?;
        Ok(bands_info)
    }

    pub fn new<P: AsRef<Path>>(path: P) -> Result<Raster> {
        let dataset = Dataset::open(&path)?;
        let (metadata, subdatasets) = Self::parse_dataset(&dataset)?;
        let bands_info = HashMap::from_iter(subdatasets
            .into_par_iter()
            // Don't use tci bands
            .filter(|dataset| !dataset.description().unwrap().contains("TCI"))
            .map(Self::parse_subdataset)
            .collect::<Result<Vec<Vec<(String, BandInfo)>>>>()?.into_iter().flatten());
        let mut projs = bands_info.values().map(|band_info| band_info.proj.clone()).collect::<HashSet<String>>();
        match projs.len() {
            1 => {
                let highest_resolution_transform = bands_info
                .values()
                .map(|band_info| band_info.geo_transform)
                .reduce(|prev, next| if prev.m11 < next.m11 { prev } else { next })
                .unwrap();
                Ok(Raster {
                path: path.as_ref().to_path_buf(),
                bands_info,
                metadata,
                proj: projs.drain().last().unwrap(),
                highest_resolution_transform
            })},
            _ => Err(RasterError::MultipleProjectionsInDataset(dataset.description()?))
        }
    }

    fn band_info(&self, band: &str) -> Result<&BandInfo> {
        self.bands_info
            .get(band)
            .ok_or(RasterError::BandNotFound(band.into()))
    }

    fn bands_info<'a>(&self, bands: &Vec<&'a str>) -> Result<Vec<(&'a str, &BandInfo)>> {
        bands
            .into_par_iter()
            .map(|band| self.band_info(band).map(|band_info| (*band, band_info)))
            .collect::<Result<Vec<(&str, &BandInfo)>>>()
    }

    pub fn read_bands(
        &self,
        bands: Vec<&'static str>,
        offset: (isize, isize),
        window: (usize, usize),
    ) -> Result<Array3<u16>> {
        let bands_info = self.bands_info(&bands)?;

        let band_rasters = bands_info
            .into_par_iter()
            .map(|(band, band_info)| {
                let transform = band_info
                    .geo_transform
                    .try_inverse()
                    .ok_or(RasterError::BandTransformNotInvertible((*band).into()))?
                    * self.highest_resolution_transform;
                let (corrected_offset, corrected_window) = transform_window(
                    (offset, window),
                    transform,
                    band_info.dataset()?.raster_size(),
                );
                band_info.reader()?
                    .read_as_array::<u16>(corrected_offset, corrected_window)
                    .map(|band_raster| (band_raster, transform))
                    .map_err(RasterError::RastersError)
            })
            .collect::<Result<Vec<(Array2<u16>, PixelTransform)>>>()?;

        Ok(Array3::from_shape_fn(
            (bands.len(), window.0, window.1),
            |(c, x, y)| {
                let (band_raster, transform) = &band_rasters[c];
                let corrected_coords = transform.transform_point(&Point2::new(x as f64, y as f64));
                band_raster[[corrected_coords.x as usize, corrected_coords.y as usize]]
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ndarray_npy::write_npy;
    use rstest::{fixture, rstest};

    const TEST_DATA: &str =
        "data/S2B_MSIL2A_20241126T093239_N0511_R136_T33PTM_20241126T120342.SAFE.zip";

    #[fixture]
    fn test_raster() -> Raster {
        Raster::new(TEST_DATA).unwrap()
    }

    #[rstest]
    fn it_works(test_raster: Raster) {
        print!(
            "{:#?}",
            test_raster
                .read_bands(vec!["B4", "B3", "B2"], (0, 0), (125, 125))
                .unwrap()
        );
    }

    #[rstest]
    fn play_ground(test_raster: Raster) {
        print!("{:#?}", test_raster);
    }

    #[rstest]
    fn to_npy(test_raster: Raster) {
        let rgb = ((test_raster
            .read_bands(vec!["B4", "B3", "B2"], (0, 0), (100, 100))
            .unwrap()
            .reversed_axes()
            / 100)
            * 255)
            / 100;

        write_npy("dev/test.npy", &rgb).unwrap();
        //println!("{:?}", rgb);
    }
}
