//! EUMETNET OPERA COMP DBZH GridFamily adapter: anonymous ORD 24h COG
//! (`docs/grid-adapters.md`). Live fetch only; no vendored GeoTIFF.

use crate::{
    cog::{self, DecodedRaster},
    live_index,
    protocol::{
        AdapterTarget, Coverage, Crs, Ellipsoid, Family, FrameStatus, Kind, MosaicFrame,
        ProductClass,
    },
    source::{AdapterCoverage, SourceMetadataBorrowed},
    sweep,
};
use chrono::{NaiveDate, NaiveDateTime, Utc};
use std::{collections::HashSet, time::Duration};
use tokio::{sync::mpsc::Sender, task::JoinHandle, time::sleep};
use xml::reader::{EventReader, XmlEvent};

pub const ID: &str = "opera";
pub const NAME: &str = "EUMETNET OPERA";
pub const ATTRIBUTION: &str = "EUMETNET OPERA";
/// Continental fallback: below fixture (100) and future national mosaics.
pub const SELECTION_PRIORITY: i32 = 10;

pub const HOST: &str = "https://s3.waw3-1.cloudferro.com/openradar-24h";
/// Service footprint from ORD docs (approx corners), not per-frame nodata.
pub const COVERAGE_NORTH: f64 = 70.0;
pub const COVERAGE_SOUTH: f64 = 32.0;
pub const COVERAGE_WEST: f64 = -30.0;
pub const COVERAGE_EAST: f64 = 50.0;

pub const LAT0_DEG: f64 = 55.0;
pub const LON0_DEG: f64 = 10.0;
pub const FALSE_EASTING_M: f64 = 1_950_000.0;
pub const FALSE_NORTHING_M: f64 = -2_100_000.0;

const BODY_MAX: usize = 12 << 20;
/// Timeline depth: matches NEXRAD's initial `BACKFILL_VOLUMES` dozen so
/// playback has the same loop length on first select. Live growth stays
/// capped here (not the polar catalog ring of 60) — each COMP is multi-MB.
pub const HISTORY_MAX: usize = 12;
const START_TIMEOUT: Duration = Duration::from_secs(45);
const IDLE: Duration = Duration::from_secs(30);
/// After the newest COMP is on screen, pull earlier stamps (NEXRAD waits
/// the same beat so a hand-off mid-pan does not spend bandwidth).
const BACKFILL_DELAY: Duration = Duration::from_secs(3);
const FILL: f32 = -9_999_000.0;

/// Grid poller events for `main.rs`.
pub enum Event {
    /// Newest COMP: joins the timeline and takes the screen while following.
    Frame {
        frame: Box<MosaicFrame>,
        texture: Vec<u8>,
        start_ms: i64,
    },
    /// Earlier COMP from the backfill window: timeline only, screen stays.
    Backfill {
        frame: Box<MosaicFrame>,
        texture: Vec<u8>,
        start_ms: i64,
    },
    Offline {
        reason: String,
    },
    Silent {
        reason: String,
    },
}

pub struct Opera {
    pub id: &'static str,
    coverage: Coverage,
    palette: Vec<String>,
    bounds: Vec<f64>,
}

impl Opera {
    pub fn new() -> Self {
        let template: crate::protocol::Frame =
            serde_json::from_str(include_str!("../data/fixture.json")).unwrap();
        Self {
            id: ID,
            coverage: Coverage::Box {
                north: COVERAGE_NORTH,
                south: COVERAGE_SOUTH,
                east: COVERAGE_EAST,
                west: COVERAGE_WEST,
            },
            palette: template.palette,
            bounds: template.bounds.iter().map(|&b| b as f64).collect(),
        }
    }

