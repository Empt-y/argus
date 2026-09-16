//! Presenters for every layer that is not aviation weather.

use crate::format::{self, bearing, celsius, duration, feet, metres, num, parse_time, when, wind_ms, words};
use crate::util::{push, row, row_note, section, Take};
use crate::{Card, Link, Row, Subject};
use serde_json::{Map, Value};

fn base<'a>(subject: Subject<'a>, subtitle: &str) -> Card {
    Card {
        title: subject.label.unwrap_or(subject.key).to_string(),
        subtitle: Some(subtitle.into()),
        summary: None,
        sections: Vec::new(),
        links: Vec::new(),
    }
}

// --- buoys -----------------------------------------------------------------

pub fn buoy<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Marine weather station");
    let station = t.str("station").unwrap_or(subject.key).to_string();
    if let Some(name) = t.str("name") {
        card.title = name.to_string();
    }
    let mut rows = Vec::new();
    let mut summary = Vec::new();
    match (t.f64("wind_dir_deg"), t.f64("wind_speed_ms"), t.f64("gust_ms")) {
        (Some(d), Some(s), g) => {
            let mut text = format!("{} at {}", bearing(d), wind_ms(s));
            if let Some(g) = g {
                text.push_str(&format!(", gusting {}", wind_ms(g)));
            }
            summary.push(format!("wind {text}"));
            rows.push(row("Wind", text));
        }
        (None, Some(s), _) => rows.push(row("Wind", wind_ms(s))),
        _ => {}
    }
    match (t.f64("wave_height_m"), t.f64("dominant_period_s"), t.f64("wave_dir_deg")) {
        (Some(h), period, dir) => {
            let mut text = format!("{} m", num(h, 1));
            if let Some(p) = period {
                text.push_str(&format!(" every {} s", num(p, 0)));
            }
            if let Some(d) = dir {
                text.push_str(&format!(" from {}", format::compass(d)));
            }
            summary.push(format!("waves {text}"));
            rows.push(row("Waves", text));
        }
        _ => {
            t.skip("dominant_period_s");
            t.skip("wave_dir_deg");
        }
    }
    push(&mut rows, "Average wave period", t.f64("average_period_s").map(|p| format!("{} s", num(p, 1))));
    if let Some(v) = t.f64("water_temp_c") {
        summary.push(format!("sea {}", celsius(v)));
        rows.push(row("Sea temperature", celsius(v)));
    }
    if let Some(v) = t.f64("air_temp_c") {
        summary.push(format!("air {}", celsius(v)));
        rows.push(row("Air temperature", celsius(v)));
    }
    push(&mut rows, "Dew point", t.f64("dewpoint_c").map(celsius));
    push(&mut rows, "Pressure", t.f64("pressure_hpa").map(|p| format!("{} hPa", num(p, 1))));
    push(
        &mut rows,
        "Pressure tendency",
        t.f64("pressure_tendency_hpa").map(|p| format!("{}{} hPa over 3 h", if p > 0.0 { "+" } else { "" }, num(p, 1))),
    );
    push(&mut rows, "Visibility", t.f64("visibility_nmi").map(|v| format!("{} nautical miles", num(v, 1))));
    push(&mut rows, "Tide", t.f64("tide_ft").map(|v| format!("{} ft above MLLW", num(v, 1))));
    card.summary = (!summary.is_empty()).then(|| {
        let mut s = summary.join(", ");
        if let Some(f) = s.get(..1) {
            let up = f.to_uppercase();
            s.replace_range(..1, &up);
        }
        s + "."
    });
    card.sections.extend(section(Some("Conditions"), rows));

    let mut rows = vec![row("Station", station)];
    push(&mut rows, "Type", t.str("station_type").map(str::to_string));
    push(&mut rows, "Owner", t.str("owner").map(str::to_string));
    push(&mut rows, "Note", t.str("note").map(str::to_string));
    card.sections.extend(section(Some("Station"), rows));
    card
}

// --- NWS alerts ------------------------------------------------------------

pub fn weather_alert<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Weather alert (US National Weather Service)");
    if let Some(e) = t.str("event") {
        card.title = e.to_string();
    }
    card.summary = t.str("headline").map(str::to_string);
    let mut rows = Vec::new();
    push(&mut rows, "Area", t.str("area").map(str::to_string));
    let sev = t.str("severity");
    let urg = t.str("urgency");
    let cert = t.str("certainty");
    if sev.is_some() || urg.is_some() || cert.is_some() {
        rows.push(row(
            "Level",
            [sev.map(|s| format!("{s} severity")), urg.map(|u| format!("{} urgency", u.to_lowercase())), cert.map(|c| c.to_lowercase())]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    push(&mut rows, "In effect", format::span(t.str("onset"), t.str("expires")));
    push(&mut rows, "Issued by", t.str("office").map(str::to_string));
    card.sections.extend(section(None, rows));
    let mut rows = Vec::new();
    push(&mut rows, "Details", t.str("description").map(str::to_string));
    push(&mut rows, "What to do", t.str("instruction").map(str::to_string));
    card.sections.extend(section(None, rows));
    for k in ["area_from", "has_polygon", "zones_unresolved"] {
        t.skip(k);
    }
    card
}

// --- flood warnings --------------------------------------------------------

pub fn flood_warning<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Flood warning (Environment Agency)");
    let severity = t.str("severity").map(str::to_string);
    let level = t.i64("severity_level");
    let meaning = match level {
        Some(1) => "Severe flood warning — danger to life",
        Some(2) => "Flood warning — flooding is expected, act now",
        Some(3) => "Flood alert — flooding is possible, be prepared",
        Some(4) => "No longer in force",
        _ => "",
    };
    card.summary = Some(if meaning.is_empty() { severity.clone().unwrap_or_default() } else { meaning.to_string() });
    let mut rows = Vec::new();
    if let Some(s) = severity {
        rows.push(row_note("Severity", s, meaning));
    }
    push(&mut rows, "Area", t.str("description").map(str::to_string));
    push(&mut rows, "River or sea", t.str("river_or_sea").map(str::to_string));
    push(&mut rows, "County", t.str("county").map(str::to_string));
    if t.bool("tidal") == Some(true) {
        rows.push(row("Tidal", "yes — this area floods from the sea or a tidal river"));
    }
    push(&mut rows, "Raised", t.str("raised").and_then(when));
    push(&mut rows, "EA area", t.str("ea_area").map(str::to_string));
    push(&mut rows, "Flood area code", t.str("flood_area").map(str::to_string));
    card.sections.extend(section(None, rows));
    let mut rows = Vec::new();
    push(&mut rows, "Message", t.str("message").map(str::to_string));
    card.sections.extend(section(None, rows));
    card
}

// --- river gauges ----------------------------------------------------------

pub fn river_gauge<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "River, tide or rainfall gauge (Environment Agency)");
    let mut rows = Vec::new();
    let mut summary = Vec::new();
    if let Some(readings) = t.value("readings").and_then(Value::as_array) {
        for r in readings {
            let name = r.get("name").and_then(Value::as_str).unwrap_or("Reading");
            let unit = r.get("unit").and_then(Value::as_str).unwrap_or("");
            let value = r.get("value").and_then(Value::as_f64);
            let at = r.get("at").and_then(Value::as_str).and_then(when);
            let qualifier = r.get("qualifier").and_then(Value::as_str);
            if let Some(v) = value {
                let text = format!("{} {}", num(v, 3), unit.replace("mAOD", "m above datum").replace("mASD", "m above stage datum"));
                let label = match qualifier {
                    Some(q) if q != "Stage" => format!("{name} ({q})"),
                    _ => name.to_string(),
                };
                summary.push(format!("{}: {text}", label.to_lowercase()));
                rows.push(match at {
                    Some(at) => row_note(label, text, format!("at {at}")),
                    None => row(label, text),
                });
            }
        }
    }
    card.summary = (!summary.is_empty()).then(|| {
        let mut s = summary.join(", ");
        if let Some(f) = s.get(..1) {
            let up = f.to_uppercase();
            s.replace_range(..1, &up);
        }
        s + "."
    });
    card.sections.extend(section(Some("Latest readings"), rows));
    let mut rows = Vec::new();
    push(&mut rows, "River", t.str("river").map(str::to_string));
    push(&mut rows, "Town", t.str("town").map(str::to_string));
    push(&mut rows, "Catchment", t.str("catchment").map(str::to_string));
    push(&mut rows, "Status", t.str("status").map(words));
    push(&mut rows, "Station", t.str("station").map(str::to_string));
    t.skip("rloi_id");
    card.sections.extend(section(Some("Station"), rows));
    card
}

