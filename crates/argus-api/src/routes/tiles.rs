//! `/v1/tiles` — vector tiles, and the MapLibre style that consumes them.
//!
//! Serving the style from the server rather than shipping it in each client is
//! what keeps a new layer from needing an app release: the Android app and the
//! web client both fetch `/v1/style.json`, and a driver added today appears in
//! both tomorrow.

use crate::error::{ApiError, ApiResult};
use crate::params::{split_csv, parse_instant};
use crate::ApiState;
use argus_core::geo::TileCoord;
use argus_core::layer::{GeometryClass, LayerStyle};
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
pub struct TileQuery {
    pub layers: Option<String>,
    pub at: Option<String>,
}

/// `/v1/tiles/{z}/{x}/{y}.mvt` — every layer, or the ones named in `?layers=`.
pub async fn tile(
    State(state): State<ApiState>,
    Path((z, x, y_ext)): Path<(u8, u32, String)>,
    Query(query): Query<TileQuery>,
) -> ApiResult<Response> {
    let layers = query.layers.as_deref().map(split_csv).unwrap_or_default();
    render(&state, z, x, &y_ext, layers, query.at.as_deref()).await
}

/// `/v1/tiles/{layer}/{z}/{x}/{y}.mvt` — the single-layer form, which is what a
/// MapLibre source URL template looks like.
pub async fn layer_tile(
    State(state): State<ApiState>,
    Path((layer, z, x, y_ext)): Path<(String, u8, u32, String)>,
    Query(query): Query<TileQuery>,
) -> ApiResult<Response> {
    render(&state, z, x, &y_ext, vec![layer], query.at.as_deref()).await
}

async fn render(
    state: &ApiState,
    z: u8,
    x: u32,
    y_ext: &str,
    layers: Vec<String>,
    at: Option<&str>,
) -> ApiResult<Response> {
    let y: u32 = y_ext
        .strip_suffix(".mvt")
        .or_else(|| y_ext.strip_suffix(".pbf"))
        .unwrap_or(y_ext)
        .parse()
        .map_err(|_| ApiError::BadRequest(format!("'{y_ext}' is not a tile row")))?;

    let request = argus_tiles::TileRequest {
        coord: TileCoord::new(z, x, y),
        filter: argus_store::EntityFilter::layers(layers),
        at: at.map(parse_instant).transpose()?,
    };
    let bytes = state.tiler.vector_tile(&request).await?;

    // An empty tile is a real answer, and 204 is how a map client is told so.
    // A 404 would make MapLibre treat it as a transient failure and retry the
    // same empty tile forever.
    if bytes.is_empty() {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // Live tiles must not be cached; a DVR tile describes a fixed past instant
    // and is immutable once the rollup behind it has settled.
    let cache = if request.at.is_some() {
        "public, max-age=3600"
    } else {
        "no-store"
    };

    Ok((
        [
            (header::CONTENT_TYPE, "application/vnd.mapbox-vector-tile"),
            (header::CACHE_CONTROL, cache),
        ],
        bytes,
    )
        .into_response())
}

/// A MapLibre style built from the live layer catalogue.
///
/// Deliberately minimal: no basemap, because both clients supply their own (the
/// Android app ships an offline region, the web client is on a photorealistic
/// globe). What this provides is the part that must stay in step with the
/// server — one source and one draw layer per Argus layer, with the zoom range
/// and colour the catalogue declares.
/// The address to build tile URLs from.
///
/// The requesting client's own `Host` first, and `public_url` only as a
/// fallback. Every client that fetches this style has just proved which address
/// reaches this server — it used one — so echoing it back is both more accurate
/// than a configured guess and impossible to misconfigure.
///
/// It matters because the same style is served to clients on different networks
/// with no address in common. A browser on the machine reaches the daemon at
/// `localhost`; the Android emulator reaches the very same socket at
/// `10.0.2.2`, because on a phone `localhost` is the phone; a real handset uses
/// the LAN or tailnet address. A single configured `public_url` is right for at
/// most one of them, and the failure is quiet — the style loads, and only the
/// tiles inside it fail.
///
/// Trusting `Host` is safe in the way that matters here: it only decides the
/// URLs handed back to the client that supplied it, so a forged value can
/// mislead nobody but itself.
fn style_base(headers: &axum::http::HeaderMap, public_url: &str) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|h| !h.is_empty());

    match host {
        Some(host) => {
            // Behind a TLS terminator the scheme is the proxy's, not ours.
            let scheme = headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .filter(|s| *s == "https" || *s == "http")
                .unwrap_or("http");
            format!("{scheme}://{host}")
        }
        None => public_url.trim_end_matches('/').to_string(),
    }
}

