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
use chrono::SecondsFormat;
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
pub struct TileQuery {
    pub layers: Option<String>,
    pub at: Option<String>,
}

/// `/v1/style.json?at=` — the DVR instant, carried by the style rather than
/// applied to each source afterwards.
///
/// This exists for MapLibre. A vector source's tile URL template is fixed once
/// the style is loaded: there is no supported way to rewrite it in place, so a
/// client that wanted to scrub would have to tear down and re-add every source
/// and every layer that referenced it, in order, and get the draw order right
/// again by hand. Putting `at` in the style URL makes the whole scrub one
/// `setStyle(url)` — the server rebuilds a self-consistent style and the client
/// stays a slider bound to a string. It also gives each instant a distinct URL,
/// which is exactly what MapLibre's on-disk style cache keys on, so a past
/// instant caches correctly and live never does.
#[derive(Debug, Default, Deserialize)]
pub struct StyleQuery {
    pub at: Option<String>,
    /// Ground only: the basemap, and none of the layers Argus collects.
    ///
    /// This is what an offline region is cut from. MapLibre's offline manager
    /// takes a style and downloads every tile it references, which for the full
    /// style would mean packaging the live contacts too — freezing an hour of
    /// aircraft into a file and replaying them forever as though they were
    /// current. What is worth having on a phone with no signal is the ground:
    /// the basemap is the part that does not change, and the contacts should be
    /// live or absent, never stale.
    #[serde(default)]
    pub basemap_only: bool,
    /// Which named basemap to put under the layers. Absent is the configured
    /// default; an unknown name also falls back to it rather than erroring,
    /// because a client holding a stale preference should get a map.
    pub basemap: Option<String>,
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