// --- storm overflows -------------------------------------------------------

pub fn storm_overflow<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    // The layer has two shapes: the outfall itself (a station, with a
    // state) and each discharge from it (an event, with a start and an
    // end). Water UK's companies write both.
    if attrs.contains_key("started") {
        return discharge(subject, t);
    }
    let mut card = base(subject, "Sewage storm overflow");
    if let Some(w) = t.str("receiving_water") {
        card.title = w.to_string();
    }
    let state = t.str("state").unwrap_or("unknown");
    let (state_words, note) = match state {
        "discharging" => ("Discharging now", "untreated sewage is being released into this water"),
        "not_discharging" => ("Not discharging", "no release at present"),
        "recent" => ("Discharged recently", "a release ended within the last 24 hours"),
        "offline" => ("Monitor offline", "the company's monitor is not reporting"),
        other => (other, ""),
    };
    card.summary = Some(match t.str("state_since").and_then(when) {
        Some(since) => format!("{state_words} since {since}."),
        None => format!("{state_words}."),
    });
    let mut rows = vec![row_note("Status", state_words, note)];
    push(&mut rows, "Company's own words", t.str("state_text").map(str::to_string));
    push(&mut rows, "Water", t.str("receiving_water").map(str::to_string));
    push(&mut rows, "Company", t.str("company").map(str::to_string));
    let start = t.str("last_discharge_start");
    let end = t.str("last_discharge_end");
    if let (Some(a), Some(b)) = (start.and_then(parse_time), end.and_then(parse_time)) {
        rows.push(row(
            "Last discharge",
            format!("{} for {}", when(start.unwrap_or("")).unwrap_or_default(), duration((b - a).num_seconds())),
        ));
    } else if let Some(s) = start.and_then(when) {
        rows.push(row("Last discharge started", s));
    }
    if t.bool("stale") == Some(true) {
        rows.push(row("Data age", "the company has not updated this record within a day"));
    }
    push(&mut rows, "Record updated", t.str("record_updated").and_then(when));
    push(&mut rows, "Outfall", t.str("outfall_id").map(str::to_string));
    for k in ["licence", "permit"] {
        push(&mut rows, &words(k), t.str(k).map(str::to_string));
    }
    card.sections.extend(section(None, rows));
    card
}

/// One discharge from an outfall.
fn discharge<'a>(subject: Subject<'a>, mut t: Take<'a, '_>) -> Card {
    let mut card = base(subject, "Sewage discharge");
    let water = t.str("receiving_water");
    let ongoing = t.bool("ongoing") == Some(true);
    card.title = match water {
        Some(w) if ongoing => format!("Discharging into {w}"),
        Some(w) => format!("Discharged into {w}"),
        None => subject.label.unwrap_or(subject.key).to_string(),
    };
    let started = t.str("started");
    let ended = t.str("ended");
    let minutes = t.i64("duration_minutes");
    card.summary = Some(match (started.and_then(when), ended.and_then(when), minutes) {
        (Some(s), _, _) if ongoing => format!("Untreated sewage has been released since {s} and is still going."),
        (Some(s), Some(_), Some(m)) => format!("Untreated sewage was released from {s} for {}.", duration(m * 60)),
        (Some(s), Some(e), None) => format!("Untreated sewage was released from {s} until {e}."),
        (Some(s), None, _) => format!("Untreated sewage was released from {s}."),
        _ => "A sewage discharge.".to_string(),
    });
    let mut rows = vec![row("Status", if ongoing { "discharging now" } else { "ended" })];
    push(&mut rows, "Started", started.and_then(when));
    push(&mut rows, "Ended", ended.and_then(when));
    push(&mut rows, "Duration", minutes.map(|m| duration(m * 60)));
    push(&mut rows, "Water", water.map(str::to_string));
    push(&mut rows, "Company", t.str("company").map(str::to_string));
    push(&mut rows, "Outfall", t.str("outfall_id").map(str::to_string));
    card.sections.extend(section(None, rows));
    card
}

// --- earthquakes -----------------------------------------------------------