pub async fn style(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<Value>> {
    let rows = state.store.layers().await?;
    let base = style_base(&headers, &state.config.public_url);

    let mut sources = serde_json::Map::new();
    let mut style_layers = Vec::new();

    // The basemap goes in first so it is underneath everything: MapLibre draws
    // style layers in array order, and a ground that arrives last is a ground
    // painted over every contact on the map.
    if let Some(basemap) = state.config.basemap.as_ref() {
        sources.insert(
            "basemap".into(),
            json!({
                "type": "raster",
                "tiles": [basemap.tiles_url],
                "tileSize": 256,
                "maxzoom": 19,
                "attribution": basemap.attribution.clone().unwrap_or_default(),
            }),
        );
        style_layers.push(json!({
            "id": "basemap",
            "type": "raster",
            "source": "basemap",
        }));
    }

    for row in rows {
        let Some(kind) = argus_store::model::parse_entity_kind(&row.entity_kind) else {
            continue;
        };
        let style = LayerStyle::for_layer(&row.layer_id, kind);
        let id = &row.layer_id;

        sources.insert(
            id.clone(),
            json!({
                "type": "vector",
                "tiles": [format!("{base}/v1/tiles/{id}/{{z}}/{{x}}/{{y}}.mvt")],
                "minzoom": style.min_zoom,
                "maxzoom": style.max_zoom,
            }),
        );

        // Areas get a fill under an outline; lines get a stroke; points get a
        // circle. Symbols and icons are the client's business — a sprite sheet
        // is not something the server can usefully guess at.
        match style.geometry {
            GeometryClass::Area | GeometryClass::Mixed => {
                style_layers.push(json!({
                    "id": format!("{id}-fill"),
                    "type": "fill",
                    "source": id,
                    "source-layer": id,
                    "filter": ["==", ["geometry-type"], "Polygon"],
                    "paint": { "fill-color": style.color, "fill-opacity": 0.25 },
                }));
                style_layers.push(json!({
                    "id": format!("{id}-outline"),
                    "type": "line",
                    "source": id,
                    "source-layer": id,
                    "filter": ["==", ["geometry-type"], "Polygon"],
                    "paint": { "line-color": style.color, "line-width": 1.0 },
                }));
            }
            GeometryClass::Line => {}
            GeometryClass::Point => {}
        }
        if matches!(style.geometry, GeometryClass::Line | GeometryClass::Mixed) {
            style_layers.push(json!({
                "id": format!("{id}-line"),
                "type": "line",
                "source": id,
                "source-layer": id,
                "filter": ["==", ["geometry-type"], "LineString"],
                "paint": { "line-color": style.color, "line-width": 1.5 },
            }));
        }
        if matches!(style.geometry, GeometryClass::Point | GeometryClass::Mixed) {
            style_layers.push(json!({
                "id": format!("{id}-point"),
                "type": "circle",
                "source": id,
                "source-layer": id,
                "filter": ["==", ["geometry-type"], "Point"],
                "paint": {
                    "circle-color": style.color,
                    "circle-radius": 4.0,
                    // Modeled and estimated positions are drawn hollow. The
                    // rule that a client must never present a propagated
                    // satellite as an observed one has to survive into the
                    // style, or it survives nowhere.
                    "circle-opacity": [
                        "match", ["get", "quality"],
                        "live", 0.9,
                        "delayed", 0.7,
                        0.35
                    ],
                    "circle-stroke-color": style.color,
                    "circle-stroke-width": 1.0,
                },
            }));
        }
    }

    Ok(Json(json!({
        "version": 8,
        "name": "Argus",
        "sources": sources,
        "layers": style_layers,
    })))
}

#[cfg(test)]
mod style_base_tests {
    use super::style_base;
    use axum::http::{HeaderMap, HeaderValue, header};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            if *k == "host" {
                h.insert(header::HOST, HeaderValue::from_str(v).unwrap());
            } else {
                h.insert(
                    axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                );
            }
        }
        h
    }

    #[test]
    fn each_client_is_told_the_address_it_arrived_on() {
        // The bug this prevents: one configured address cannot be right for a
        // browser on the host, an emulator reaching the same socket via
        // 10.0.2.2, and a handset on the LAN. The style loads for all three and
        // only the tiles fail, which is a slow thing to diagnose.
        let cfg = "http://127.0.0.1:8787";
        assert_eq!(
            style_base(&headers(&[("host", "10.0.2.2:8787")]), cfg),
            "http://10.0.2.2:8787"
        );
        assert_eq!(
            style_base(&headers(&[("host", "192.168.1.20:8787")]), cfg),
            "http://192.168.1.20:8787"
        );
        assert_eq!(
            style_base(&headers(&[("host", "localhost:5173")]), cfg),
            "http://localhost:5173"
        );
    }

    #[test]
    fn a_terminating_proxy_keeps_its_own_scheme() {
        assert_eq!(
            style_base(
                &headers(&[("host", "argus.tail.ts.net"), ("x-forwarded-proto", "https")]),
                "http://127.0.0.1:8787"
            ),
            "https://argus.tail.ts.net"
        );
    }

    #[test]
    fn without_a_host_the_configured_address_stands() {
        assert_eq!(
            style_base(&HeaderMap::new(), "http://127.0.0.1:8787/"),
            "http://127.0.0.1:8787"
        );
    }
}