    pub fn metadata(&self) -> SourceMetadataBorrowed<'_> {
        SourceMetadataBorrowed {
            id: self.id,
            family: Family::Grid,
            kind: Kind::Mosaic,
            default_product_class: ProductClass::Reflectivity,
            name: NAME,
            attribution: ATTRIBUTION,
            coverage: AdapterCoverage::Mosaic {
                coverage: &self.coverage,
                selection_priority: SELECTION_PRIORITY,
            },
        }
    }

    pub fn crs() -> Crs {
        Crs::LambertAzimuthalEqualArea {
            ellipsoid: Ellipsoid::WGS84,
            lat0_deg: LAT0_DEG,
            lon0_deg: LON0_DEG,
            false_easting_m: FALSE_EASTING_M,
            false_northing_m: FALSE_NORTHING_M,
            datum_transform: None,
        }
    }

    pub fn poll(
        &self,
        target: &AdapterTarget,
        events: Sender<Event>,
        known_keys: HashSet<String>,
    ) -> Option<JoinHandle<()>> {
        match target {
            AdapterTarget::Mosaic => {
                let palette = self.palette.clone();
                let bounds = self.bounds.clone();
                Some(tokio::spawn(async move {
                    poll_loop(events, known_keys, palette, bounds).await;
                }))
            }
            AdapterTarget::Site { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompObject {
    pub key: String,
    pub stamp: NaiveDateTime,
}

/// Parse ListObjects v2 XML for `OPERA@…@DBZH.tiff` keys.
pub fn parse_dbzh_listing(body: &str) -> Result<Vec<CompObject>, String> {
    let mut objects = Vec::new();
    let mut in_key = false;
    let mut key = None::<String>;
    for event in EventReader::new(body.as_bytes()) {
        match event.map_err(|e| format!("reading OPERA listing: {e}"))? {
            XmlEvent::StartElement { name, .. } if name.local_name == "Key" => {
                in_key = true;
                key = Some(String::new());
            }
            XmlEvent::Characters(text) if in_key => {
                if let Some(k) = key.as_mut() {
                    k.push_str(&text);
                }
            }
            XmlEvent::EndElement { name } if name.local_name == "Key" => {
                in_key = false;
                if let Some(k) = key.take()
                    && let Some(stamp) = parse_dbzh_key(&k)
                {
                    objects.push(CompObject { key: k, stamp });
                }
            }
            _ => {}
        }
    }
    objects.sort_by_key(|o| o.stamp);
    Ok(objects)
}

/// `…/OPERA/COMP/OPERA@YYYYMMDDTHHMM@0@DBZH.tiff`
pub fn parse_dbzh_key(key: &str) -> Option<NaiveDateTime> {
    let name = key.rsplit('/').next()?;
    let rest = name.strip_prefix("OPERA@")?;
    let stamp = rest.strip_suffix("@0@DBZH.tiff")?;
    NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M").ok()
}

pub fn prefix_for(day: NaiveDate) -> String {
    format!("{}/OPERA/COMP/", day.format("%Y/%m/%d"))
}

/// Classify OPERA DBZH floats into the grid texture encoding.
pub fn classify(values: &[f32], nodata: Option<f32>, bounds: &[f64], classes: usize) -> Vec<u8> {
    let classes = classes.max(1);
    let fill = nodata.unwrap_or(FILL);
    let mut pixels = Vec::with_capacity(values.len() * 4);
    for &v in values {
        let (r, g) = if v.is_nan() {
            (0, 2) // undetect
        } else if (v - fill).abs() < 1.0 || v <= fill / 2.0 {
            // Missing / nodata (G bit 0). Grid shader draws transparent —
            // nodata draws nothing; oceans must not use the polar folded hatch.
            (0, 1)
        } else {
            let class = class_of(v as f64, bounds, classes);
            (class + 1, 0)
        };
        pixels.extend_from_slice(&[r, g, 0, 255]);
    }
    pixels
}

fn class_of(value: f64, bounds: &[f64], classes: usize) -> u8 {
    if bounds.len() < 2 {
        return 0;
    }
    let mut above = 0usize;
    for (i, edge) in bounds.iter().enumerate().skip(1) {
        if value >= bounds[i - 1] && value < *edge {
            return (i - 1).min(classes - 1) as u8;
        }
        if value >= *edge {
            above = i;
        }
    }
    above.saturating_sub(1).min(classes - 1) as u8
}

/// Decode a COMP DBZH COG into a mosaic frame + PNG texture.
pub fn decode_frame(
    bytes: &[u8],
    stamp: NaiveDateTime,
    palette: &[String],
    bounds: &[f64],
) -> Result<(MosaicFrame, Vec<u8>, i64), String> {
    let raster = cog::decode_float_cog(bytes)?;
    validate_opera_georef(&raster)?;
    let pixels = classify(&raster.values, raster.nodata, bounds, palette.len());
    let png = sweep::png(raster.width, raster.height, &pixels)
        .map_err(|e| format!("encoding OPERA texture: {e}"))?;
    let scan_time = stamp.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let start_ms = stamp.and_utc().timestamp_millis();
    let frame = MosaicFrame {
        id: format!("{ID}-{}", stamp.format("%Y%m%dT%H%M%SZ")),
        product: "REF".into(),
        product_name: "Reflectivity".into(),
        units: "dBZ".into(),
        scan_time,
        sweep_end: None,
        status: FrameStatus::Complete,
        texture: String::new(),
        width: raster.width,
        height: raster.height,
        crs: Opera::crs(),
        geotransform: raster.geotransform,
        palette: palette.to_vec(),
        bounds: bounds.to_vec(),
    };
    Ok((frame, png, start_ms))
}

fn validate_opera_georef(raster: &DecodedRaster) -> Result<(), String> {
    if raster.geo_double_params.len() >= 4 {
        let (lat0, lon0, fe, fnorth) = (
            raster.geo_double_params[0],
            raster.geo_double_params[1],
            raster.geo_double_params[2],
            raster.geo_double_params[3],
        );
        if (lat0 - LAT0_DEG).abs() > 1e-6
            || (lon0 - LON0_DEG).abs() > 1e-6
            || (fe - FALSE_EASTING_M).abs() > 1e-3
            || (fnorth - FALSE_NORTHING_M).abs() > 1e-3
        {
            return Err(format!(
                "unexpected OPERA LAEA params lat0={lat0} lon0={lon0} fe={fe} fn={fnorth}"
            ));
        }
    }
    if (raster.geotransform[1] - 1000.0).abs() > 1e-6
        || (raster.geotransform[5] + 1000.0).abs() > 1e-6
    {
        return Err(format!(
            "unexpected OPERA pixel size {:?}",
            raster.geotransform
        ));
    }
    Ok(())
}

/// List today's COMP objects, falling back to yesterday when today is empty.
pub async fn list_recent<L, LF>(list_day: L) -> Result<Vec<CompObject>, String>
where
    L: Fn(NaiveDate) -> LF,
    LF: std::future::Future<Output = Result<Vec<CompObject>, String>>,
{
    let today = Utc::now().date_naive();
    let mut objects = list_day(today).await?;
    if objects.is_empty() {
        objects = list_day(today.pred_opt().unwrap_or(today)).await?;
    }
    Ok(objects)
}

/// Newest `limit` COMP keys that are not already `known`, oldest first so
/// the timeline fills in order like NEXRAD backfill.
pub fn backfill_targets(
    objects: &[CompObject],
    known: &HashSet<String>,
    limit: usize,
) -> Vec<CompObject> {
    let start = objects.len().saturating_sub(limit);
    objects[start..]
        .iter()
        .filter(|o| !known.contains(&o.key))
        .cloned()
        .collect()
}

/// One list+fetch cycle with injectable HTTP (tests mock the closures).
pub async fn refresh<L, G, LF, GF>(
    known: &HashSet<String>,
    list_day: L,
    get: G,
    palette: &[String],
    bounds: &[f64],
) -> Result<Option<(CompObject, MosaicFrame, Vec<u8>, i64)>, String>
where
    L: Fn(NaiveDate) -> LF,
    LF: std::future::Future<Output = Result<Vec<CompObject>, String>>,
    G: Fn(String) -> GF,
    GF: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    let objects = list_recent(list_day).await?;
    let Some(newest) = objects.last().cloned() else {
        return Ok(None);
    };
    if known.contains(&newest.key) {
        return Ok(None);
    }
    let bytes = get(newest.key.clone()).await?;
    let (frame, texture, start_ms) = decode_frame(&bytes, newest.stamp, palette, bounds)?;
    Ok(Some((newest, frame, texture, start_ms)))
}

/// Fetch up to `HISTORY_MAX` recent COMP frames the live poll has not
/// already delivered. Failures on one object skip it; a listing failure
/// ends the backfill.
async fn backfill_loop(
    events: Sender<Event>,
    known: HashSet<String>,
    palette: Vec<String>,
    bounds: Vec<f64>,
) {
    sleep(BACKFILL_DELAY).await;
    let objects = match list_recent(list_http).await {
        Ok(objects) => objects,
        Err(reason) => {
            eprintln!("{} OPERA backfill listing: {reason}", Utc::now().to_rfc3339());
            return;
        }
    };
    let targets = backfill_targets(&objects, &known, HISTORY_MAX);
    for obj in targets {
        let bytes = match get_http(obj.key.clone()).await {
            Ok(bytes) => bytes,
            Err(reason) => {
                eprintln!(
                    "{} OPERA backfill {}: {reason}",
                    Utc::now().to_rfc3339(),
                    obj.key
                );
                continue;
            }
        };
        let (frame, texture, start_ms) = match decode_frame(&bytes, obj.stamp, &palette, &bounds) {
            Ok(decoded) => decoded,
            Err(reason) => {
                eprintln!(
                    "{} OPERA backfill decode {}: {reason}",
                    Utc::now().to_rfc3339(),
                    obj.key
                );
                continue;
            }
        };
        if events
            .send(Event::Backfill {
                frame: Box::new(frame),
                texture,
                start_ms,
            })
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn list_http(day: NaiveDate) -> Result<Vec<CompObject>, String> {
    let prefix = prefix_for(day);
    let url = format!("{HOST}?list-type=2&prefix={prefix}&max-keys=1000");
    let response = live_index::http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("listing OPERA: {e}"))?
        .error_for_status()
        .map_err(|e| format!("listing OPERA: {e}"))?;
    let bytes = live_index::take_body(response, live_index::LISTING_MAX)
        .await
        .map_err(|e| format!("reading OPERA listing: {e}"))?;
    let body = String::from_utf8(bytes).map_err(|e| format!("reading OPERA listing: {e}"))?;
    parse_dbzh_listing(&body)
}

async fn get_http(key: String) -> Result<Vec<u8>, String> {
    let url = format!("{HOST}/{key}");
    let response = live_index::http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("fetching OPERA {key}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("fetching OPERA {key}: {e}"))?;
    live_index::take_body(response, BODY_MAX)
        .await
        .map_err(|e| format!("reading OPERA {key}: {e}"))
}

async fn poll_loop(
    events: Sender<Event>,
    mut known: HashSet<String>,
    palette: Vec<String>,
    bounds: Vec<f64>,
) {
    let first = tokio::time::timeout(
        START_TIMEOUT,
        refresh(&known, list_http, get_http, &palette, &bounds),
    )
    .await;
    match first {
        Ok(Ok(Some((obj, frame, texture, start_ms)))) => {
            known.insert(obj.key);
            let _ = events
                .send(Event::Frame {
                    frame: Box::new(frame),
                    texture,
                    start_ms,
                })
                .await;
        }
        Ok(Ok(None)) => {
            let _ = events
                .send(Event::Silent {
                    reason: "ORD cache has no OPERA DBZH.tiff yet".into(),
                })
                .await;
        }
        Ok(Err(reason)) => {
            let _ = events.send(Event::Offline { reason }).await;
        }
        Err(_) => {
            let _ = events
                .send(Event::Offline {
                    reason: "OPERA fetch timed out".into(),
                })
                .await;
        }
    }
    // Same shape as NEXRAD: newest is already on screen; pull the rest of
    // the dozen in the background so play/[ ] have a loop.
    {
        let events = events.clone();
        let known = known.clone();
        let palette = palette.clone();
        let bounds = bounds.clone();
        tokio::spawn(async move {
            backfill_loop(events, known, palette, bounds).await;
        });
    }
    loop {
        sleep(IDLE).await;
        match refresh(&known, list_http, get_http, &palette, &bounds).await {
            Ok(Some((obj, frame, texture, start_ms))) => {
                known.insert(obj.key.clone());
                if known.len() > HISTORY_MAX * 4 {
                    known = HashSet::from([obj.key]);
                }
                if events
                    .send(Event::Frame {
                        frame: Box::new(frame),
                        texture,
                        start_ms,
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Ok(None) => {}
            Err(reason) => {
                if events.send(Event::Offline { reason }).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Tiny synthetic COMP-shaped COG for decoder / fetch tests (not live data).
#[cfg(test)]
pub fn synthetic_fixture_cog() -> Vec<u8> {
    let width = 32u32;
    let height = 32u32;
    let mut band0 = vec![f32::NAN; (width * height) as usize];
    let band1 = vec![FILL; band0.len()];
    // A few measured cells and one nodata.
    band0[0] = 5.0;
    band0[1] = FILL;
    band0[2] = 35.0;
    band0[16 * 32 + 16] = 45.0;
    let gt = [-500.0, 1000.0, 0.0, 500.0, 0.0, -1000.0];
    let doubles = [
        LAT0_DEG,
        LON0_DEG,
        FALSE_EASTING_M,
        FALSE_NORTHING_M,
        298.257223563,
        6_378_137.0,
    ];
    cog::write_float_cog(width, height, 16, 16, gt, &doubles, FILL, &band0, &band1)
        .expect("synthetic OPERA fixture")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AdapterTarget, GeoPoint, Selection};
    use crate::source::{Candidate, SourceRegistry, covering_selection};

    #[test]
    fn listing_keeps_only_dbzh_tiffs() {
        let body = r#"<ListBucketResult>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1200@0@DBZH.h5</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1200@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1205@0@RATE.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1210@0@DBZH.tiff</Key></Contents>
            </ListBucketResult>"#;
        let objects = parse_dbzh_listing(body).unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(
            objects[0].key,
            "2026/09/17/OPERA/COMP/OPERA@20260917T1200@0@DBZH.tiff"
        );
        assert_eq!(
            objects[1].stamp,
            NaiveDate::from_ymd_opt(2026, 9, 17)
                .unwrap()
                .and_hms_opt(12, 10, 0)
                .unwrap()
        );
    }

    #[test]
    fn decoder_classifies_synthetic_fixture() {
        let bytes = synthetic_fixture_cog();
        let stamp = NaiveDate::from_ymd_opt(2026, 9, 17)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let opera = Opera::new();
        let (frame, png, start_ms) =
            decode_frame(&bytes, stamp, &opera.palette, &opera.bounds).unwrap();
        assert_eq!(frame.width, 32);
        assert_eq!(frame.height, 32);
        assert!(matches!(
            frame.crs,
            Crs::LambertAzimuthalEqualArea { lat0_deg: 55.0, .. }
        ));
        assert_eq!(frame.product_name, "Reflectivity");
        assert_eq!(frame.units, "dBZ");
        assert_eq!(start_ms, stamp.and_utc().timestamp_millis());
        let info = png::Decoder::new(std::io::Cursor::new(&png))
            .read_info()
            .unwrap()
            .info()
            .clone();
        assert_eq!((info.width, info.height), (32, 32));
        let pixels = classify(
            &cog::decode_float_cog(&bytes).unwrap().values,
            Some(FILL),
            &opera.bounds,
            opera.palette.len(),
        );
        assert_eq!(pixels[1], 0); // measured G
        assert!(pixels[0] >= 1);
        assert_eq!(pixels[4], 0);
        assert_eq!(pixels[5], 1); // nodata/missing
        assert_eq!(pixels[12], 0);
        assert_eq!(pixels[13], 2); // undetect nan
    }

    #[test]
    fn refresh_fetches_newest_unseen_with_mocked_http() {
        let fixture = synthetic_fixture_cog();
        let opera = Opera::new();
        let listing = r#"<ListBucketResult>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1200@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1205@0@DBZH.tiff</Key></Contents>
            </ListBucketResult>"#;
        let known = HashSet::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let got = runtime
            .block_on(refresh(
                &known,
                |_day| async { parse_dbzh_listing(listing) },
                |key| {
                    let fixture = fixture.clone();
                    async move {
                        assert!(key.ends_with("OPERA@20260917T1205@0@DBZH.tiff"));
                        Ok(fixture)
                    }
                },
                &opera.palette,
                &opera.bounds,
            ))
            .unwrap()
            .unwrap();
        assert!(got.0.key.ends_with("1205@0@DBZH.tiff"));
        assert_eq!(got.1.width, 32);
        let mut known = HashSet::new();
        known.insert(got.0.key.clone());
        let again = runtime
            .block_on(refresh(
                &known,
                |_day| async { parse_dbzh_listing(listing) },
                |_key| async { panic!("should not refetch") },
                &opera.palette,
                &opera.bounds,
            ))
            .unwrap();
        assert!(again.is_none());
    }

    #[test]
    fn backfill_skips_known_and_keeps_a_dozen() {
        let listing = r#"<ListBucketResult>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1100@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1105@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1110@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1115@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1120@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1125@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1130@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1135@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1140@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1145@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1150@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1155@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1200@0@DBZH.tiff</Key></Contents>
            <Contents><Key>2026/09/17/OPERA/COMP/OPERA@20260917T1205@0@DBZH.tiff</Key></Contents>
            </ListBucketResult>"#;
        let objects = parse_dbzh_listing(listing).unwrap();
        assert_eq!(objects.len(), 14);
        let mut known = HashSet::new();
        known.insert("2026/09/17/OPERA/COMP/OPERA@20260917T1205@0@DBZH.tiff".into());
        let targets = backfill_targets(&objects, &known, HISTORY_MAX);
        assert_eq!(targets.len(), 11);
        assert!(targets[0].key.ends_with("1110@0@DBZH.tiff"));
        assert!(targets[10].key.ends_with("1200@0@DBZH.tiff"));
        assert!(
            !targets
                .iter()
                .any(|o| o.key.contains("1100") || o.key.contains("1105"))
        );
    }

    #[test]
    fn opera_covers_london_and_loses_to_polar() {
        let registry = SourceRegistry::compiled();
        let london = GeoPoint {
            lat: 51.5,
            lon: -0.1,
        };
        let picked = registry.covering_selection(london, None).unwrap();
        assert_eq!(picked.source_id, ID);
        assert_eq!(picked.target, AdapterTarget::Mosaic);

        let ktlx = GeoPoint {
            lat: 35.333,
            lon: -97.278,
        };
        let picked = registry.covering_selection(ktlx, None).unwrap();
        assert_eq!(picked.source_id, "nexrad");
    }

    #[test]
    fn national_priority_preempts_opera_when_both_cover() {
        let opera = Candidate {
            selection: Selection {
                source_id: ID.into(),
                target: AdapterTarget::Mosaic,
            },
            family: Family::Grid,
            product: ProductClass::Reflectivity,
            priority: SELECTION_PRIORITY,
            coverage: Coverage::Box {
                north: COVERAGE_NORTH,
                south: COVERAGE_SOUTH,
                east: COVERAGE_EAST,
                west: COVERAGE_WEST,
            },
            dish: None,
        };
        let national = Candidate {
            selection: Selection {
                source_id: "italy-dpc".into(),
                target: AdapterTarget::Mosaic,
            },
            family: Family::Grid,
            product: ProductClass::Reflectivity,
            priority: 50,
            coverage: Coverage::Box {
                north: 47.0,
                south: 36.0,
                east: 19.0,
                west: 6.0,
            },
            dish: None,
        };
        let rome = GeoPoint {
            lat: 41.9,
            lon: 12.5,
        };
        assert_eq!(
            covering_selection(rome, None, &[opera.clone(), national.clone()]).map(|s| s.source_id),
            Some("italy-dpc".into())
        );
        let held_opera = Selection {
            source_id: ID.into(),
            target: AdapterTarget::Mosaic,
        };
        assert_eq!(
            covering_selection(rome, Some(&held_opera), &[opera, national]).map(|s| s.source_id),
            Some("italy-dpc".into()),
            "higher priority national preempts held OPERA"
        );
    }

    #[test]
    fn attribution_and_hello_metadata() {
        let opera = Opera::new();
        let meta = opera.metadata();
        assert_eq!(meta.attribution, ATTRIBUTION);
        assert_eq!(meta.name, NAME);
        assert_eq!(meta.default_product_class, ProductClass::Reflectivity);
    }
}