pub fn earthquake<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Earthquake");
    let mag = t.f64("magnitude");
    let place = t.str("place");
    if let (Some(m), Some(p)) = (mag, place) {
        card.title = format!("Magnitude {} — {p}", num(m, 1));
    }
    let depth = t.f64("depth_km");
    let mut rows = Vec::new();
    if let Some(m) = mag {
        let feel = match m {
            x if x < 2.0 => "not felt",
            x if x < 4.0 => "felt by a few people nearby, no damage",
            x if x < 5.0 => "felt widely, little damage",
            x if x < 6.0 => "can damage poorly built buildings",
            x if x < 7.0 => "damaging over a wide area",
            _ => "major — serious damage over a large area",
        };
        rows.push(row_note("Magnitude", num(m, 1), feel));
    }
    if let Some(d) = depth {
        let kind = if d < 70.0 { "shallow" } else if d < 300.0 { "intermediate" } else { "deep" };
        rows.push(row_note("Depth", format!("{} km", num(d, 1)), kind));
    }
    push(&mut rows, "Type", t.str("event_type").map(|e| e.replace('_', " ")));
    if t.bool("tsunami") == Some(true) {
        rows.push(row("Tsunami", "a tsunami message was issued"));
    }
    push(&mut rows, "Felt reports", t.i64("felt_reports").map(|n| n.to_string()));
    push(&mut rows, "PAGER alert", t.str("alert_level").map(|l| format!("{l} — estimated impact level")));
    push(&mut rows, "Stations", t.i64("stations").map(|n| format!("{n} reporting")));
    push(&mut rows, "Updated", t.str("updated_at").and_then(when));
    if let Some(r) = t.f64("modeled_radius_m") {
        let kind = t.str("modeled_radius_kind").unwrap_or("area").replace('_', " ");
        rows.push(row_note("Estimated reach", format!("~{} radius", metres(r)), format!("{kind}, modelled from magnitude, not observed")));
    } else {
        t.skip("modeled_radius_kind");
    }
    card.summary = match (mag, depth) {
        (Some(m), Some(d)) => Some(format!("Magnitude {} at {} km depth.", num(m, 1), num(d, 0))),
        (Some(m), None) => Some(format!("Magnitude {}.", num(m, 1))),
        _ => None,
    };
    card.sections.extend(section(None, rows));
    card
}

// --- flights ---------------------------------------------------------------

fn squawk(code: &str) -> Option<&'static str> {
    match code {
        "7500" => Some("HIJACK — unlawful interference"),
        "7600" => Some("radio failure"),
        "7700" => Some("EMERGENCY"),
        "1000" => Some("IFR flight under ADS-B, no discrete code assigned"),
        "1200" => Some("VFR, no code assigned (US)"),
        "7000" => Some("VFR, no code assigned (Europe)"),
        "2000" => Some("entering airspace from an area without radar"),
        "0000" => Some("no code set"),
        _ => None,
    }
}

fn category(code: &str) -> Option<&'static str> {
    match code {
        "A0" => Some("no category information"),
        "A1" => Some("light aircraft, under 7 t"),
        "A2" => Some("small aircraft, 7–34 t"),
        "A3" => Some("large aircraft, 34–136 t"),
        "A4" => Some("high-vortex large aircraft"),
        "A5" => Some("heavy aircraft, over 136 t"),
        "A6" => Some("high performance, over 5 g and 400 kt"),
        "A7" => Some("rotorcraft"),
        "B1" => Some("glider or sailplane"),
        "B2" => Some("lighter-than-air"),
        "B3" => Some("parachutist or skydiver"),
        "B4" => Some("ultralight, hang-glider or paraglider"),
        "B6" => Some("unmanned aerial vehicle"),
        "B7" => Some("spacecraft or trans-atmospheric"),
        "C1" => Some("surface vehicle, emergency"),
        "C2" => Some("surface vehicle, service"),
        "C3" => Some("fixed ground or tethered obstruction"),
        _ => None,
    }
}

pub fn flight<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Aircraft");
    let callsign = t.str("callsign");
    let reg = t.str("registration");
    let type_code = t.str("type_code");
    card.title = match (callsign, reg) {
        (Some(c), Some(r)) if c != r => format!("{c} ({r})"),
        (Some(c), _) => c.to_string(),
        (None, Some(r)) => r.to_string(),
        _ => subject.label.unwrap_or(subject.key).to_string(),
    };
    let mut rows = Vec::new();
    push(&mut rows, "Callsign", callsign.map(str::to_string));
    push(&mut rows, "Registration", reg.map(str::to_string));
    push(&mut rows, "Type", type_code.map(str::to_string));
    if let Some(c) = t.str("category") {
        rows.push(match category(c) {
            Some(words) => row_note("Category", words, c),
            None => row("Category", c),
        });
    }
    let on_ground = t.bool("on_ground");
    // adsb.fi and adsb.lol report altitude in feet, OpenSky in metres; the
    // card shows feet with metres beside it either way.
    let baro = t.f64("baro_altitude_ft").or_else(|| t.f64("baro_altitude_m").map(|m| m / 0.3048));
    let geo = t.f64("geom_altitude_ft").or_else(|| t.f64("geo_altitude_m").map(|m| m / 0.3048));
    match (baro, geo) {
        (Some(b), Some(g)) => {
            rows.push(row_note("Altitude", feet(b), "pressure altitude, against the standard 1013 hPa datum"));
            rows.push(row_note("Height (GPS)", feet(g), "geometric height above the ellipsoid"));
        }
        (Some(b), None) => rows.push(row_note("Altitude", feet(b), "pressure altitude, against the standard 1013 hPa datum")),
        (None, Some(g)) => rows.push(row("Height (GPS)", feet(g))),
        _ => {}
    }
    if on_ground == Some(true) {
        rows.push(row("On the ground", "yes"));
    }
    push(&mut rows, "Magnetic heading", t.f64("magnetic_heading_deg").map(bearing));
    if let Some(s) = t.str("squawk") {
        rows.push(match squawk(s) {
            Some(meaning) => row_note("Squawk", s, meaning),
            None => row("Squawk", s),
        });
    }
    if let Some(e) = t.str("emergency")
        && e != "none" {
            rows.push(row("Emergency", e.to_uppercase()));
        }
    if t.bool("mlat") == Some(true) {
        rows.push(row_note("Position from", "multilateration", "computed from signal timing at several receivers, not from the aircraft's own GPS"));
    }
    push(&mut rows, "Position age", t.f64("position_age_s").map(|s| format!("{} s", num(s, 0))));
    push(&mut rows, "Registered in", t.str("origin_country").map(str::to_string));
    card.sections.extend(section(None, rows));
    let mut links = Vec::new();
    if let Some(r) = reg {
        links.push(Link { label: "Photos and history".into(), url: format!("https://www.flightradar24.com/data/aircraft/{}", r.to_lowercase()) });
    }
    card.links = links;
    card
}

// --- radiosondes -----------------------------------------------------------

