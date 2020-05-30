use chrono::prelude::{DateTime, Utc};
use gdal::metadata::Metadata;
use gdal::raster::Dataset;
use geohash::Coordinate;
use swarm::prelude::Dht;
use zip::ZipArchive;

use crate::image::RAW_SOURCE;

use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::ffi::OsStr;
use std::fs::File;
use std::io::BufReader;
use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub fn process(dht: &Arc<RwLock<Dht>>, precision: usize, 
        record: &PathBuf, x_interval: f64, y_interval: f64)
        -> Result<(), Box<dyn Error>> {
    // compute tile name
    let tile_path = record.with_extension("");
    let tile = tile_path.file_name()
        .unwrap_or(OsStr::new("")).to_string_lossy();

    //println!("TILE: '{}'", tile);

    // open zip archive
    let file = File::open(&record)?;
    let reader = BufReader::new(file);
    let archive = ZipArchive::new(reader)?;

    // identify metadata xml file and band image files
    let mut zip_metadata_option = None;
    for filename in archive.file_names() {
        let path = PathBuf::from(&filename);

        if path.file_name() == Some(OsStr::new("MTD_MSIL1C.xml")) {
            zip_metadata_option = Some(filename);
        }
    }

    // check if we identified xml metadata file and band image files
    if zip_metadata_option == None {
        return Err("unable to find xml metadata file".into());
    }

    // open gdal metadata dataset - TODO error
    let zip_metadata = zip_metadata_option.unwrap();
    let metadata_filename = format!("/vsizip/{}/{}",
        record.to_string_lossy(), zip_metadata);
    let metadata_path = PathBuf::from(&metadata_filename);
    let dataset = Dataset::open(&metadata_path).unwrap();

    // parse metadata
    let timestamp = match dataset.metadata_item("PRODUCT_START_TIME", "") {
        Some(time) => time.parse::<DateTime<Utc>>()?.timestamp(),
        None => return Err("start time metadata not found".into()),
    };

    // populate subdatasets collection
    let mut subdatasets: Vec<(&str, &str)> = Vec::new();
    let mut count = 0;
    let metadata = dataset.metadata("SUBDATASETS");
    loop {
        if count + 1 >= metadata.len() {
            break;
        }

        // parse subdataset name
        let name_fields: Vec<&str> =
            metadata[count].split("=").collect();

        // parse subdataset desc
        let description_fields: Vec<&str> =
            metadata[count+1].split("=").collect();

        subdatasets.push((name_fields[1], description_fields[1]));
        count += 2;
    }

    // process data subsets
    for (i, (name, description)) in subdatasets.iter().enumerate() {
        // open dataset
        let path = PathBuf::from(name);
        let dataset = Dataset::open(&path).unwrap();

        // split image with geohash precision - TODO error
        for (dataset, _, win_max_x, _, win_max_y) in
                st_image::prelude::split(&dataset, 4326,
                    x_interval, y_interval).unwrap() {
            // compute window geohash
            let coordinate = Coordinate{x: win_max_x, y: win_max_y};
            let geohash = geohash::encode(coordinate, precision)?;

            // if image has 0.0 coverage -> don't process - TODO error
            let pixel_coverage = st_image::coverage(&dataset).unwrap();
            if pixel_coverage == 0f64 {
                continue;
            }

            // compute geohash hash
            let mut hasher = DefaultHasher::new();
            hasher.write(geohash.as_bytes());
            let hash = hasher.finish();

            // discover hash location - TODO move elsewhere
            let addr = {
                let dht = dht.read().unwrap(); 
                let (node_id, addrs) = match dht.locate(hash) {
                    Some(node) => node,
                    None => {
                        warn!("no dht location for hash {}", hash);
                        continue;
                    },
                };

                match addrs.1 {
                    Some(addr) => addr.clone(),
                    None => {
                        warn!("dht node {} has no xfer_addr", node_id);
                        continue;
                    },
                }
            };

            // send image to new host
            if let Err(e) = crate::transfer::send_image(&addr, &dataset,
                    description, &geohash, pixel_coverage, "Sentinel-2",
                    &RAW_SOURCE, i as u8, &tile, timestamp) {
                warn!("failed to write image to node {}: {}", addr, e);
            }
        }
    }

    Ok(())
}