    // A DVR tile describes a fixed past instant and is immutable once the
    // rollup behind it has settled. A live tile is not immutable, but it is not
    // uncacheable either: it is true for about as long as the ingest cadence,
    // and saying so is what makes a map refresh itself.
    //
    // MapLibre Native re-requests a tile when its cached copy expires, and has
    // no other API for "this is stale now" — there is no way to invalidate a
    // vector source in place. Under `no-store` the consequence was a map that
    // never moved until the user panned: contacts frozen at whatever second the
    // style happened to load. Fifteen seconds is under the fastest driver's
    // poll interval, so nothing is served past its usefulness, and the map
    // advances on its own.
    let cache = if request.at.is_some() {
        "public, max-age=3600"
    } else {
        "public, max-age=15"
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
    Query(query): Query<StyleQuery>,
) -> ApiResult<Response> {
    let rows = state.store.layers().await?;
    let base = style_base(&headers, &state.config.public_url);

    // Parsed and re-serialised rather than pasted through, so a malformed
    // instant fails here — once, with a message — instead of arriving as a 400
    // on every tile request the style goes on to generate.
    let at = query.at.as_deref().map(parse_instant).transpose()?;
    // Serialised in the `Z` form rather than with a numeric offset, because
    // `+00:00` in a query string decodes to a space. The tile route restores
    // that, but a URL that never needed rescuing is better than one that does.
    let at_suffix = at
        .map(|at| format!("?at={}", at.to_rfc3339_opts(SecondsFormat::Millis, true)))
        .unwrap_or_default();

    let mut sources = serde_json::Map::new();
    let mut style_layers = Vec::new();

    // The basemap goes in first so it is underneath everything: MapLibre draws
    // style layers in array order, and a ground that arrives last is a ground
    // painted over every contact on the map.
    let chosen = query
        .basemap
        .as_deref()
        .and_then(|name| state.config.basemaps.get(name))
        .or(state.config.basemap.as_ref());

    if let Some(basemap) = chosen {
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
        let mut paint = serde_json::Map::new();
        if let Some(v) = basemap.paint.brightness_max {
            paint.insert("raster-brightness-max".into(), json!(v));
        }
        if let Some(v) = basemap.paint.brightness_min {
            paint.insert("raster-brightness-min".into(), json!(v));
        }
        if let Some(v) = basemap.paint.saturation {
            paint.insert("raster-saturation".into(), json!(v));
        }
        if let Some(v) = basemap.paint.contrast {
            paint.insert("raster-contrast".into(), json!(v));
        }
        style_layers.push(json!({
            "id": "basemap",
            "type": "raster",
            "source": "basemap",
            "paint": paint,
        }));
    }

    for row in rows {
        if query.basemap_only {
            break;
        }
        let Some(kind) = argus_store::model::parse_entity_kind(&row.entity_kind) else {
            continue;
        };
        let style = LayerStyle::for_layer(&row.layer_id, kind);
        let id = &row.layer_id;

        sources.insert(
            id.clone(),
            json!({
                "type": "vector",
                "tiles": [format!("{base}/v1/tiles/{id}/{{z}}/{{x}}/{{y}}.mvt{at_suffix}")],
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
            // A glyph rather than a dot, named after the layer.
            //
            // The client draws and registers one image per layer, tinted with
            // the colour this catalogue supplies, so a layer added to the
            // daemon tomorrow gets a correctly-coloured contact without an app
            // release — the same property the rest of this document has.
            //
            // `coalesce` is what keeps that safe. A symbol layer whose image is
            // missing renders *nothing*, so a client that had not yet drawn
            // this layer's glyph would show an empty map rather than an ugly
            // one. Falling back to `argus-contact` — which every client
            // registers once, unconditionally — turns that into a generic
            // marker instead of a silent disappearance.
            let rotation = if style.rotates_with_course {
                // Course is the direction of travel, heading is where the nose
                // points; they differ in a crosswind. Whichever the source
                // actually measured is the one worth drawing, in that order,
                // and 0 rather than null so an unrotatable contact still draws.
                json!(["coalesce", ["get", "course_deg"], ["get", "heading_deg"], 0])
            } else {
                json!(0)
            };

            style_layers.push(json!({
                "id": format!("{id}-point"),
                "type": "symbol",
                "source": id,
                "source-layer": id,
                "filter": ["==", ["geometry-type"], "Point"],
                "layout": {
                    "icon-image": [
                        "coalesce", ["image", id], ["image", "argus-contact"]
                    ],
                    // Scaled by zoom rather than fixed. At a constant size the
                    // glyphs are either unreadable dots across a continent or,
                    // at the size that reads well there, a solid mass of
                    // overlapping chevrons over an airport — the first attempt
                    // rendered Heathrow as an unreadable blob. Small when the
                    // view is wide, legible when it is close.
                    "icon-size": [
                        "interpolate", ["linear"], ["zoom"],
                        3, 0.3,
                        7, 0.42,
                        11, 0.62,
                        15, 0.85
                    ],
                    // Contacts overlap constantly around an airport, and a
                    // decluttered map that hides half the approach is worse
                    // than a busy one that shows it.
                    "icon-allow-overlap": true,
                    "icon-ignore-placement": true,
                    // Rotate with the map, not the screen: a heading is a
                    // bearing on the ground, and keeping it screen-aligned
                    // would make every contact lie as soon as the map turned.
                    "icon-rotation-alignment": "map",
                    "icon-rotate": rotation,
                },
                "paint": {
                    // Modeled and estimated positions are drawn faint. The rule
                    // that a client must never present a propagated satellite
                    // as an observed one has to survive into the style, or it
                    // survives nowhere.
                    "icon-opacity": [
                        "match", ["get", "quality"],
                        "live", 1.0,
                        "delayed", 0.75,
                        0.4
                    ],
                },
            }));
        }
    }

    // A style describing a fixed past instant is immutable; a live one names
    // the layers that exist right now and must not outlive them. The same rule
    // the tile route follows, for the same reason.
    let cache = if at.is_some() || query.basemap_only {
        "public, max-age=3600"
    } else {
        "no-store"
    };

    Ok((
        [(header::CACHE_CONTROL, cache)],
        Json(json!({
            "version": 8,
            "name": "Argus",
            // Echoed so a client can label what it is showing without having to
            // remember which URL it asked for.
            "metadata": {
                "argus:at": at.map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true)),
                // The basemaps on offer, so a client can present the choice
                // without a second endpoint or a hard-coded list — the same
                // reasoning that puts the layer catalogue on the server.
                "argus:basemaps": state.config.basemaps.keys().collect::<Vec<_>>(),
                "argus:basemap": query.basemap,
            },
            "sources": sources,
            "layers": style_layers,
        })),
    )
        .into_response())
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