pub fn radiosonde<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Weather balloon (radiosonde)");
    let phase = t.str("phase");
    let mut rows = Vec::new();
    if let Some(p) = phase {
        let words = match p {
            "ascent" => "ascending — the balloon is still rising",
            "descent" => "descending — the balloon has burst and the sonde is falling",
            "landed" => "landed",
            other => other,
        };
        rows.push(row("Phase", words));
    }
    let temp = t.f64("temp_c");
    push(&mut rows, "Air temperature", temp.map(celsius));
    push(&mut rows, "Humidity", t.f64("humidity_pct").map(|h| format!("{}%", num(h, 0))));
    push(&mut rows, "Pressure", t.f64("pressure_hpa").map(|p| format!("{} hPa", num(p, 1))));
    card.summary = match (phase, temp) {
        (Some(p), Some(tc)) => Some(format!("{}, {} outside.", words(p), celsius(tc))),
        (Some(p), None) => Some(format!("{}.", words(p))),
        _ => None,
    };
    card.sections.extend(section(Some("Conditions aloft"), rows));
    let mut rows = Vec::new();
    let maker = t.str("manufacturer");
    let sonde = t.str("sonde_type");
    let sub = t.str("subtype");
    push(&mut rows, "Model", match (maker, sub.or(sonde)) {
        (Some(m), Some(s)) => Some(format!("{m} {s}")),
        (_, Some(s)) => Some(s.to_string()),
        (Some(m), None) => Some(m.to_string()),
        _ => None,
    });
    push(&mut rows, "Serial", t.str("serial").map(str::to_string));
    push(&mut rows, "Frequency", t.f64("frequency_mhz").map(|f| format!("{} MHz", num(f, 3))));
    push(&mut rows, "GPS satellites", t.i64("gps_satellites").map(|n| n.to_string()));
    push(&mut rows, "Burst timer", t.i64("burst_timer_s").map(|s| format!("{} until the sonde cuts down", duration(s))));
    push(&mut rows, "Heard by", t.str("heard_by").map(str::to_string));
    card.sections.extend(section(Some("Sonde"), rows));
    card
}

// --- satellites ------------------------------------------------------------

pub fn satellite<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Satellite, position propagated from orbital elements");
    if let Some(n) = t.str("object_name") {
        card.title = n.to_string();
    }
    let mut rows = Vec::new();
    let period = t.f64("period_minutes");
    let inc = t.f64("inclination_deg");
    let ecc = t.f64("eccentricity");
    push(&mut rows, "Orbital period", period.map(|p| format!("{} min ({} orbits a day)", num(p, 1), num(1440.0 / p, 1))));
    push(&mut rows, "Inclination", inc.map(|i| format!("{}° to the equator", num(i, 1))));
    push(&mut rows, "Eccentricity", ecc.map(|e| format!("{} — {}", num(e, 4), if e < 0.01 { "near-circular" } else if e < 0.1 { "slightly elliptical" } else { "elliptical" })));
    t.skip("mean_motion_rev_per_day");
    push(&mut rows, "Elements dated", t.str("epoch").and_then(when));
    if let Some(h) = t.f64("element_age_hours") {
        let trust = if h < 24.0 { "fresh" } else if h < 72.0 { "a few days old; position error grows with age" } else { "stale; treat the position as approximate" };
        rows.push(row_note("Element age", duration((h * 3600.0) as i64), trust));
    }
    card.summary = match (period, inc) {
        (Some(p), Some(i)) => Some(format!("Orbits every {} minutes at {}° inclination. Position is computed, not observed.", num(p, 0), num(i, 0))),
        _ => Some("Position is computed from orbital elements, not observed.".into()),
    };
    card.sections.extend(section(Some("Orbit"), rows));
    let mut rows = Vec::new();
    push(&mut rows, "NORAD number", t.i64("norad_id").map(|n| n.to_string()));
    push(&mut rows, "International designator", t.str("international_designator").map(str::to_string));
    card.sections.extend(section(Some("Identity"), rows));
    if let Some(n) = attrs.get("norad_id").and_then(Value::as_i64) {
        card.links.push(Link { label: "Track on N2YO".into(), url: format!("https://www.n2yo.com/satellite/?s={n}") });
    }
    card
}

// --- buses -----------------------------------------------------------------

pub fn bus<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Bus");
    let line = t.str("line");
    let dest = t.str("destination");
    card.title = match (line, dest) {
        (Some(l), Some(d)) => format!("{l} to {d}"),
        (Some(l), None) => format!("Route {l}"),
        (None, Some(d)) => format!("Bus to {d}"),
        _ => "Bus".into(),
    };
    let mut rows = Vec::new();
    push(&mut rows, "Route", line.map(str::to_string));
    push(&mut rows, "From", t.str("origin").map(str::to_string));
    push(&mut rows, "To", dest.map(str::to_string));
    push(&mut rows, "Direction", t.str("direction").map(words));
    if let Some(o) = t.str("occupancy") {
        rows.push(row("Occupancy", match o {
            "seatsAvailable" => "seats available",
            "standingAvailable" => "standing room only",
            "full" => "full",
            other => other,
        }));
    }
    push(&mut rows, "Scheduled departure", t.str("aimed_departure").and_then(when));
    push(&mut rows, "Scheduled arrival", t.str("aimed_arrival").and_then(when));
    card.sections.extend(section(Some("Journey"), rows));
    let mut rows = Vec::new();
    push(&mut rows, "Operator", t.str("operator").map(|o| format!("{o} (National Operator Code)")));
    push(&mut rows, "Vehicle", t.str("vehicle").map(str::to_string));
    push(&mut rows, "Journey", t.str("journey").map(str::to_string));
    push(&mut rows, "Block", t.str("block").map(str::to_string));
    card.sections.extend(section(Some("Vehicle"), rows));
    card
}

// --- Argo ------------------------------------------------------------------

