//! Airport METARs for the selected radar (DESIGN.md, observations).
//!
//! `metar_query` is a reply to its sender, like `search_places`. The engine
//! fetches NOAA/NWS Aviation Weather Center JSON, keeps the raw observation
//! string, and names FAA flight category only so the UI can color ICAO chips.
//! It does not decode English. Nothing is fetched until a client asks.
//! Coverage is the NEXRAD envelope (US plus Canada, which AWC serves). A
//! radar outside that envelope, including OPERA Europe, is a no-op: empty
//! `metars`, no fetch. Default pick is the nearest stations around the radar.
//! `pick=priority` ranks AWC stationinfo `priority` (lower is a hub) inside
//! the view bbox. `OMASTORM_METAR_URL` / `OMASTORM_METAR_FIXTURE` and
//! `OMASTORM_STATIONS_URL` / `OMASTORM_STATIONS_FIXTURE` override the live
//! feeds for checks (no network).

use crate::protocol::MetarReport;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

const DEFAULT_URL: &str = "https://aviationweather.gov/api/data/metar";
const DEFAULT_STATIONS_URL: &str = "https://aviationweather.gov/api/data/stationinfo";
const USER_AGENT: &str = concat!(
    "omastorm/",
    env!("CARGO_PKG_VERSION"),
    " (https://omastorm.com)"
);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY: usize = 1 << 20;
/// How far around the radar a station may sit when no view bbox is sent.
const RADIUS_KM: f64 = 250.0;
pub const LIMIT: usize = 16;
const MISSING_PRIORITY: u8 = 99;
const TTL: Duration = Duration::from_secs(600);
const STATIONS_TTL: Duration = Duration::from_secs(24 * 3600);
const EARTH_KM: f64 = 6371.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Nearest,
    Priority,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BBox {
    pub south: f64,
    pub west: f64,
    pub north: f64,
    pub east: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Query {
    pub lat: f64,
    pub lon: f64,
    pub bbox: Option<BBox>,
    pub pick: Pick,
    pub limit: usize,
    pub always_on: Vec<String>,
}

pub struct Service {
    client: reqwest::Client,
    url: String,
    stations_url: String,
    fixture: Option<PathBuf>,
    stations_fixture: Option<PathBuf>,
    cache: Mutex<HashMap<CacheKey, Cached>>,
    stations: Mutex<HashMap<StationsKey, CachedStations>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    lat: i32,
    lon: i32,
    south: i32,
    west: i32,
    north: i32,
    east: i32,
    pick: u8,
    limit: u8,
    always: String,
    hour: i64,
}

struct Cached {
    at: Instant,
    reports: Vec<MetarReport>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct StationsKey {
    south: i32,
    west: i32,
    north: i32,
    east: i32,
    day: i64,
}

struct CachedStations {
    at: Instant,
    by_id: HashMap<String, u8>,
}

#[derive(Deserialize)]
struct AwcMetar {
    #[serde(rename = "icaoId")]
    icao_id: String,
    #[serde(rename = "rawOb", default)]
    raw_ob: String,
    #[serde(rename = "fltCat", default)]
    flt_cat: String,
    lat: f64,
    lon: f64,
    #[serde(rename = "obsTime", default)]
    obs_time: Option<f64>,
    #[serde(default)]
    visib: serde_json::Value,
    #[serde(default)]
    clouds: Vec<AwcCloud>,
    #[serde(rename = "vertVis", default)]
    vert_vis: Option<f64>,
}

#[derive(Deserialize, Default)]
struct AwcCloud {
    #[serde(default)]
    cover: String,
    #[serde(default)]
    base: Option<f64>,
}

#[derive(Deserialize)]
struct AwcStation {
    #[serde(rename = "icaoId")]
    icao_id: String,
    #[serde(default)]
    priority: serde_json::Value,
}

impl Query {
    #[allow(clippy::too_many_arguments)]
    pub fn parse(
        lat: f64,
        lon: f64,
        south: Option<f64>,
        west: Option<f64>,
        north: Option<f64>,
        east: Option<f64>,
        pick: Option<String>,
        limit: Option<u32>,
        always_on: Vec<String>,
    ) -> Result<Query, String> {
        if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
            return Err("metar_query needs lat in [-90, 90] and lon in [-180, 180].".into());
        }
        let pick = match pick.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("nearest") => Pick::Nearest,
            Some("priority") => Pick::Priority,
            Some(_) => return Err("metar_query pick must be nearest or priority.".into()),
        };
        let limit = match limit {
            None => LIMIT,
            Some(n) if (1..=LIMIT as u32).contains(&n) => n as usize,
            Some(_) => return Err("metar_query limit must be 1 through 16.".into()),
        };
        let bbox = match (south, west, north, east) {
            (None, None, None, None) => None,
            (Some(south), Some(west), Some(north), Some(east)) => {
                if !(-90.0..=90.0).contains(&south)
                    || !(-90.0..=90.0).contains(&north)
                    || !(-180.0..=180.0).contains(&west)
                    || !(-180.0..=180.0).contains(&east)
                    || south > north
                    || west > east
                {
                    return Err("metar_query needs south, west, north, east as a view box.".into());
                }
                Some(BBox {
                    south,
                    west,
                    north,
                    east,
                })
            }
            _ => {
                return Err("metar_query needs south, west, north, east together.".into());
            }
        };
        if pick == Pick::Priority && bbox.is_none() {
            return Err("metar_query pick=priority needs the view box.".into());
        }
        let mut seen = HashSet::new();
        let mut pinned = Vec::new();
        for raw in always_on {
            let id = raw.trim().to_uppercase();
            if id.is_empty() {
                continue;
            }
            if !id.chars().all(|c| c.is_ascii_alphanumeric()) || !(3..=4).contains(&id.len()) {
                return Err("metar_query always_on ids must be ICAO station ids.".into());
            }
            if pinned.len() >= limit {
                break;
            }
            if seen.insert(id.clone()) {
                pinned.push(id);
            }
        }
        Ok(Query {
            lat,
            lon,
            bbox,
            pick,
            limit,
            always_on: pinned,
        })
    }
}

