#[macro_use]
extern crate anyhow;
#[macro_use]
extern crate log;

mod classify;
mod crossings;
mod edits;
mod geometry;
mod scrape;

use std::collections::HashMap;
use std::sync::Once;

use anyhow::Result;
use geo::{Coord, Euclidean, Length, LineString, Point};
use geojson::GeoJson;
use osm_reader::{NodeID, WayID};
use serde::Serialize;
use utils::{Mercator, Tags};
use wasm_bindgen::prelude::*;

use crate::classify::{Kind, Quickfix};
use crate::edits::{Edits, UserCmd};

static START: Once = Once::new();

#[wasm_bindgen]
pub struct Speedwalk {
    original_nodes: HashMap<NodeID, Node>,
    original_ways: HashMap<WayID, Way>,
    mercator: Mercator,

    edits: Option<Edits>,

    derived_nodes: HashMap<NodeID, Node>,
    derived_ways: HashMap<WayID, Way>,
}

#[derive(Clone)]
pub struct Node {
    pub pt: Coord,
    pub tags: Tags,
    pub version: i32,

    pub way_ids: Vec<WayID>,
}

#[derive(Clone)]
pub struct Way {
    pub node_ids: Vec<NodeID>,
    pub linestring: LineString,
    pub tags: Tags,
    pub version: i32,

    // TODO Manage derived state better
    pub kind: Kind,
    pub num_crossings: usize,
    pub is_main_road: bool,
    // From the start of the line. first entry is 0, last entry is linestring length
    pub distance_per_node: Vec<f64>,
}

#[wasm_bindgen]
impl Speedwalk {
    #[wasm_bindgen(constructor)]
    pub fn new(input_bytes: &[u8]) -> Result<Speedwalk, JsValue> {
        // Panics shouldn't happen, but if they do, console.log them.
        console_error_panic_hook::set_once();
        START.call_once(|| {
            console_log::init_with_level(log::Level::Info).unwrap();
        });

        scrape::scrape_osm(input_bytes).map_err(err_to_js)
    }

    #[wasm_bindgen(js_name = getNodes)]
    pub fn get_nodes(&self) -> Result<String, JsValue> {
        let mut features = Vec::new();
        // TODO HashMap nondet order
        for (idx, (id, node)) in self.derived_nodes.iter().enumerate() {
            let mut f = self.mercator.to_wgs84_gj(&Point::from(node.pt));
            f.id = Some(geojson::feature::Id::Number(idx.into()));
            f.set_property("id", id.0);
            if !node.tags.0.is_empty() {
                f.set_property("tags", serde_json::to_value(&node.tags).map_err(err_to_js)?);
            }
            f.set_property("is_crossing", node.tags.is("highway", "crossing"));
            features.push(f);
        }
        serde_json::to_string(&GeoJson::from(features)).map_err(err_to_js)
    }

    #[wasm_bindgen(js_name = getWays)]
    pub fn get_ways(&self) -> Result<String, JsValue> {
        let mut features = Vec::new();
        // TODO HashMap nondet order
        for (idx, (id, way)) in self.derived_ways.iter().enumerate() {
            let mut f = self.mercator.to_wgs84_gj(&way.linestring);
            f.id = Some(geojson::feature::Id::Number(idx.into()));
            f.set_property("id", id.0);
            f.set_property("tags", serde_json::to_value(&way.tags).map_err(err_to_js)?);
            f.set_property("kind", way.kind.to_simple_string());
            f.set_property("num_crossings", way.num_crossings);
            if let Kind::QuickfixRoadway(ref fix) = way.kind {
                f.set_property("fix", serde_json::to_value(&fix).map_err(err_to_js)?);
            }
            if let Kind::BadRoadway(ref problem) = way.kind {
                f.set_property(
                    "problem",
                    serde_json::to_value(&problem).map_err(err_to_js)?,
                );
            }
            features.push(f);
        }
        serde_json::to_string(&GeoJson::from(features)).map_err(err_to_js)
    }

    #[wasm_bindgen(js_name = getMetrics)]
    pub fn get_metrics(&self) -> Result<String, JsValue> {
        serde_json::to_string(&Metrics::new(self)).map_err(err_to_js)
    }

    #[wasm_bindgen(js_name = previewSidewalk)]
    pub fn preview_sidewalk(
        &self,
        base: i64,
        left_meters: f64,
        right_meters: f64,
    ) -> Result<String, JsValue> {
        let (left, right) = self
            .make_sidewalk(WayID(base), left_meters, right_meters)
            .map_err(err_to_js)?;
        let mut features = Vec::new();
        for new_sidewalk in vec![left, right].into_iter().flatten() {
            features.push(self.mercator.to_wgs84_gj(&new_sidewalk.linestring));
            for (_, new_node, _) in new_sidewalk.crossing_points {
                features.push(self.mercator.to_wgs84_gj(&Point::from(new_node)));
            }
        }
        Ok(serde_json::to_string(&GeoJson::from(features)).map_err(err_to_js)?)
    }

    #[wasm_bindgen(js_name = editMakeSidewalk)]
    pub fn edit_make_sidewalk(
        &mut self,
        base: i64,
        left_meters: f64,
        right_meters: f64,
    ) -> Result<(), JsValue> {
        let mut edits = self.edits.take().unwrap();
        edits
            .apply_cmd(
                UserCmd::MakeSidewalk(WayID(base), left_meters, right_meters),
                self,
            )
            .map_err(err_to_js)?;
        self.edits = Some(edits);
        self.after_edit();
        Ok(())
    }

    #[wasm_bindgen(js_name = editApplyQuickfix)]
    pub fn edit_apply_quickfix(&mut self, base: i64, quickfix: JsValue) -> Result<(), JsValue> {
        let quickfix: Quickfix = serde_wasm_bindgen::from_value(quickfix)?;
        let mut edits = self.edits.take().unwrap();
        edits
            .apply_cmd(UserCmd::ApplyQuickfix(WayID(base), quickfix), self)
            .map_err(err_to_js)?;
        self.edits = Some(edits);
        self.after_edit();
        Ok(())
    }

    #[wasm_bindgen(js_name = editClear)]
    pub fn edit_clear(&mut self) {
        self.edits = Some(Edits::default());
        self.after_edit();
    }

    /// List of UserCmd
    #[wasm_bindgen(js_name = getEdits)]
    pub fn get_edits(&self) -> Result<String, JsValue> {
        serde_json::to_string(&self.edits.as_ref().unwrap().user_commands).map_err(err_to_js)
    }

    #[wasm_bindgen(js_name = toOsc)]
    pub fn to_osc(&self) -> String {
        self.edits.as_ref().unwrap().to_osc(self)
    }

    #[wasm_bindgen(js_name = toOsmChangeJson)]
    pub fn to_osmchange_json(&self) -> Result<String, JsValue> {
        self.edits
            .as_ref()
            .unwrap()
            .to_osmchange_json(self)
            .map_err(err_to_js)
    }
}

fn err_to_js<E: std::fmt::Display>(err: E) -> JsValue {
    JsValue::from_str(&err.to_string())
}

#[derive(Default, Serialize)]
struct Metrics {
    total_length_meters: HashMap<&'static str, f64>,
}

impl Metrics {
    fn new(model: &Speedwalk) -> Self {
        let mut metrics = Self::default();
        for way in model.derived_ways.values() {
            *metrics
                .total_length_meters
                .entry(way.kind.to_simple_string())
                .or_insert(0.0) += Euclidean.length(&way.linestring);
        }
        metrics
    }
}