pub fn argo<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Argo profiling float");
    if let Some(f) = t.str("float") {
        card.title = format!("Argo float {f}");
    }
    let surfaced = t.str("surfaced_at");
    let stale = t.bool("stale").unwrap_or(false);
    card.summary = surfaced.and_then(when).map(|s| {
        if stale {
            format!("Last surfaced {s}; it has since dived and its position is no longer known.")
        } else {
            format!("Surfaced {s} — this position is current.")
        }
    });
    let mut rows = Vec::new();
    push(&mut rows, "Last surfaced", surfaced.and_then(when));
    push(&mut rows, "Cycle", t.i64("cycle").map(|c| format!("{c} — the number of dives this float has made")));
    if let Some(d) = t.str("direction") {
        rows.push(row("Profile", match d {
            "A" => "ascending — measured on the way up",
            "D" => "descending — measured on the way down",
            other => other,
        }));
    }
    if let Some(m) = t.str("data_mode") {
        rows.push(row_note("Data mode", match m {
            "R" => "real-time — automatic checks only",
            "A" => "real-time, adjusted",
            "D" => "delayed-mode — scientist-checked",
            other => other,
        }, m));
    }
    push(&mut rows, "Position from", t.str("positioning_system").map(str::to_string));
    if let Some(q) = t.str("position_qc") {
        rows.push(row("Position quality", match q {
            "1" => "good",
            "2" => "probably good",
            "8" => "interpolated",
            other => other,
        }));
    }
    card.sections.extend(section(Some("Latest cycle"), rows));
    let mut rows = Vec::new();
    push(&mut rows, "Float type", t.str("platform_type").map(str::to_string));
    push(&mut rows, "Programme", t.str("project").map(str::to_string));
    push(&mut rows, "Principal investigator", t.str("pi").map(str::to_string));
    push(&mut rows, "Data centre", t.str("data_center").map(str::to_string));
    card.sections.extend(section(Some("Float"), rows));
    if let Some(u) = attrs.get("url").and_then(Value::as_str) {
        card.links.push(Link { label: "Float page (profiles and track)".into(), url: u.to_string() });
    }
    card
}

// --- meteors ---------------------------------------------------------------

fn shower_name(code: &str) -> Option<&'static str> {
    Some(match code {
        "PER" => "Perseids",
        "GEM" => "Geminids",
        "QUA" => "Quadrantids",
        "LYR" => "Lyrids",
        "ETA" => "eta Aquariids",
        "SDA" => "Southern delta Aquariids",
        "ORI" => "Orionids",
        "LEO" => "Leonids",
        "URS" => "Ursids",
        "TAU" | "NTA" => "Northern Taurids",
        "STA" => "Southern Taurids",
        "DRA" => "Draconids",
        "CAP" => "alpha Capricornids",
        "SPE" => "September epsilon Perseids",
        "NUE" => "nu Eridanids",
        "KCG" => "kappa Cygnids",
        "AUR" => "Aurigids",
        "OCT" => "October Camelopardalids",
        "MON" => "December Monocerotids",
        "HYD" => "sigma Hydrids",
        "COM" => "Comae Berenicids",
        "DLM" => "December Leonis Minorids",
        "ACL" => "Anthelion source",
        "BAU" => "beta Aurigids",
        "NIA" => "Northern iota Aquariids",
        "SLY" => "September Lyncids",
        "ICE" => "iota Cetids",
        _ => return None,
    })
}

pub fn meteor<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Meteor, triangulated by the Global Meteor Network");
    let shower = t.str("shower");
    card.title = match shower {
        Some(code) => match shower_name(code) {
            Some(n) => format!("{n} meteor"),
            None => format!("{code} shower meteor"),
        },
        None => "Sporadic meteor".into(),
    };
    let mag = t.f64("peak_abs_mag");
    let vgeo = t.f64("v_geo_kms");
    let hb = t.f64("height_begin_km");
    let he = t.f64("height_end_km");
    let mass = t.f64("mass_kg");
    let mut rows = Vec::new();
    if let Some(m) = mag {
        let words = if m <= -4.0 { "a fireball, brighter than Venus" } else if m <= -1.0 { "bright, like a bright star" } else if m <= 2.0 { "moderate" } else { "faint" };
        rows.push(row_note("Brightness", format!("magnitude {}", num(m, 1)), words));
    }
    if let (Some(b), Some(e)) = (hb, he) {
        rows.push(row("Lit from", format!("{} km down to {} km altitude", num(b, 0), num(e, 0))));
    }
    push(&mut rows, "Duration", t.f64("duration_s").map(|d| format!("{} s", num(d, 2))));
    if let Some(v) = vgeo {
        let origin = if v > 60.0 { "fast — likely from a long-period comet" } else if v > 40.0 { "medium" } else { "slow — likely asteroidal" };
        rows.push(row_note("Speed", format!("{} km/s", num(v, 1)), origin));
    }
    push(&mut rows, "Entry speed", t.f64("v_init_kms").map(|v| format!("{} km/s", num(v, 1))));
    t.skip("v_avg_kms");
    if let Some(kg) = mass {
        let text = if kg < 0.001 { format!("{} mg", num(kg * 1e6, 1)) } else if kg < 1.0 { format!("{} g", num(kg * 1000.0, 1)) } else { format!("{} kg", num(kg, 2)) };
        rows.push(row_note("Mass", text, "estimated from brightness"));
    }
    push(&mut rows, "Peak at", t.f64("peak_height_km").map(|h| format!("{} km altitude", num(h, 0))));
    card.summary = match (mag, vgeo) {
        (Some(m), Some(v)) => Some(format!("Magnitude {} meteor at {} km/s{}.", num(m, 1), num(v, 0), hb.map(|h| format!(", first seen at {} km", num(h, 0))).unwrap_or_default())),
        _ => None,
    };
    card.sections.extend(section(Some("Meteor"), rows));

    let mut rows = Vec::new();
    if let Some(orbit) = t.value("orbit").and_then(Value::as_object) {
        let g = |k: &str| orbit.get(k).and_then(Value::as_f64);
        push(&mut rows, "Semi-major axis", g("a_au").map(|a| format!("{} AU", num(a, 2))));
        push(&mut rows, "Perihelion", g("q_au").map(|q| format!("{} AU from the Sun", num(q, 3))));
        push(&mut rows, "Eccentricity", g("e").map(|e| num(e, 3)));
        push(&mut rows, "Inclination", g("i_deg").map(|i| format!("{}°{}", num(i, 1), if i > 90.0 { " — retrograde" } else { "" })));
        if let Some(tj) = g("tisserand_j") {
            let kind = if tj < 2.0 { "Halley-type or long-period comet orbit" } else if tj < 3.0 { "Jupiter-family comet orbit" } else { "asteroidal orbit" };
            rows.push(row_note("Tisserand parameter", num(tj, 2), kind));
        }
    }
    card.sections.extend(section(Some("Orbit before it hit the atmosphere"), rows));

    let mut rows = Vec::new();
    let stations = t.strings("stations");
    if !stations.is_empty() {
        rows.push(row("Cameras", stations.join(", ")));
    }
    t.skip("station_count");
    push(&mut rows, "Convergence angle", t.f64("convergence_deg").map(|c| format!("{}° — {}", num(c, 0), if c < 15.0 { "poor geometry, solution uncertain" } else { "good geometry" })));
    push(&mut rows, "Fit error", t.f64("fit_err_arcsec").map(|e| format!("{} arcsec", num(e, 0))));
    push(&mut rows, "Trajectory", t.str("trajectory").map(str::to_string));
    for k in ["lat_end", "lon_end", "shower_iau_no"] {
        t.skip(k);
    }
    card.sections.extend(section(Some("Observation"), rows));
    card
}