impl Service {
    pub fn open() -> io::Result<Service> {
        let url = env::var("OMASTORM_METAR_URL").unwrap_or_else(|_| DEFAULT_URL.into());
        let stations_url =
            env::var("OMASTORM_STATIONS_URL").unwrap_or_else(|_| DEFAULT_STATIONS_URL.into());
        let fixture = env::var_os("OMASTORM_METAR_FIXTURE").map(PathBuf::from);
        let stations_fixture = env::var_os("OMASTORM_STATIONS_FIXTURE").map(PathBuf::from);
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(io::Error::other)?;
        Ok(Service {
            client,
            url,
            stations_url,
            fixture,
            stations_fixture,
            cache: Mutex::new(HashMap::new()),
            stations: Mutex::new(HashMap::new()),
        })
    }

    pub async fn query(&self, q: Query) -> Result<Vec<MetarReport>, String> {
        if !crate::envelope::nexrad_network(q.lon, q.lat) {
            return Ok(Vec::new());
        }
        let key = cache_key(&q);
        if let Some(hit) = self.cached(&key) {
            return Ok(hit);
        }
        let priorities = if q.pick == Pick::Priority {
            self.priorities(&q).await.unwrap_or_default()
        } else {
            HashMap::new()
        };
        let body = self.load_metars(&q).await?;
        let reports = select(parse_body(&body)?, &q, &priorities);
        self.cache.lock().unwrap().insert(
            key,
            Cached {
                at: Instant::now(),
                reports: reports.clone(),
            },
        );
        Ok(reports)
    }

    fn cached(&self, key: &CacheKey) -> Option<Vec<MetarReport>> {
        let cache = self.cache.lock().unwrap();
        let entry = cache.get(key)?;
        if entry.at.elapsed() >= TTL {
            return None;
        }
        Some(entry.reports.clone())
    }