// --- SatNOGS ---------------------------------------------------------------

fn pass_rows(p: &Map<String, Value>) -> Vec<Row> {
    let mut rows = Vec::new();
    let sat = p.get("satellite").and_then(Value::as_str);
    let norad = p.get("norad_id").and_then(Value::as_i64);
    push(&mut rows, "Satellite", match (sat, norad) {
        (Some(s), Some(n)) => Some(format!("{s} (NORAD {n})")),
        (Some(s), None) => Some(s.to_string()),
        (None, Some(n)) => Some(format!("NORAD {n}")),
        _ => None,
    });
    push(&mut rows, "Window", format::span(p.get("start").and_then(Value::as_str), p.get("end").and_then(Value::as_str)));
    push(&mut rows, "Frequency", p.get("frequency_hz").and_then(Value::as_f64).map(|f| format!("{} MHz", num(f / 1e6, 3))));
    push(&mut rows, "Mode", p.get("mode").and_then(Value::as_str).map(str::to_string));
    push(&mut rows, "Transmitter", p.get("transmitter").and_then(Value::as_str).map(str::to_string));
    push(&mut rows, "Peak elevation", p.get("max_elevation_deg").and_then(Value::as_f64).map(|e| format!("{}° above the horizon", num(e, 0))));
    rows
}

pub fn ground_station<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Amateur satellite ground station (SatNOGS)");
    let status = t.str("status").unwrap_or("Unknown");
    let last_seen = t.str("last_seen").and_then(when);
    t.skip("stale");
    card.summary = Some(match (status, last_seen.as_deref()) {
        ("Online", _) => "Online and taking scheduled observations.".into(),
        ("Testing", _) => "Online in testing mode.".into(),
        (_, Some(l)) => format!("Offline; last heard from {l}."),
        _ => "Offline; never heard from.".into(),
    });
    if let Some(p) = t.value("listening_to").and_then(Value::as_object) {
        card.sections.extend(section(Some("Listening to now"), pass_rows(p)));
    }
    if let Some(p) = t.value("next").and_then(Value::as_object) {
        card.sections.extend(section(Some("Next pass"), pass_rows(p)));
    }
    let mut rows = vec![row("Status", status)];
    push(&mut rows, "Last seen", last_seen);
    let bands = t.strings("bands");
    if !bands.is_empty() {
        rows.push(row("Bands", bands.join(", ")));
    }
    if let Some(ants) = t.value("antennas").and_then(Value::as_array) {
        let parts: Vec<String> = ants
            .iter()
            .filter_map(|a| {
                let ty = a.get("type").and_then(Value::as_str)?;
                let lo = a.get("freq_min_hz").and_then(Value::as_f64);
                let hi = a.get("freq_max_hz").and_then(Value::as_f64);
                Some(match (lo, hi) {
                    (Some(l), Some(h)) => format!("{ty}, {}–{} MHz", num(l / 1e6, 0), num(h / 1e6, 0)),
                    _ => ty.to_string(),
                })
            })
            .collect();
        if !parts.is_empty() {
            rows.push(row("Antennas", parts.join("; ")));
        }
    }
    push(&mut rows, "Minimum elevation", t.f64("min_horizon_deg").map(|d| format!("{}° — passes lower than this are not scheduled", num(d, 0))));
    push(&mut rows, "Altitude", t.f64("altitude_m").map(|m| format!("{} m", num(m, 0))));
    push(&mut rows, "Locator", t.str("qth_locator").map(str::to_string));
    push(&mut rows, "Observations", t.i64("observations").map(|n| format!("{} recorded", num(n as f64, 0))));
    push(&mut rows, "Scheduled", t.i64("future_observations").map(|n| format!("{n} ahead")));
    push(&mut rows, "Success rate", t.f64("success_rate_pct").map(|p| format!("{}%", num(p, 0))));
    push(&mut rows, "Client", t.str("client_version").map(|v| format!("SatNOGS client {v}")));
    push(&mut rows, "Registered", t.str("created").and_then(when));
    push(&mut rows, "Description", t.str("description").map(str::to_string));
    t.skip("station_id");
    card.sections.extend(section(Some("Station"), rows));
    card
}

// --- TfL -------------------------------------------------------------------

pub fn road_disruption<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Road disruption (Transport for London)");
    let what = t.str("sub_category").or_else(|| t.str("category")).unwrap_or("Disruption");
    t.skip("category");
    let location = t.str("location").map(|l| l.replace(['[', ']'], ""));
    card.title = match &location {
        Some(l) => format!("{what} — {l}"),
        None => what.to_string(),
    };
    card.summary = t.str("description").map(str::to_string);
    let mut rows = Vec::new();
    if let Some(s) = t.str("severity") {
        rows.push(row("Severity", s));
    }
    if t.bool("planned") == Some(true) {
        rows.push(row("Planned", "yes — this has not started yet"));
    }
    if t.bool("closures") == Some(true) {
        rows.push(row("Closures", "yes — a road or lane is closed"));
    }
    push(&mut rows, "When", format::span(t.str("start"), t.str("end")));
    push(&mut rows, "Status", t.str("status").map(str::to_string));
    push(&mut rows, "Advice", t.str("current_update").map(str::to_string));
    push(&mut rows, "Updated", t.str("updated_at").and_then(when));
    push(&mut rows, "Interest", t.str("level_of_interest").map(str::to_string));
    if t.bool("provisional") == Some(true) {
        rows.push(row("Provisional", "yes — details may change"));
    }
    let corridors = t.strings("corridors");
    if !corridors.is_empty() {
        rows.push(row("Roads", corridors.iter().map(|c| c.to_uppercase()).collect::<Vec<_>>().join(", ")));
    }
    push(&mut rows, "Reference", t.str("disruption_id").map(str::to_string));
    card.sections.extend(section(None, rows));
    card
}

// --- carbon intensity ------------------------------------------------------

pub fn carbon<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Grid carbon intensity");
    if let Some(r) = t.str("region") {
        card.title = r.to_string();
    }
    let value = t.f64("intensity_gco2_kwh");
    let index = t.str("index");
    let mut rows = Vec::new();
    if let Some(v) = value {
        let meaning = match index {
            Some("very low") => "very low — mostly wind, nuclear and hydro",
            Some("low") => "low",
            Some("moderate") => "moderate",
            Some("high") => "high — mostly gas",
            Some("very high") => "very high — gas and coal",
            _ => "",
        };
        rows.push(row_note("Carbon intensity", format!("{} g CO₂ per kWh", num(v, 0)), meaning));
    }
    push(&mut rows, "Forecast", t.f64("intensity_forecast_gco2_kwh").map(|v| format!("{} g/kWh", num(v, 0))));
    push(&mut rows, "Measured", t.f64("intensity_actual_gco2_kwh").map(|v| format!("{} g/kWh", num(v, 0))));
    push(&mut rows, "Period", format::span(t.str("period_from"), t.str("period_to")));
    push(&mut rows, "Network operator", t.str("dno").map(str::to_string));
    t.skip("region_id");
    card.summary = match (value, index) {
        (Some(v), Some(i)) => Some(format!("{} g CO₂/kWh — {i}.", num(v, 0))),
        (Some(v), None) => Some(format!("{} g CO₂/kWh.", num(v, 0))),
        _ => None,
    };
    card.sections.extend(section(None, rows));
    if let Some(mix) = t.value("generation_mix_pct").and_then(Value::as_object) {
        let mut fuels: Vec<(&str, f64)> = mix.iter().filter_map(|(k, v)| v.as_f64().map(|p| (k.as_str(), p))).filter(|(_, p)| *p > 0.0).collect();
        fuels.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let rows: Vec<Row> = fuels.into_iter().map(|(f, p)| row(words(f), format!("{}%", num(p, 1)))).collect();
        card.sections.extend(section(Some("Generation mix"), rows));
    }
    card
}

// --- EMODnet ---------------------------------------------------------------

pub fn platform<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Offshore platform");
    let mut rows = Vec::new();
    push(&mut rows, "Status", t.str("status").map(str::to_string));
    push(&mut rows, "Operator", t.str("operator").map(str::to_string));
    push(&mut rows, "Function", t.str("function").map(str::to_string));
    push(&mut rows, "Produces", t.str("production").map(str::to_string));
    push(&mut rows, "Structure", t.str("category").map(str::to_string));
    push(&mut rows, "Country", t.str("country").map(str::to_string));
    push(&mut rows, "Water depth", t.f64("water_depth_m").map(|d| format!("{} m", num(d, 0))));
    push(&mut rows, "Distance from coast", t.f64("coast_distance_m").map(|d| format!("{} km", num(d / 1000.0, 0))));
    push(&mut rows, "Licence blocks", t.str("blocks").map(str::to_string));
    push(&mut rows, "In service since", t.str("valid_from").map(|v| if v.len() == 8 { format!("{}-{}-{}", &v[..4], &v[4..6], &v[6..]) } else { v.to_string() }));
    push(&mut rows, "Out of service", t.str("valid_to").map(str::to_string));
    push(&mut rows, "Notes", t.str("remarks").map(str::to_string));
    push(&mut rows, "Register id", t.str("platform_id").map(str::to_string));
    card.summary = match (attrs.get("status").and_then(Value::as_str), attrs.get("operator").and_then(Value::as_str)) {
        (Some(s), Some(o)) => Some(format!("{s}, operated by {o}.")),
        (Some(s), None) => Some(format!("{s}.")),
        _ => None,
    };
    card.sections.extend(section(None, rows));
    card
}

pub fn wind_farm<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Offshore wind farm");
    let status = t.str("status");
    let power = t.f64("power_mw");
    let turbines = t.i64("turbines");
    let mut rows = Vec::new();
    push(&mut rows, "Status", status.map(str::to_string));
    push(&mut rows, "Capacity", power.map(|p| if p >= 1000.0 { format!("{} GW", num(p / 1000.0, 2)) } else { format!("{} MW", num(p, 0)) }));
    push(&mut rows, "Turbines", turbines.map(|n| n.to_string()));
    push(&mut rows, "Foundation", t.str("foundation").map(|f| match f { "Grounded" => "fixed to the seabed".to_string(), "Floating" => "floating".to_string(), other => other.to_string() }));
    push(&mut rows, "Country", t.str("country").map(str::to_string));
    push(&mut rows, "Area", t.f64("area_km2").map(|a| format!("{} km²", num(a, 0))));
    push(&mut rows, "Distance from coast", t.f64("coast_distance_m").map(|d| format!("{} km", num(d / 1000.0, 0))));
    push(&mut rows, "Commissioned", t.str("year").map(str::to_string));
    push(&mut rows, "Register updated", t.str("updated_year").map(str::to_string));
    push(&mut rows, "Notes", t.str("notes").map(str::to_string));
    card.summary = match (status, power) {
        (Some(s), Some(p)) => Some(format!("{s}, {} MW{}.", num(p, 0), turbines.map(|n| format!(" from {n} turbines")).unwrap_or_default())),
        (Some(s), None) => Some(format!("{s}.")),
        _ => None,
    };
    card.sections.extend(section(None, rows));
    card
}

#[cfg(test)]
mod tests {
    use crate::{card, Subject};

    fn present(layer: &str, attrs: serde_json::Value, label: &str) -> crate::Card {
        card(Subject { layer_id: layer, kind: "x", key: "k", label: Some(label), attrs: &attrs })
    }

    fn value(c: &crate::Card, label: &str) -> String {
        c.sections.iter().flat_map(|s| &s.rows).find(|r| r.label == label).map(|r| r.value.clone()).unwrap_or_else(|| panic!("no row {label} in {c:#?}"))
    }

    #[test]
    fn a_buoy_reads_as_sea_conditions() {
        let c = present("buoys", serde_json::json!({"name": "WEST GULF - 207 NM East of Brownsville, TX", "station": "42002", "gust_ms": 3.0, "air_temp_c": 30.1, "dewpoint_c": 25.5, "pressure_hpa": 1017.6, "station_type": "3-meter foam buoy", "water_temp_c": 30.8, "wave_dir_deg": 185.0, "wind_dir_deg": 80.0, "wave_height_m": 0.6, "wind_speed_ms": 3.0, "average_period_s": 4.4, "dominant_period_s": 5.0}), "42002");
        assert_eq!(c.summary.as_deref(), Some("Wind 80° (E) at 3 m/s (6 kt), gusting 3 m/s (6 kt), waves 0.6 m every 5 s from S, sea 30.8 °C, air 30.1 °C."));
        assert_eq!(value(&c, "Pressure"), "1,017.6 hPa");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
    }