    async fn load_metars(&self, q: &Query) -> Result<Vec<u8>, String> {
        if let Some(path) = &self.fixture {
            return fs::read(path).map_err(|e| format!("metar fixture: {e}"));
        }
        let box_ = q
            .bbox
            .unwrap_or_else(|| radius_bbox(q.lat, q.lon, RADIUS_KM));
        take_body(
            self.client
                .get(bbox_url(&self.url, box_, true))
                .send()
                .await
                .map_err(|e| e.to_string())?,
        )
        .await
    }

    async fn priorities(&self, q: &Query) -> Result<HashMap<String, u8>, String> {
        let box_ = q
            .bbox
            .unwrap_or_else(|| radius_bbox(q.lat, q.lon, RADIUS_KM));
        let key = StationsKey {
            south: (box_.south * 10.0).round() as i32,
            west: (box_.west * 10.0).round() as i32,
            north: (box_.north * 10.0).round() as i32,
            east: (box_.east * 10.0).round() as i32,
            day: Utc::now().timestamp().div_euclid(86400),
        };
        if let Some(hit) = self.cached_stations(&key) {
            return Ok(hit);
        }
        let body = self.load_stations(box_).await?;
        let by_id = parse_stations(&body)?;
        self.stations.lock().unwrap().insert(
            key,
            CachedStations {
                at: Instant::now(),
                by_id: by_id.clone(),
            },
        );
        Ok(by_id)
    }

    fn cached_stations(&self, key: &StationsKey) -> Option<HashMap<String, u8>> {
        let cache = self.stations.lock().unwrap();
        let entry = cache.get(key)?;
        if entry.at.elapsed() >= STATIONS_TTL {
            return None;
        }
        Some(entry.by_id.clone())
    }

    async fn load_stations(&self, box_: BBox) -> Result<Vec<u8>, String> {
        if let Some(path) = &self.stations_fixture {
            return fs::read(path).map_err(|e| format!("stations fixture: {e}"));
        }
        take_body(
            self.client
                .get(bbox_url(&self.stations_url, box_, false))
                .send()
                .await
                .map_err(|e| e.to_string())?,
        )
        .await
    }
}