    #[test]
    fn a_bus_is_a_route_to_somewhere() {
        let c = present("buses", serde_json::json!({"line": "477", "block": "1701", "origin": "Home Gardens", "journey": "14", "vehicle": "SEN65", "operator": "KNCO", "direction": "inbound", "occupancy": "seatsAvailable", "destination": "Walnuts Centre", "aimed_arrival": "2026-09-16T12:35:00+00:00", "aimed_departure": "2026-09-16T11:45:00+00:00"}), "477");
        assert_eq!(c.title, "477 to Walnuts Centre");
        assert_eq!(value(&c, "Occupancy"), "seats available");
        assert_eq!(value(&c, "Scheduled arrival"), "16 Sep 12:35 UTC");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
    }

    #[test]
    fn a_meteor_names_its_shower_and_reads_its_orbit() {
        let c = present("meteors", serde_json::json!({"orbit": {"e": 0.734808, "a_au": 2.344417, "q_au": 0.62172, "i_deg": 139.164923, "node_deg": 168.992774, "peri_deg": 264.413905, "tisserand_j": 1.530987}, "shower": "SPE", "lat_end": 50.32, "lon_end": 3.38, "mass_kg": 0.0000758, "stations": ["BE0008", "FR000X"], "v_avg_kms": 57.7, "v_geo_kms": 60.14847, "duration_s": 0.21, "trajectory": "20260912022521_is3wK", "v_init_kms": 61.24, "peak_abs_mag": -2.03, "height_end_km": 96.5263, "shower_iau_no": 208, "station_count": 2, "fit_err_arcsec": 51.7, "peak_height_km": 100.1444, "convergence_deg": 60.39, "height_begin_km": 108.2893}), "SPE meteor");
        assert_eq!(c.title, "September epsilon Perseids meteor");
        assert_eq!(value(&c, "Mass"), "75.8 mg");
        assert_eq!(value(&c, "Inclination"), "139.2° — retrograde");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
    }

    #[test]
    fn a_flight_decodes_its_squawk_and_a_ground_station_its_pass() {
        let f = present("flights", serde_json::json!({"mlat": true, "squawk": "7700", "callsign": "N477BC", "on_ground": false, "type_code": "M20P", "registration": "N477BC", "position_age_s": 1.56, "baro_altitude_ft": 2100.0}), "N477BC");
        let squawk = f.sections[0].rows.iter().find(|r| r.label == "Squawk").unwrap();
        assert_eq!(squawk.note.as_deref(), Some("EMERGENCY"));
        assert_eq!(f.title, "N477BC");
        let g = present("ground-stations", serde_json::json!({"url": "https://network.satnogs.org/stations/5078/", "next": {"end": "2026-09-16T12:38:40Z", "mode": "BPSK PMT-A3", "start": "2026-09-16T12:29:16Z", "status": "future", "norad_id": 39086, "satellite": "SARAL", "transmitter": "ARGOS-3", "frequency_hz": 465988000, "observation_id": 14997254, "max_elevation_deg": 45.0}, "bands": ["UHF"], "stale": false, "status": "Online", "last_seen": "2026-09-16T12:14:47Z", "antennas": [{"band": "UHF", "type": "Ground Plane", "freq_max_hz": 470000000, "freq_min_hz": 400000000}], "station_id": 5078, "observations": 3614, "success_rate_pct": 79.0}), "dm43");
        let next = g.sections.iter().find(|s| s.heading.as_deref() == Some("Next pass")).unwrap();
        assert_eq!(next.rows[0].value, "SARAL (NORAD 39086)");
        assert_eq!(value(&g, "Frequency"), "465.988 MHz");
        assert_eq!(value(&g, "Antennas"), "Ground Plane, 400–470 MHz");
        assert!(g.links.iter().any(|l| l.url.contains("satnogs")));
    }

    #[test]
    fn an_opensky_flight_in_metres_reads_like_an_adsb_one_in_feet() {
        let c = present("flights", serde_json::json!({"mlat": false, "squawk": "7405", "callsign": "AAR202", "on_ground": false, "geo_altitude_m": 45.72, "origin_country": "Republic of Korea", "position_age_s": 7, "baro_altitude_m": 22.86}), "AAR202");
        assert_eq!(value(&c, "Altitude"), "75 ft (23 m)");
        assert_eq!(value(&c, "Registered in"), "Republic of Korea");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
    }

    #[test]
    fn a_discharge_event_is_told_apart_from_its_outfall() {
        let c = present("storm-overflows", serde_json::json!({"ended": "2026-09-16T15:06:26Z", "company": "South West Water", "ongoing": false, "started": "2026-09-16T15:05:26Z", "outfall_id": "SBB00284", "receiving_water": "COFTON STREAM(S)", "duration_minutes": 1}), "x");
        assert_eq!(c.title, "Discharged into COFTON STREAM(S)");
        assert_eq!(c.summary.as_deref(), Some("Untreated sewage was released from 16 Sep 15:05 UTC for 1 min."));
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
        let o = present("storm-overflows", serde_json::json!({"stale": false, "state": "not_discharging", "company": "United Utilities", "outfall_id": "UUP00001", "state_since": "2024-12-04T11:32:00Z", "record_updated": "2026-09-16T14:56:41Z", "receiving_water": "River Weaver"}), "UUP00001");
        assert_eq!(o.title, "River Weaver");
        assert!(o.summary.as_deref().unwrap().starts_with("Not discharging since"));
    }

    #[test]
    fn an_unknown_layer_still_reads_as_words_and_units() {
        let c = present("something-new", serde_json::json!({"wind_speed_kt": 16.0, "observed": "2026-09-16T12:00:00Z", "active": true, "note": "hello"}), "thing");
        assert_eq!(c.title, "thing");
        assert_eq!(value(&c, "Wind speed"), "16 kt");
        assert_eq!(value(&c, "Observed"), "16 Sep 12:00 UTC");
        assert_eq!(value(&c, "Active"), "yes");
    }

    #[test]
    fn an_attribute_a_presenter_does_not_know_is_shown_not_lost() {
        let c = present("buses", serde_json::json!({"line": "1", "destination": "Town", "brand_new_field_kt": 3.0}), "1");
        let also = c.sections.iter().find(|s| s.heading.as_deref() == Some("Also")).expect("an Also section");
        assert_eq!(also.rows[0].label, "Brand new field");
        assert_eq!(also.rows[0].value, "3 kt");
    }
}