async fn take_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_BODY as u64)
    {
        return Err("body over the size limit".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if chunk.len() > MAX_BODY - bytes.len() {
            return Err("body over the size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_body(bytes: &[u8]) -> Result<Vec<MetarReport>, String> {
    let rows: Vec<AwcMetar> = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mut by_id: HashMap<String, MetarReport> = HashMap::new();
    for row in rows {
        let id = row.icao_id.trim().to_uppercase();
        if id.is_empty() || row.raw_ob.trim().is_empty() {
            continue;
        }
        if !(-90.0..=90.0).contains(&row.lat) || !(-180.0..=180.0).contains(&row.lon) {
            continue;
        }
        let obs = row.obs_time.unwrap_or(0.0) as i64;
        let report = MetarReport {
            id: id.clone(),
            lat: row.lat,
            lon: row.lon,
            category: category(&row),
            raw: row.raw_ob.trim().to_string(),
            obs_time: format_obs(obs),
        };
        match by_id.get(&id) {
            Some(prev) if prev.obs_time >= report.obs_time => {}
            _ => {
                by_id.insert(id, report);
            }
        }
    }
    Ok(by_id.into_values().collect())
}

/// AWC METAR is worldwide. This overlay keeps US and Canadian stations:
/// ICAO `K` (CONUS), `C` (Canada), `P` (Alaska/Hawaii/Guam and other US
/// Pacific), `TJ`/`TI` (Puerto Rico / US Virgin Islands).
fn awc_na_station(id: &str) -> bool {
    let b = id.as_bytes();
    match b.first() {
        Some(b'K' | b'C') => true,
        Some(b'P') => true,
        Some(b'T') => b.len() >= 2 && (b[1] == b'J' || b[1] == b'I'),
        _ => false,
    }
}

fn select(
    mut reports: Vec<MetarReport>,
    q: &Query,
    priorities: &HashMap<String, u8>,
) -> Vec<MetarReport> {
    reports.retain(|r| awc_na_station(&r.id));
    if let Some(box_) = q.bbox {
        reports.retain(|r| in_bbox(r.lat, r.lon, box_));
    }
    let mut by_id: HashMap<String, MetarReport> =
        reports.into_iter().map(|r| (r.id.clone(), r)).collect();
    let mut chosen = Vec::new();
    for id in &q.always_on {
        if let Some(report) = by_id.remove(id) {
            chosen.push(report);
            if chosen.len() >= q.limit {
                return chosen;
            }
        }
    }
    let mut rest: Vec<MetarReport> = by_id.into_values().collect();
    rest.sort_by(|a, b| {
        let pri = if q.pick == Pick::Priority {
            let pa = *priorities.get(&a.id).unwrap_or(&MISSING_PRIORITY);
            let pb = *priorities.get(&b.id).unwrap_or(&MISSING_PRIORITY);
            pa.cmp(&pb)
        } else {
            std::cmp::Ordering::Equal
        };
        pri.then(
            great_circle_km(q.lat, q.lon, a.lat, a.lon)
                .total_cmp(&great_circle_km(q.lat, q.lon, b.lat, b.lon)),
        )
        .then(a.id.cmp(&b.id))
    });
    for report in rest {
        chosen.push(report);
        if chosen.len() >= q.limit {
            break;
        }
    }
    chosen
}

fn in_bbox(lat: f64, lon: f64, box_: BBox) -> bool {
    lat >= box_.south && lat <= box_.north && lon >= box_.west && lon <= box_.east
}

fn cache_key(q: &Query) -> CacheKey {
    let hour = Utc::now().timestamp().div_euclid(3600);
    let (south, west, north, east) = match q.bbox {
        Some(b) => (
            (b.south * 10.0).round() as i32,
            (b.west * 10.0).round() as i32,
            (b.north * 10.0).round() as i32,
            (b.east * 10.0).round() as i32,
        ),
        None => (0, 0, 0, 0),
    };
    CacheKey {
        lat: (q.lat * 10.0).round() as i32,
        lon: (q.lon * 10.0).round() as i32,
        south,
        west,
        north,
        east,
        pick: match q.pick {
            Pick::Nearest => 0,
            Pick::Priority => 1,
        },
        limit: q.limit as u8,
        always: q.always_on.join(" "),
        hour,
    }
}

fn bbox_url(base: &str, box_: BBox, hours: bool) -> String {
    let join = if base.contains('?') { '&' } else { '?' };
    let hours = if hours { "&hours=1" } else { "" };
    format!(
        "{base}{join}bbox={:.4},{:.4},{:.4},{:.4}&format=json{hours}",
        box_.south, box_.west, box_.north, box_.east
    )
}

fn parse_stations(bytes: &[u8]) -> Result<HashMap<String, u8>, String> {
    let rows: Vec<AwcStation> = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let mut by_id = HashMap::new();
    for row in rows {
        let id = row.icao_id.trim().to_uppercase();
        if id.is_empty() {
            continue;
        }
        if let Some(priority) = parse_priority(&row.priority) {
            by_id.insert(id, priority);
        }
    }
    Ok(by_id)
}

fn parse_priority(value: &serde_json::Value) -> Option<u8> {
    match value {
        serde_json::Value::Number(n) => n.as_u64().and_then(|n| u8::try_from(n).ok()),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn radius_bbox(lat: f64, lon: f64, km: f64) -> BBox {
    let (south, west, north, east) = bbox(lat, lon, km);
    BBox {
        south,
        west,
        north,
        east,
    }
}

fn category(row: &AwcMetar) -> String {
    let named = row.flt_cat.trim().to_lowercase();
    if matches!(named.as_str(), "vfr" | "mvfr" | "ifr" | "lifr") {
        return named;
    }
    from_vis_ceiling(parse_vis_sm(&row.visib), ceiling_ft(row))
}

/// FAA flight category from visibility (statute miles) and ceiling (feet).
fn from_vis_ceiling(vis: Option<f64>, ceiling: Option<f64>) -> String {
    let vis = vis.unwrap_or(f64::INFINITY);
    let ceil = ceiling.unwrap_or(f64::INFINITY);
    if ceil < 500.0 || vis < 1.0 {
        "lifr".into()
    } else if ceil < 1000.0 || vis < 3.0 {
        "ifr".into()
    } else if ceil <= 3000.0 || vis <= 5.0 {
        "mvfr".into()
    } else {
        "vfr".into()
    }
}

fn parse_vis_sm(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => {
            let t = s.trim().trim_end_matches('+');
            if let Ok(n) = t.parse::<f64>() {
                return Some(n);
            }
            if let Some((a, b)) = t.split_once('/') {
                let n = a.parse::<f64>().ok()?;
                let d = b.parse::<f64>().ok()?;
                if d != 0.0 {
                    return Some(n / d);
                }
            }
            None
        }
        _ => None,
    }
}

fn ceiling_ft(row: &AwcMetar) -> Option<f64> {
    if let Some(vv) = row.vert_vis.filter(|v| *v > 0.0) {
        return Some(vv);
    }
    row.clouds
        .iter()
        .filter(|c| {
            matches!(
                c.cover.to_uppercase().as_str(),
                "BKN" | "OVC" | "VV" | "OVX"
            )
        })
        .filter_map(|c| c.base)
        .min_by(|a, b| a.total_cmp(b))
}

fn format_obs(secs: i64) -> String {
    DateTime::from_timestamp(secs, 0)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn bbox(lat: f64, lon: f64, km: f64) -> (f64, f64, f64, f64) {
    let dlat = km / 111.0;
    let dlon = km / (111.0 * lat.to_radians().cos().abs().max(0.2));
    (
        (lat - dlat).clamp(-90.0, 90.0),
        (lon - dlon).clamp(-180.0, 180.0),
        (lat + dlat).clamp(-90.0, 90.0),
        (lon + dlon).clamp(-180.0, 180.0),
    )
}

fn great_circle_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let (dp, dl) = ((lat2 - lat1).to_radians(), (lon2 - lon1).to_radians());
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_KM * h.clamp(0.0, 1.0).sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MetarReport;
    use std::collections::HashMap;

    const KTLX: (f64, f64) = (35.33306, -97.27748);
    const FIXTURE: &str = include_str!("../data/metar-ktlx.json");
    const STATIONS: &str = include_str!("../data/stations-ktlx.json");

    fn nearest_query(limit: usize) -> Query {
        Query {
            lat: KTLX.0,
            lon: KTLX.1,
            bbox: None,
            pick: Pick::Nearest,
            limit,
            always_on: vec![],
        }
    }

    fn priorities() -> HashMap<String, u8> {
        parse_stations(STATIONS.as_bytes()).unwrap()
    }

    #[test]
    fn fixture_ranks_sixteen_nearest_to_ktlx() {
        let reports = select(
            parse_body(FIXTURE.as_bytes()).unwrap(),
            &nearest_query(LIMIT),
            &HashMap::new(),
        );
        assert_eq!(reports.len(), LIMIT);
        assert_eq!(reports[0].id, "KTIK");
        assert!(reports.iter().any(|r| r.id == "KOKC"));
        assert!(reports.iter().all(|r| !r.raw.is_empty()));
        let far = great_circle_km(
            KTLX.0,
            KTLX.1,
            reports.last().unwrap().lat,
            reports.last().unwrap().lon,
        );
        let near = great_circle_km(KTLX.0, KTLX.1, reports[0].lat, reports[0].lon);
        assert!(near <= far);
    }

    #[test]
    fn count_shrinks_the_pool() {
        let reports = select(
            parse_body(FIXTURE.as_bytes()).unwrap(),
            &nearest_query(4),
            &HashMap::new(),
        );
        assert_eq!(reports.len(), 4);
        assert_eq!(reports[0].id, "KTIK");
    }

    #[test]
    fn priority_prefers_hubs_over_near_fields() {
        let q = Query {
            lat: KTLX.0,
            lon: KTLX.1,
            bbox: Some(BBox {
                south: 34.0,
                west: -99.0,
                north: 37.0,
                east: -95.0,
            }),
            pick: Pick::Priority,
            limit: 16,
            always_on: vec![],
        };
        let reports = select(parse_body(FIXTURE.as_bytes()).unwrap(), &q, &priorities());
        assert_eq!(reports[0].id, "KOKC");
        let tik = reports.iter().position(|r| r.id == "KTIK").unwrap();
        let okc = reports.iter().position(|r| r.id == "KOKC").unwrap();
        assert!(okc < tik);
    }

    #[test]
    fn always_on_pins_a_home_field_in_view() {
        let q = Query {
            lat: KTLX.0,
            lon: KTLX.1,
            bbox: Some(BBox {
                south: 34.0,
                west: -99.0,
                north: 37.0,
                east: -95.0,
            }),
            pick: Pick::Priority,
            limit: 4,
            always_on: vec!["KOUN".into()],
        };
        let reports = select(parse_body(FIXTURE.as_bytes()).unwrap(), &q, &priorities());
        assert_eq!(reports.len(), 4);
        assert_eq!(reports[0].id, "KOUN");
        assert_eq!(reports[1].id, "KOKC");
    }

    fn report(id: &str, lat: f64, lon: f64) -> MetarReport {
        MetarReport {
            id: id.into(),
            lat,
            lon,
            category: "vfr".into(),
            raw: format!("{id} TEST"),
            obs_time: String::new(),
        }
    }

    #[test]
    fn kbna_in_view_drops_km19_when_priority_fills() {
        // KNQA view that reaches Nashville (KOHX is the radar landmark).
        // KBNA is priority 1; KM19 is 5. Sixteen better-ranked stations
        // in that box crowd KM19 out.
        let knqa = (35.3566, -89.8704);
        let mut reports = vec![
            report("KBNA", 36.1245, -86.6782),
            report("KM19", 35.6377, -91.1764),
            report("KMEM", 35.0424, -89.9767),
        ];
        let mut pri = HashMap::from([("KBNA".into(), 1u8), ("KMEM".into(), 2), ("KM19".into(), 5)]);
        for i in 0..14 {
            let id = format!("K{i:02}X");
            reports.push(report(&id, 35.4, -89.9 - i as f64 * 0.05));
            pri.insert(id, 4);
        }
        let q = Query {
            lat: knqa.0,
            lon: knqa.1,
            bbox: Some(BBox {
                south: 34.5,
                west: -92.5,
                north: 36.5,
                east: -86.4,
            }),
            pick: Pick::Priority,
            limit: 16,
            always_on: vec![],
        };
        let chosen = select(reports, &q, &pri);
        assert_eq!(chosen.len(), 16);
        assert!(chosen.iter().any(|r| r.id == "KBNA"));
        assert!(chosen.iter().any(|r| r.id == "KMEM"));
        assert!(chosen.iter().all(|r| r.id != "KM19"));
    }

    #[test]
    fn bbox_drops_stations_off_screen() {
        let q = Query {
            lat: KTLX.0,
            lon: KTLX.1,
            bbox: Some(BBox {
                south: 35.3,
                west: -97.5,
                north: 35.5,
                east: -97.2,
            }),
            pick: Pick::Nearest,
            limit: 16,
            always_on: vec![],
        };
        let reports = select(parse_body(FIXTURE.as_bytes()).unwrap(), &q, &HashMap::new());
        assert!(reports.iter().any(|r| r.id == "KTIK"));
        assert!(reports.iter().all(|r| r.id != "KTUL"));
    }

    #[test]
    fn parse_query_rejects_bad_pick_and_limit() {
        assert!(
            Query::parse(
                KTLX.0,
                KTLX.1,
                None,
                None,
                None,
                None,
                Some("hubs".into()),
                None,
                vec![]
            )
            .is_err()
        );
        assert!(
            Query::parse(
                KTLX.0,
                KTLX.1,
                None,
                None,
                None,
                None,
                None,
                Some(32),
                vec![]
            )
            .is_err()
        );
        assert!(
            Query::parse(
                KTLX.0,
                KTLX.1,
                None,
                None,
                None,
                None,
                Some("priority".into()),
                None,
                vec![]
            )
            .is_err()
        );
    }

    #[test]
    fn categories_follow_awc_then_faa_vis_ceiling() {
        let reports = parse_body(FIXTURE.as_bytes()).unwrap();
        let cat = |id: &str| {
            reports
                .iter()
                .find(|r| r.id == id)
                .unwrap()
                .category
                .as_str()
        };
        assert_eq!(cat("KOKC"), "vfr");
        assert_eq!(cat("KPWA"), "mvfr");
        assert_eq!(cat("KTIK"), "ifr");
        assert_eq!(cat("KADM"), "lifr");
        assert_eq!(cat("KSRE"), "vfr"); // no fltCat; SCT 3500 / 10 SM
    }

    #[test]
    fn vis_fractions_and_plus_parse() {
        assert_eq!(parse_vis_sm(&serde_json::json!("10+")), Some(10.0));
        assert_eq!(parse_vis_sm(&serde_json::json!("1/2")), Some(0.5));
        assert_eq!(parse_vis_sm(&serde_json::json!(4)), Some(4.0));
    }

    #[test]
    fn faa_breakpoints() {
        assert_eq!(from_vis_ceiling(Some(0.5), Some(8000.0)), "lifr");
        assert_eq!(from_vis_ceiling(Some(10.0), Some(400.0)), "lifr");
        assert_eq!(from_vis_ceiling(Some(2.0), Some(8000.0)), "ifr");
        assert_eq!(from_vis_ceiling(Some(10.0), Some(800.0)), "ifr");
        assert_eq!(from_vis_ceiling(Some(4.0), Some(8000.0)), "mvfr");
        assert_eq!(from_vis_ceiling(Some(10.0), Some(2000.0)), "mvfr");
        assert_eq!(from_vis_ceiling(Some(10.0), Some(3500.0)), "vfr");
    }

    #[test]
    fn coverage_is_nexrad_envelope_not_opera() {
        assert!(crate::envelope::nexrad_network(KTLX.1, KTLX.0));
        assert!(crate::envelope::nexrad_network(-79.629, 43.679)); // CYYZ
        assert!(!crate::envelope::nexrad_network(-0.1, 51.5)); // London OPERA
    }

    #[test]
    fn select_keeps_us_and_canada_drops_europe() {
        let q = nearest_query(16);
        let reports = vec![
            report("KTIK", 35.4147, -97.3867),
            report("CYYZ", 43.679, -79.629),
            report("EGLL", 51.477, -0.461),
            report("LFPG", 49.015, 2.534),
            report("MMMX", 19.436, -99.072),
        ];
        let chosen = select(reports, &q, &HashMap::new());
        let ids: Vec<_> = chosen.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"KTIK"));
        assert!(ids.contains(&"CYYZ"));
        assert!(!ids.contains(&"EGLL"));
        assert!(!ids.contains(&"LFPG"));
        assert!(!ids.contains(&"MMMX"));
    }

    #[test]
    fn duplicate_ids_keep_the_newer_observation() {
        let json = br#"[
            {"icaoId":"KOKC","rawOb":"KOKC OLD","lat":35.4,"lon":-97.6,"obsTime":100,"fltCat":"VFR"},
            {"icaoId":"KOKC","rawOb":"KOKC NEW","lat":35.4,"lon":-97.6,"obsTime":200,"fltCat":"IFR"}
        ]"#;
        let reports = parse_body(json).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].raw, "KOKC NEW");
        assert_eq!(reports[0].category, "ifr");
    }
}
