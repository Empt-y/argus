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


// --- Open-Meteo lattices ----------------------------------------------------

/// The European Air Quality Index bands, with what each means for a person.
fn aqi_words(aqi: f64) -> (&'static str, &'static str) {
    match aqi as i64 {
        i64::MIN..=20 => ("good", "air quality is satisfactory"),
        21..=40 => ("fair", "acceptable; a few sensitive people may notice"),
        41..=60 => ("moderate", "sensitive groups may feel effects"),
        61..=80 => ("poor", "health effects possible for everyone"),
        81..=100 => ("very poor", "health effects likely; reduce exertion outdoors"),
        _ => ("extremely poor", "avoid outdoor exertion"),
    }
}

fn pollen_words(grains: f64) -> &'static str {
    match grains as i64 {
        0 => "none",
        1..=19 => "low",
        20..=49 => "moderate",
        50..=149 => "high",
        _ => "very high",
    }
}

pub fn air_quality<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Air quality, modelled (CAMS via Open-Meteo)");
    let mut rows = Vec::new();
    if let Some(aqi) = t.f64("european_aqi") {
        let (band, meaning) = aqi_words(aqi);
        card.title = format!("Air quality {band} — index {}", num(aqi, 0));
        card.summary = Some(format!(
            "European air quality index {} ({band}): {meaning}. A model value for a 10 km cell, not a sensor reading.",
            num(aqi, 0)
        ));
        rows.push(row_note("Index", format!("{} — {band}", num(aqi, 0)), "European AQI, 0–20 good to over 100 extremely poor"));
    } else {
        card.title = "Air quality".into();
    }
    let ug = |v: f64| format!("{} µg/m³", num(v, 1));
    push(&mut rows, "PM2.5", t.f64("pm2_5_ugm3").map(ug));
    push(&mut rows, "PM10", t.f64("pm10_ugm3").map(ug));
    push(&mut rows, "Nitrogen dioxide", t.f64("no2_ugm3").map(ug));
    push(&mut rows, "Ozone", t.f64("ozone_ugm3").map(ug));
    push(&mut rows, "Sulphur dioxide", t.f64("so2_ugm3").map(ug));
    push(&mut rows, "Carbon monoxide", t.f64("co_ugm3").map(ug));
    push(&mut rows, "Ammonia", t.f64("nh3_ugm3").map(ug));
    push(&mut rows, "Dust", t.f64("dust_ugm3").map(ug));
    push(&mut rows, "UV index", t.f64("uv_index").map(|v| num(v, 1)));
    card.sections.extend(section(Some("Pollutants"), rows));

    let mut rows = Vec::new();
    for (key, name) in [
        ("grass_pollen_grains_m3", "Grass"),
        ("birch_pollen_grains_m3", "Birch"),
        ("alder_pollen_grains_m3", "Alder"),
        ("mugwort_pollen_grains_m3", "Mugwort"),
        ("olive_pollen_grains_m3", "Olive"),
        ("ragweed_pollen_grains_m3", "Ragweed"),
    ] {
        if let Some(g) = t.f64(key) {
            rows.push(row(name, format!("{} — {} grains/m³", pollen_words(g), num(g, 0))));
        }
    }
    card.sections.extend(section(Some("Pollen"), rows));

    let mut rows = Vec::new();
    push(&mut rows, "Model hour", t.str("model_time").and_then(when));
    push(&mut rows, "Sampled every", t.f64("lattice_spacing_deg").map(|d| format!("{}° — one point of a lattice over the area", num(d, 2))));
    push(&mut rows, "Cell elevation", t.f64("model_elevation_m").map(metres));
    t.skip("aqi_scale");
    card.sections.extend(section(Some("Model"), rows));
    card
}

pub fn sea_state<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Sea state, modelled (Open-Meteo marine)");
    let mut rows = Vec::new();
    let mut summary = Vec::new();
    match (t.f64("wave_height_m"), t.f64("wave_period_s"), t.f64("wave_dir_deg")) {
        (Some(h), period, dir) => {
            let mut text = format!("{} m", num(h, 1));
            if let Some(p) = period {
                text.push_str(&format!(" every {} s", num(p, 0)));
            }
            if let Some(d) = dir {
                text.push_str(&format!(" from {}", format::compass(d)));
            }
            card.title = format!("Waves {} m", num(h, 1));
            summary.push(format!("waves {text}"));
            rows.push(row("Waves", text));
        }
        _ => card.title = "Sea state".into(),
    }
    push(&mut rows, "Wind waves", t.f64("wind_wave_height_m").map(|h| format!("{} m", num(h, 1))));
    push(&mut rows, "Swell", t.f64("swell_wave_height_m").map(|h| format!("{} m", num(h, 1))));
    if let Some(v) = t.f64("sea_temp_c") {
        summary.push(format!("sea {}", celsius(v)));
        rows.push(row("Sea temperature", celsius(v)));
    }
    push(&mut rows, "Current", t.f64("current_speed_kmh").map(|v| format!("{} km/h ({} kt)", num(v, 1), num(v / 1.852, 1))));
    card.summary = (!summary.is_empty()).then(|| {
        let mut s = summary.join(", ");
        if let Some(f) = s.get(..1) {
            let up = f.to_uppercase();
            s.replace_range(..1, &up);
        }
        s + ". A wave model's value for its cell, not a buoy."
    });
    card.sections.extend(section(Some("Conditions"), rows));
    let mut rows = Vec::new();
    push(&mut rows, "Model time", t.str("model_time").and_then(when));
    push(&mut rows, "Sampled every", t.f64("lattice_spacing_deg").map(|d| format!("{}° — one point of a lattice over the area", num(d, 2))));
    card.sections.extend(section(Some("Model"), rows));
    card
}

// --- Raspberry Shake ---------------------------------------------------------

pub fn seismograph<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Citizen seismograph (Raspberry Shake)");
    let station = t.str("station").unwrap_or(subject.key).to_string();
    let model = t.str("model").unwrap_or("Raspberry Shake").to_string();
    card.title = format!("{model} {station}");
    let senses = t.strings("senses");
    card.summary = Some(match senses.len() {
        0 => format!("A {model} in the citizen seismic network."),
        _ => format!("A {model} recording {}.", senses.join(" and ")),
    });
    let mut rows = vec![row("Station", format!("AM.{station}"))];
    rows.push(row("Model", model));
    let channels = t.strings("channels");
    if !channels.is_empty() {
        rows.push(row_note(
            "Channels",
            channels.join(", "),
            "EH: geophone velocity, EN: accelerometer, HDF: infrasound; Z vertical, N north, E east",
        ));
    }
    push(&mut rows, "Sample rate", t.f64("sample_rate_hz").map(|r| format!("{} Hz", num(r, 0))));
    push(&mut rows, "Elevation", t.f64("elevation_m").map(metres));
    push(&mut rows, "Recording since", t.str("installed").and_then(when));
    t.skip("network");
    card.sections.extend(section(None, rows));
    if let Some(url) = t.str("url") {
        card.links.push(Link { label: "Live trace on StationView".into(), url: url.to_string() });
    }
    card
}

// --- data.police.uk ---------------------------------------------------------

fn crime_words(slug: &str) -> &str {
    match slug {
        "anti-social-behaviour" => "Anti-social behaviour",
        "bicycle-theft" => "Bicycle theft",
        "burglary" => "Burglary",
        "criminal-damage-arson" => "Criminal damage and arson",
        "drugs" => "Drugs",
        "other-theft" => "Other theft",
        "possession-of-weapons" => "Possession of weapons",
        "public-order" => "Public order",
        "robbery" => "Robbery",
        "shoplifting" => "Shoplifting",
        "theft-from-the-person" => "Theft from the person",
        "vehicle-crime" => "Vehicle crime",
        "violent-crime" => "Violence and sexual offences",
        "other-crime" => "Other crime",
        other => other,
    }
}

/// `2026-07` → `July 2026`.
fn month_words(ym: &str) -> String {
    let months = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
    match ym.split_once('-') {
        Some((y, m)) => match m.parse::<usize>() {
            Ok(m) if (1..=12).contains(&m) => format!("{} {y}", months[m - 1]),
            _ => ym.to_string(),
        },
        None => ym.to_string(),
    }
}

pub fn street_crime<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Recorded crime (data.police.uk)");
    let category = t.str("category").map(crime_words).unwrap_or("Recorded crime").to_string();
    let street = t.str("street").map(|s| s.strip_prefix("On or near ").unwrap_or(s).to_string());
    let month = t.str("month").map(month_words);
    card.title = match &street {
        Some(s) => format!("{category}, {s}"),
        None => category.clone(),
    };
    let reported_by = match t.str("location_type") {
        Some("BTP") => " to the British Transport Police",
        _ => "",
    };
    card.summary = Some(match &month {
        Some(m) => format!(
            "{category}, reported{reported_by} in {m}. The police publish the month only, and the point is snapped to a nearby street or place, not the address."
        ),
        None => format!("{category}, snapped to a nearby street or place, not the address."),
    });
    let mut rows = vec![row("Category", category)];
    push(&mut rows, "Month", month);
    if let Some(s) = street {
        rows.push(row_note("Near", s, "the anonymised point the police snapped this to"));
    }
    push(&mut rows, "Place type", t.str("location_subtype").map(str::to_string));
    match (t.str("outcome"), t.str("outcome_month")) {
        (Some(o), Some(m)) => rows.push(row("Outcome", format!("{o} ({})", month_words(m)))),
        (Some(o), None) => rows.push(row("Outcome", o)),
        (None, _) => rows.push(row_note("Outcome", "none recorded", "anti-social behaviour carries no outcome; other categories may not have one yet")),
    }
    push(&mut rows, "Context", t.str("context").map(str::to_string));
    push(&mut rows, "Reference", t.str("persistent_id").map(str::to_string));
    t.skip("street_id");
    t.skip("snapped");
    if t.str("location_type") == Some("BTP") {
        rows.push(row("Reported to", "British Transport Police"));
    }
    card.sections.extend(section(None, rows));
    card
}

// --- submarine cables ---------------------------------------------------------

pub fn cable<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Submarine cable");
    let name = t.str("name").unwrap_or(subject.key).to_string();
    let planned = t.bool("planned") == Some(true);
    card.title = name.clone();
    if planned {
        card.subtitle = Some("Submarine cable, planned".into());
    }
    let length = t.f64("length_km");
    let rfs = t.str("ready_for_service").map(str::to_string);
    let landings = t.value("landings").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let landing_names: Vec<String> = landings.iter().filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(str::to_string)).collect();
    let countries: Vec<String> = {
        let mut c: Vec<String> = landings.iter().filter_map(|l| l.get("country").and_then(|n| n.as_str()).map(str::to_string)).collect();
        c.sort();
        c.dedup();
        c
    };
    let mut summary = String::new();
    if let Some(km) = length {
        summary.push_str(&format!("{} km", num(km, 0)));
    }
    if !countries.is_empty() {
        if !summary.is_empty() {
            summary.push_str(", ");
        }
        summary.push_str(&format!("landing in {}", if countries.len() <= 4 { countries.join(", ") } else { format!("{} countries", countries.len()) }));
    }
    match (&rfs, planned) {
        (Some(r), true) => summary.push_str(&format!(", planned for {r}")),
        (Some(r), false) => summary.push_str(&format!(", in service since {r}")),
        _ => {}
    }
    if !summary.is_empty() {
        let mut s = summary;
        if let Some(f) = s.get(..1) {
            let up = f.to_uppercase();
            s.replace_range(..1, &up);
        }
        card.summary = Some(s + ". The route drawn is schematic, not the surveyed track on the seabed.");
    }
    let mut rows = Vec::new();
    push(&mut rows, "Length", length.map(|km| format!("{} km", num(km, 0))));
    t.skip("length_text");
    push(&mut rows, if planned { "Planned for" } else { "In service since" }, rfs);
    t.skip("ready_for_service_year");
    let owners = t.strings("owners");
    if !owners.is_empty() {
        rows.push(row("Owners", owners.join(", ")));
    }
    let suppliers = t.strings("suppliers");
    if !suppliers.is_empty() {
        rows.push(row("Built by", suppliers.join(", ")));
    }
    push(&mut rows, "Notes", t.str("notes").map(str::to_string));
    t.skip("cable_id");
    t.skip("map_color");
    t.skip("route");
    t.skip("landing_count");
    card.sections.extend(section(None, rows));
    if !landing_names.is_empty() {
        card.sections.extend(section(Some(&format!("Landings ({})", landing_names.len())), landing_names.into_iter().map(|n| row("", n)).collect()));
    }
    if let Some(url) = t.str("url") {
        card.links.push(Link { label: "Cable's own site".into(), url: url.to_string() });
    }
    card
}

pub fn cable_landing<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Submarine cable landing point");
    card.title = t.str("name").unwrap_or(subject.key).to_string();
    let cables = t.strings("cables");
    let planned = t.strings("planned_cables");
    t.skip("cable_count");
    t.skip("landing_id");
    card.summary = Some(match (cables.len(), planned.len()) {
        (0, 0) => "A landing point with no cable recorded against it.".to_string(),
        (n, 0) => format!("{n} cable{} come{} ashore here.", if n == 1 { "" } else { "s" }, if n == 1 { "s" } else { "" }),
        (0, p) => format!("{p} planned cable{} will land here.", if p == 1 { "" } else { "s" }),
        (n, p) => format!("{n} cable{} ashore here, {p} more planned.", if n == 1 { "" } else { "s" }),
    });
    if t.bool("location_tbd") == Some(true) {
        card.summary = Some(format!("{} The exact site is still to be decided.", card.summary.take().unwrap_or_default()));
    }
    if !cables.is_empty() {
        card.sections.extend(section(Some("Cables"), cables.into_iter().map(|c| row("", c)).collect()));
    }
    if !planned.is_empty() {
        card.sections.extend(section(Some("Planned"), planned.into_iter().map(|c| row("", c)).collect()));
    }
    card
}

// --- power grid ---------------------------------------------------------------

pub fn power<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let kind = t.str("kind").unwrap_or("");
    let mut card = base(
        subject,
        match kind {
            "line" => "Power line (OpenStreetMap)",
            "substation" => "Substation (OpenStreetMap)",
            "plant" => "Power plant (OpenStreetMap)",
            _ => "Power grid (OpenStreetMap)",
        },
    );
    let kv = t.f64("voltage_kv");
    let levels = t.value("voltages_kv").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_f64()).map(|x| num(x, 0)).collect::<Vec<_>>()).unwrap_or_default();
    let mut rows = Vec::new();
    push(&mut rows, "Name", t.str("name").map(str::to_string));
    match (kv, levels.len()) {
        (Some(_), n) if n > 1 => rows.push(row("Voltage", format!("{} kV", levels.join(" / ")))),
        (Some(kv), _) => rows.push(row("Voltage", format!("{} kV", num(kv, 0)))),
        _ => {}
    }
    match kind {
        "line" => {
            let circuits = t.i64("circuits");
            let cables = t.i64("cables");
            card.summary = Some(match (kv, circuits) {
                (Some(kv), Some(c)) => format!("A {} kV overhead line with {c} circuit{}.", num(kv, 0), if c == 1 { "" } else { "s" }),
                (Some(kv), None) => format!("A {} kV line.", num(kv, 0)),
                _ => "A power line.".to_string(),
            });
            push(&mut rows, "Circuits", circuits.map(|c| c.to_string()));
            push(&mut rows, "Conductors", cables.map(|c| c.to_string()));
            push(&mut rows, "Frequency", t.f64("frequency_hz").map(|f| format!("{} Hz", num(f, 0))));
            push(&mut rows, "Type", t.str("line_type").map(words));
            push(&mut rows, "Location", t.str("location").map(words));
        }
        "substation" => {
            let st = t.str("substation_type").map(words);
            card.summary = Some(match (&st, kv) {
                (Some(s), Some(kv)) => format!("A {} substation at {} kV.", s.to_lowercase(), num(kv, 0)),
                (Some(s), None) => format!("A {} substation.", s.to_lowercase()),
                (None, Some(kv)) => format!("A substation at {} kV.", num(kv, 0)),
                _ => "A substation.".to_string(),
            });
            push(&mut rows, "Type", st);
            push(&mut rows, "Owner", t.str("owner").map(str::to_string));
        }
        "plant" => {
            let source = t.str("source").map(words);
            let method = t.str("method").map(words);
            let mw = t.f64("output_mw");
            card.summary = Some(match (&source, mw) {
                (Some(s), Some(mw)) => format!("A {} plant with {} MW of electrical output.", s.to_lowercase(), num(mw, 1)),
                (Some(s), None) => format!("A {} plant.", s.to_lowercase()),
                (None, Some(mw)) => format!("A power plant with {} MW of electrical output.", num(mw, 1)),
                _ => "A power plant.".to_string(),
            });
            push(&mut rows, "Source", source);
            push(&mut rows, "Method", method);
            push(&mut rows, "Output", mw.map(|mw| format!("{} MW", num(mw, 1))));
            push(&mut rows, "Commissioned", t.str("start_date").map(str::to_string));
            push(&mut rows, "REPD id", t.str("repd_id").map(str::to_string));
        }
        _ => {}
    }
    push(&mut rows, "Operator", t.str("operator").map(str::to_string));
    push(&mut rows, "Reference", t.str("ref").map(str::to_string));
    card.sections.extend(section(None, rows));
    if let Some(osm) = t.str("osm") {
        card.links.push(Link { label: "On OpenStreetMap".into(), url: format!("https://www.openstreetmap.org/{osm}") });
    }
    card
}

// --- fires ------------------------------------------------------------------

pub fn fire<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Active fire detection (NASA FIRMS, VIIRS)");
    let frp = t.f64("frp_mw");
    let confidence = t.str("confidence");
    let (conf_words, conf_note) = match confidence {
        Some("h") => ("high", "a strong thermal signature; almost certainly a fire"),
        Some("n") => ("nominal", "the usual detection; a fire, a flare or something else hot"),
        Some("l") => ("low", "a weak signature; may be a hot surface rather than a flame"),
        _ => ("unknown", ""),
    };
    let daynight = t.str("daynight");
    let sat = match t.str("satellite") {
        // Rows written before the driver learned the file's own codes.
        Some("N20") => "NOAA-20".to_string(),
        Some("N21") => "NOAA-21".to_string(),
        Some("N") => "Suomi NPP".to_string(),
        Some(s) => s.to_string(),
        None => "VIIRS".to_string(),
    };
    let acquired = t.str("acquired").and_then(when);
    card.title = match (frp, confidence) {
        (Some(f), Some("l")) => format!("Hot spot, {} MW", num(f, 0)),
        (Some(f), _) => format!("Fire, {} MW", num(f, 0)),
        _ => "Fire detection".to_string(),
    };
    card.summary = Some(format!(
        "A 375 m pixel that {} saw burning{}{} with {conf_words} confidence. The satellite cannot tell a wildfire from a flare, a field or a furnace.",
        sat,
        acquired.as_deref().map(|a| format!(" at {a}")).unwrap_or_default(),
        frp.map(|f| format!(", radiating {} MW", num(f, 0))).unwrap_or_default()
    ));
    let mut rows = Vec::new();
    push(&mut rows, "Fire radiative power", frp.map(|f| format!("{} MW", num(f, 1))));
    if confidence.is_some() {
        rows.push(row_note("Confidence", conf_words, conf_note));
    }
    push(&mut rows, "Brightness (I-4)", t.f64("brightness_k").map(|k| format!("{} K", num(k, 1))));
    push(&mut rows, "Brightness (I-5)", t.f64("brightness_ti5_k").map(|k| format!("{} K", num(k, 1))));
    push(&mut rows, "Seen", acquired);
    push(
        &mut rows,
        "Pass",
        daynight.map(|d| match d {
            "D" => "daytime".to_string(),
            "N" => "night".to_string(),
            other => other.to_string(),
        }),
    );
    rows.push(row("Satellite", sat));
    push(&mut rows, "Instrument", t.str("instrument").map(str::to_string));
    if let (Some(s), Some(tr)) = (t.f64("scan_km"), t.f64("track_km")) {
        rows.push(row_note("Pixel", format!("{} × {} km", num(s, 2), num(tr, 2)), "the footprint of the detection, larger towards the edge of the swath"));
    }
    t.skip("version");
    card.sections.extend(section(None, rows));
    card
}

// --- fireballs ----------------------------------------------------------------

pub fn fireball<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Fireball (NASA/JPL CNEOS)");
    let kt = t.f64("impact_energy_kt");
    let alt = t.f64("peak_altitude_km");
    let vel = t.f64("velocity_kms");
    let detected = t.str("detected").and_then(when);
    let energy = kt.map(|kt| if kt >= 1.0 { format!("{} kilotons of TNT", num(kt, 1)) } else { format!("{} tons of TNT", num(kt * 1000.0, 0)) });
    card.title = match kt {
        Some(kt) if kt >= 1.0 => format!("Fireball, {} kt", num(kt, 1)),
        Some(kt) => format!("Fireball, {} t", num(kt * 1000.0, 0)),
        None => "Fireball".to_string(),
    };
    let mut summary = String::from("A bright meteor seen from orbit");
    if let Some(d) = &detected {
        summary.push_str(&format!(" at {d}"));
    }
    if let Some(e) = &energy {
        summary.push_str(&format!(", releasing the energy of {e}"));
    }
    if let Some(a) = alt {
        summary.push_str(&format!(", brightest at {} km up", num(a, 0)));
    }
    summary.push('.');
    if kt.is_some_and(|k| k >= 100.0) {
        summary.push_str(" Chelyabinsk-class.");
    }
    card.summary = Some(summary);
    let mut rows = Vec::new();
    push(&mut rows, "Detected", detected);
    if let Some(e) = energy {
        rows.push(row_note("Impact energy", e, "from the radiated energy, by the empirical scaling CNEOS uses"));
    }
    push(&mut rows, "Radiated energy", t.f64("radiated_energy_1e10_j").map(|e| format!("{} × 10¹⁰ J", num(e, 1))));
    push(&mut rows, "Peak brightness altitude", alt.map(|a| format!("{} km", num(a, 1))));
    push(&mut rows, "Entry speed", vel.map(|v| format!("{} km/s", num(v, 1))));
    if let Some(v) = t.value("velocity_ecef_kms").and_then(|v| v.as_array()) {
        let parts: Vec<String> = v.iter().filter_map(|x| x.as_f64()).map(|x| num(x, 1)).collect();
        if parts.len() == 3 {
            rows.push(row_note("Velocity (ECEF)", format!("{} km/s", parts.join(", ")), "x, y, z in the Earth-fixed frame"));
        }
    }
    card.sections.extend(section(None, rows));
    card.links.push(Link { label: "CNEOS fireball table".into(), url: "https://cneos.jpl.nasa.gov/fireballs/".into() });
    card
}

// --- airports -----------------------------------------------------------------

pub fn airport<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let kind = t.str("type").unwrap_or("");
    let kind_words = match kind {
        "large_airport" => "Large airport",
        "medium_airport" => "Medium airport",
        "small_airport" => "Small airfield",
        "heliport" => "Heliport",
        "seaplane_base" => "Seaplane base",
        "balloonport" => "Balloonport",
        "closed" => "Closed airfield",
        _ => "Airfield",
    };
    let mut card = base(subject, &format!("{kind_words} (OurAirports)"));
    let name = t.str("name").unwrap_or(subject.key).to_string();
    let icao = t.str("icao");
    let iata = t.str("iata");
    card.title = match (iata, icao) {
        (Some(a), Some(i)) => format!("{name} ({a} / {i})"),
        (Some(a), None) => format!("{name} ({a})"),
        (None, Some(i)) => format!("{name} ({i})"),
        (None, None) => name.clone(),
    };
    let closed = t.bool("closed") == Some(true);
    let scheduled = t.bool("scheduled_service") == Some(true);
    let town = t.str("municipality");
    let country = t.str("country");
    let runways = t.value("runways").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let longest = runways.iter().filter_map(|r| r.get("length_ft").and_then(|l| l.as_f64())).fold(None, |m: Option<f64>, l| Some(m.map_or(l, |m| m.max(l))));
    let mut summary = kind_words.to_string();
    if let (Some(tn), Some(c)) = (town, country) {
        summary.push_str(&format!(" at {tn}, {c}"));
    } else if let Some(c) = country {
        summary.push_str(&format!(" in {c}"));
    }
    match (runways.len(), longest) {
        (0, _) => {}
        (n, Some(l)) => summary.push_str(&format!(", {n} runway{}, the longest {}", if n == 1 { "" } else { "s" }, feet(l))),
        (n, None) => summary.push_str(&format!(", {n} runway{}", if n == 1 { "" } else { "s" })),
    }
    if scheduled {
        summary.push_str(", with scheduled airline service");
    }
    summary.push('.');
    if closed {
        summary.push_str(" Closed.");
    }
    card.summary = Some(summary);
    let mut rows = Vec::new();
    push(&mut rows, "ICAO", icao.map(str::to_string));
    push(&mut rows, "IATA", iata.map(str::to_string));
    push(&mut rows, "GPS code", t.str("gps_code").map(str::to_string));
    push(&mut rows, "Local code", t.str("local_code").map(str::to_string));
    push(&mut rows, "Elevation", t.f64("elevation_ft").map(feet));
    push(&mut rows, "Town", town.map(str::to_string));
    push(&mut rows, "Region", t.str("region").map(str::to_string));
    push(&mut rows, "Country", country.map(str::to_string));
    t.skip("ident");
    t.skip("runway_count");
    card.sections.extend(section(None, rows));
    if !runways.is_empty() {
        let rows: Vec<Row> = runways
            .iter()
            .map(|r| {
                let ident = r.get("ident").and_then(|v| v.as_str()).unwrap_or("?");
                let mut parts = Vec::new();
                if let Some(l) = r.get("length_ft").and_then(|v| v.as_f64()) {
                    parts.push(feet(l));
                }
                if let Some(w) = r.get("width_ft").and_then(|v| v.as_f64()) {
                    parts.push(format!("{} ft wide", num(w, 0)));
                }
                if let Some(s) = r.get("surface").and_then(|v| v.as_str()) {
                    parts.push(surface_words(s));
                }
                if r.get("lighted").and_then(|v| v.as_bool()) == Some(true) {
                    parts.push("lit".into());
                }
                if r.get("closed").and_then(|v| v.as_bool()) == Some(true) {
                    parts.push("closed".into());
                }
                let text = if parts.is_empty() { "—".to_string() } else { parts.join(", ") };
                match r.get("heading_deg").and_then(|v| v.as_f64()) {
                    Some(h) => row_note(ident, text, format!("true heading {}", bearing(h))),
                    None => row(ident, text),
                }
            })
            .collect();
        card.sections.extend(section(Some("Runways"), rows));
    }
    if let Some(u) = t.str("wikipedia") {
        card.links.push(Link { label: "Wikipedia".into(), url: u.to_string() });
    }
    if let Some(u) = t.str("url") {
        card.links.push(Link { label: "Airport site".into(), url: u.to_string() });
    }
    card
}

fn surface_words(code: &str) -> String {
    let c = code.to_uppercase();
    let word = match c.as_str() {
        "ASP" | "ASPH" | "ASPH-G" | "ASPHALT" => "asphalt",
        "CON" | "CONC" | "CONCRETE" => "concrete",
        "GRS" | "GRASS" | "TURF" | "TURF-G" => "grass",
        "GRE" | "GRVL" | "GRAVEL" => "gravel",
        "DIRT" | "DIRT-G" | "EARTH" => "dirt",
        "WATER" => "water",
        "SAND" => "sand",
        "PEM" => "asphalt over concrete",
        "UNK" | "" => return String::new(),
        _ => return code.to_lowercase(),
    };
    word.to_string()
}

// --- river discharge ------------------------------------------------------------

fn flow(v: f64) -> String {
    if v >= 10.0 { format!("{} m³/s", num(v, 0)) } else { format!("{} m³/s", num(v, 1)) }
}

pub fn river_discharge<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "River discharge, modelled (GloFAS via Open-Meteo)");
    let place = t.str("place").map(str::to_string);
    let rivers = t.strings("rivers");
    card.title = place.clone().or_else(|| rivers.first().cloned()).unwrap_or_else(|| "River discharge".into());
    let today = t.f64("discharge_m3s");
    let week_ago = t.f64("discharge_week_ago_m3s");
    let peak = t.f64("discharge_7d_peak_m3s");
    let series: Vec<f64> = t.value("discharge_7d_m3s").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default();
    let trend = match (today, week_ago) {
        (Some(t), Some(w)) if w > 0.0 && t > w * 1.5 => Some(format!("rising — {} a week ago", flow(w))),
        (Some(t), Some(w)) if w > 0.0 && t < w / 1.5 => Some(format!("falling — {} a week ago", flow(w))),
        (Some(_), Some(w)) => Some(format!("steady — {} a week ago", flow(w))),
        _ => None,
    };
    card.summary = today.map(|v| {
        let mut s = format!("Modelled flow of {} today", flow(v));
        if let Some(tr) = &trend {
            s.push_str(&format!(", {tr}"));
        }
        s.push_str(". A 5 km river-model cell, not the gauge's own reading.");
        s
    });
    let mut rows = Vec::new();
    push(&mut rows, "Discharge today", today.map(flow));
    push(&mut rows, "Trend", trend);
    push(&mut rows, "Peak this week", peak.map(flow));
    if series.len() >= 2 {
        rows.push(row_note("Last 7 days", series.iter().map(|v| if *v >= 10.0 { num(*v, 0) } else { num(*v, 1) }).collect::<Vec<_>>().join(" → "), "m³/s, oldest first"));
    }
    push(&mut rows, "Model day", t.str("model_day").map(str::to_string));
    if rivers.len() > 1 {
        rows.push(row("Rivers gauged here", rivers.join(", ")));
    }
    push(&mut rows, "Gauges in this cell", t.i64("gauges_in_cell").map(|n| n.to_string()));
    card.sections.extend(section(None, rows));
    card
}

// --- HF propagation ---------------------------------------------------------

pub fn hf_path<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "HF propagation path (WSPR)");
    let band = t.str("band").unwrap_or("HF").to_string();
    let tx = t.str("tx_callsign").map(str::to_string);
    let rx = t.str("rx_callsign").map(str::to_string);
    let tx_loc = t.str("tx_locator").unwrap_or("?").to_string();
    let rx_loc = t.str("rx_locator").unwrap_or("?").to_string();
    let km = t.f64("distance_km");
    let spots = t.i64("spots");
    let snr = t.f64("best_snr_db");
    let power = t.f64("tx_power_dbm");
    let window = t.i64("window_minutes").unwrap_or(10);
    card.title = format!("{band}: {} → {}", tx.clone().unwrap_or_else(|| tx_loc.clone()), rx.clone().unwrap_or_else(|| rx_loc.clone()));
    let mut summary = format!("A {band} beacon");
    if let Some(p) = power {
        summary.push_str(&format!(" at {}", dbm_words(p)));
    }
    if let Some(k) = km {
        summary.push_str(&format!(" heard {} km away", num(k, 0)));
    }
    match spots {
        Some(1) => summary.push_str(&format!(" once in the last {window} minutes")),
        Some(n) => summary.push_str(&format!(" {n} times in the last {window} minutes")),
        None => {}
    }
    if let Some(s) = snr {
        summary.push_str(&format!(", best signal {} dB", num(s, 0)));
    }
    summary.push_str(". The band is open along this path.");
    card.summary = Some(summary);
    let mut rows = vec![row("Band", band)];
    push(&mut rows, "Transmitter", tx.map(|c| format!("{c} ({tx_loc})")));
    push(&mut rows, "Receiver", rx.map(|c| format!("{c} ({rx_loc})")));
    push(&mut rows, "Distance", km.map(|k| format!("{} km", num(k, 0))));
    push(&mut rows, "Bearing", t.f64("bearing_deg").map(bearing));
    push(&mut rows, "Transmit power", power.map(dbm_words));
    if let Some(s) = snr {
        rows.push(row_note("Best signal", format!("{} dB", num(s, 0)), "signal to noise in 2.5 kHz; WSPR decodes down to about −30 dB"));
    }
    push(&mut rows, "Spots", spots.map(|n| format!("{n} in {window} min")));
    push(&mut rows, "Last heard", t.str("last_spot").and_then(when));
    t.skip("band_mhz");
    card.sections.extend(section(None, rows));
    card.links.push(Link { label: "wspr.live".into(), url: "https://wspr.live/".into() });
    card
}

/// `37 dBm` → `5 W`; `23 dBm` → `200 mW`.
fn dbm_words(dbm: f64) -> String {
    let watts = 10f64.powf((dbm - 30.0) / 10.0);
    if watts >= 1.0 {
        format!("{} W", num(watts, if watts >= 10.0 { 0 } else { 1 }))
    } else {
        format!("{} mW", num(watts * 1000.0, 0))
    }
}

// --- food hygiene ---------------------------------------------------------------

/// What a rating means, in the scheme's own words.
fn rating_words(rating: &Value) -> (String, Option<&'static str>) {
    match rating {
        Value::Number(n) => {
            let n = n.as_i64().unwrap_or(-1);
            let meaning = match n {
                5 => Some("very good"),
                4 => Some("good"),
                3 => Some("generally satisfactory"),
                2 => Some("improvement necessary"),
                1 => Some("major improvement necessary"),
                0 => Some("urgent improvement necessary"),
                _ => None,
            };
            (format!("{n} out of 5"), meaning)
        }
        Value::String(s) => (
            match s.as_str() {
                "pass" => "Pass".to_string(),
                "pass_and_eat_safe" => "Pass and Eat Safe".to_string(),
                "improvement_required" => "Improvement required".to_string(),
                "awaiting_inspection" => "Awaiting inspection".to_string(),
                "awaiting_publication" => "Awaiting publication".to_string(),
                "exempt" => "Exempt".to_string(),
                other => words(other),
            },
            None,
        ),
        _ => ("Unrated".to_string(), None),
    }
}

/// The scheme's fourteen business types as a phrase in a sentence.
fn business_words(t: &str) -> String {
    match t {
        "Restaurant/Cafe/Canteen" => "a restaurant, café or canteen".into(),
        "Retailers - other" => "a retailer".into(),
        "Retailers - supermarkets/hypermarkets" => "a supermarket".into(),
        "Other catering premises" => "catering premises".into(),
        "Takeaway/sandwich shop" => "a takeaway or sandwich shop".into(),
        "Pub/bar/nightclub" => "a pub, bar or nightclub".into(),
        "Hospitals/Childcare/Caring Premises" => "a hospital, childcare or care premises".into(),
        "School/college/university" => "a school, college or university".into(),
        "Mobile caterer" => "a mobile caterer".into(),
        "Hotel/bed & breakfast/guest house" => "a hotel, B&B or guest house".into(),
        "Manufacturers/packers" => "a manufacturer or packer".into(),
        "Distributors/Transporters" => "a distributor or transporter".into(),
        "Farmers/growers" => "a farm or grower".into(),
        "Importers/Exporters" => "an importer or exporter".into(),
        other => format!("a {}", other.to_lowercase()),
    }
}

/// `2026-03-12` as `12 Mar 2026`; a rating date has no time.
fn day_words(s: &str) -> Option<String> {
    chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok().map(|d| d.format("%-d %b %Y").to_string())
}

pub fn food_hygiene<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let scheme = t.str("scheme").unwrap_or("FHRS");
    let mut card = base(subject, &format!("Food hygiene rating ({scheme}, Food Standards Agency)"));
    let name = t.str("name").unwrap_or(subject.key).to_string();
    card.title = name.clone();
    let rating = t.value("rating").cloned().unwrap_or(Value::Null);
    let (rating_text, meaning) = rating_words(&rating);
    let rated = t.str("rating_date").and_then(day_words);
    let business_type = t.str("business_type").map(business_words);
    let authority = t.str("authority").map(str::to_string);
    let pending = t.bool("new_rating_pending") == Some(true);

    let by = authority.as_ref().map(|a| format!(" by {a}")).unwrap_or_default();
    let on = rated.as_ref().map(|d| format!(" on {d}")).unwrap_or_default();
    let mut summary = match (&rating, meaning) {
        (Value::Number(_), Some(m)) => format!("Rated {rating_text}, {m}{on}{by}"),
        (Value::String(s), _) if s.starts_with("awaiting") => format!("{rating_text}{by}, so no rating yet"),
        (Value::String(s), _) if s == "exempt" => format!("Exempt from rating{by}"),
        _ => format!("Rated {rating_text}{on}{by}"),
    };
    if let Some(b) = &business_type {
        summary.push_str(&format!("; {b}"));
    }
    summary.push('.');
    if pending {
        summary.push_str(" A new rating is pending publication.");
    }
    card.summary = Some(summary);

    let mut rows = vec![match meaning {
        Some(m) => row_note("Rating", rating_text, m),
        None => row("Rating", rating_text),
    }];
    push(&mut rows, "Rated", rated);
    push(&mut rows, "Business type", t.str("business_type").map(str::to_string));
    push(&mut rows, "Address", t.str("address").map(str::to_string));
    push(&mut rows, "Postcode", t.str("postcode").map(str::to_string));
    push(&mut rows, "Local authority", authority);
    if pending {
        rows.push(row("New rating", "pending publication"));
    }
    card.sections.extend(section(None, rows));

    // The three scores behind an FHRS rating, as the inspector marks them:
    // points lost, so 0 is clean and the worst possible is 25, 25 and 30.
    let mut scores = Vec::new();
    for (key, label, worst) in [("hygiene_points", "Hygiene", 25), ("structural_points", "Structural", 25), ("management_points", "Confidence in management", 30)] {
        if let Some(p) = t.i64(key) {
            scores.push(row_note(label, format!("{p} points lost"), format!("out of {worst}; 0 is best")));
        }
    }
    card.sections.extend(section(Some("Inspection scores"), scores));

    let fhrsid = t.str("fhrsid").map(str::to_string);
    t.skip("authority_code");
    if let Some(u) = t.str("url") {
        card.links.push(Link { label: "Rating on ratings.food.gov.uk".into(), url: u.to_string() });
    } else if let Some(id) = &fhrsid {
        card.links.push(Link { label: "Rating on ratings.food.gov.uk".into(), url: format!("https://ratings.food.gov.uk/business/{id}") });
    }
    if let Some(u) = t.str("authority_url") {
        card.links.push(Link { label: "Local authority".into(), url: u.to_string() });
    }
    card
}

// --- geomagnetic activity -------------------------------------------------------

/// The AuroraWatch levels in their own words, from `status-descriptions.xml`.
fn aurora_words(status: &str) -> (&'static str, &'static str) {
    match status {
        "green" => ("No significant activity", "Aurora is unlikely to be visible by eye or camera from anywhere in the UK."),
        "yellow" => ("Minor geomagnetic activity", "Aurora may be visible by eye from Scotland and by camera from Scotland, northern England and Northern Ireland."),
        "amber" => ("Amber alert: possible aurora", "Aurora is likely to be visible by eye from Scotland, northern England and Northern Ireland, possibly elsewhere in the UK; photographs are likely from anywhere in the UK."),
        "red" => ("Red alert: aurora likely", "Aurora is likely to be visible by eye and camera from anywhere in the UK."),
        _ => ("Unknown level", ""),
    }
}

pub fn geomagnetic<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let mut card = base(subject, "Magnetometer (AuroraWatch UK)");
    let location = t.str("location").map(|l| l.trim_end_matches(", UK").to_string()).unwrap_or_else(|| subject.key.to_string());
    let site = t.str("site").unwrap_or("").to_string();
    let nt = t.f64("activity_nt");
    let status = t.str("status").unwrap_or("").to_string();
    let hour = t.str("hour").and_then(when);
    let peak = t.f64("peak_24h_nt");
    let alerting = t.bool("alerting") == Some(true);
    let alert_level = t.str("alert_level").map(str::to_string);
    let until = t.str("until").map(str::to_string);

    card.title = if site.is_empty() { location.clone() } else { format!("{location} ({site})") };
    let (level_words, meaning) = aurora_words(&status);
    let mut summary = match nt {
        Some(v) => format!("Geomagnetic disturbance {} nT in the hour from {}: {}", num(v, 1), hour.clone().unwrap_or_else(|| "now".into()), level_words.to_lowercase()),
        None => level_words.to_string(),
    };
    if let Some(p) = peak.filter(|p| nt.is_some_and(|n| *p > n)) {
        summary.push_str(&format!(", peaking at {} nT in the last day", num(p, 1)));
    }
    summary.push('.');
    if alerting {
        let level = alert_level.clone().unwrap_or_else(|| status.clone());
        let (_, m) = aurora_words(&level);
        summary.push_str(&format!(" This is the instrument that sets the AuroraWatch UK alert, currently {level}. {m}"));
    } else if !meaning.is_empty() {
        summary.push_str(&format!(" {meaning}"));
    }
    if until.is_some() {
        summary.push_str(" The site is closed.");
    }
    card.summary = Some(summary);

    let mut rows = Vec::new();
    push(&mut rows, "Activity", nt.map(|v| format!("{} nT", num(v, 1))));
    rows.push(row_note("Level", status.clone(), level_words));
    push(&mut rows, "Hour", hour);
    push(&mut rows, "Peak, last 24 h", peak.map(|p| format!("{} nT", num(p, 1))));
    if alerting {
        push(&mut rows, "AuroraWatch UK alert", alert_level);
    }
    push(&mut rows, "Project", t.str("project").map(str::to_string));
    push(&mut rows, "About", t.str("description").map(str::to_string));
    push(&mut rows, "Since", t.str("since").and_then(|s| when(s).or_else(|| Some(s.to_string()))));
    push(&mut rows, "Closed", until.and_then(|s| when(&s).or(Some(s))));
    card.sections.extend(section(None, rows));

    if let Some(th) = t.value("thresholds_nt").and_then(|v| v.as_array()) {
        let rows: Vec<Row> = th
            .iter()
            .filter_map(|x| Some(row(x.get("status")?.as_str()?, format!("from {} nT", num(x.get("from_nt")?.as_f64()?, 0)))))
            .collect();
        card.sections.extend(section(Some("Alert thresholds"), rows));
    }
    if let Some(hours) = t.value("hours").and_then(|v| v.as_array()) {
        // Newest first, the way a person checks whether it is rising.
        let rows: Vec<Row> = hours
            .iter()
            .rev()
            .filter_map(|h| {
                let stamp = h.get("hour")?.as_str()?;
                let label = when(stamp).unwrap_or_else(|| stamp.to_string());
                Some(row(label, format!("{} nT, {}", num(h.get("nt")?.as_f64()?, 1), h.get("status")?.as_str()?)))
            })
            .collect();
        card.sections.extend(section(Some("Last 24 hours"), rows));
    }
    if let Some(u) = t.str("url") {
        card.links.push(Link { label: "Summary plots".into(), url: u.to_string() });
    }
    card.links.push(Link { label: "AuroraWatch UK".into(), url: "https://aurorawatch.lancs.ac.uk/".into() });
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

    #[test]
    fn an_air_cell_reads_as_a_band_and_says_it_is_a_model() {
        let c = present("air-quality", serde_json::json!({"european_aqi": 19.0, "pm10_ugm3": 7.1, "pm2_5_ugm3": 3.2, "no2_ugm3": 7.1, "ozone_ugm3": 58.0, "so2_ugm3": 0.7, "co_ugm3": 199.0, "nh3_ugm3": 2.3, "dust_ugm3": 0.0, "uv_index": 1.15, "grass_pollen_grains_m3": 0.1, "birch_pollen_grains_m3": 0.0, "alder_pollen_grains_m3": 0.0, "mugwort_pollen_grains_m3": 0.0, "olive_pollen_grains_m3": 0.0, "ragweed_pollen_grains_m3": 0.0, "model_time": "2026-09-16T15:00:00Z", "lattice_spacing_deg": 0.25, "model_elevation_m": 12.0}), "AQI 19 (good)");
        assert_eq!(c.title, "Air quality good — index 19");
        assert!(c.summary.as_deref().unwrap().contains("not a sensor reading"));
        assert_eq!(value(&c, "PM2.5"), "3.2 µg/m³");
        assert_eq!(value(&c, "Grass"), "none — 0 grains/m³");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
    }

    #[test]
    fn a_sea_cell_reads_like_a_buoy_but_says_it_is_not_one() {
        let c = present("sea-state", serde_json::json!({"wave_height_m": 0.52, "wave_dir_deg": 293.0, "wave_period_s": 3.05, "wind_wave_height_m": 0.4, "swell_wave_height_m": 0.3, "sea_temp_c": 19.5, "current_speed_kmh": 1.2, "model_time": "2026-09-16T15:45:00Z", "lattice_spacing_deg": 0.2}), "Waves 0.5 m");
        assert_eq!(c.title, "Waves 0.5 m");
        assert_eq!(c.summary.as_deref(), Some("Waves 0.5 m every 3 s from WNW, sea 19.5 °C. A wave model's value for its cell, not a buoy."));
        assert_eq!(value(&c, "Current"), "1.2 km/h (0.6 kt)");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
    }

    #[test]
    fn a_shake_names_its_model_and_what_it_hears() {
        let c = present("seismographs", serde_json::json!({"station": "R0A1B", "network": "AM", "model": "Raspberry Shake 4D", "senses": ["ground velocity", "ground acceleration"], "channels": ["EHZ", "ENE", "ENN", "ENZ"], "sample_rate_hz": 100.0, "elevation_m": 80.0, "installed": "2019-05-02T10:00:00Z", "url": "https://stationview.raspberryshake.org/#?net=AM&sta=R0A1B"}), "Raspberry Shake 4D R0A1B");
        assert_eq!(c.title, "Raspberry Shake 4D R0A1B");
        assert_eq!(c.summary.as_deref(), Some("A Raspberry Shake 4D recording ground velocity and ground acceleration."));
        assert_eq!(value(&c, "Recording since"), "2 May 2019 10:00 UTC");
        assert!(c.links.iter().any(|l| l.label.contains("StationView")));
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
    }

    #[test]
    fn a_crime_says_its_month_and_that_its_point_is_snapped() {
        let c = present("street-crime", serde_json::json!({"category": "bicycle-theft", "month": "2026-07", "street": "On or near Kings Cross", "street_id": 1682390, "location_type": "BTP", "location_subtype": "Station", "outcome": "Investigation complete; no suspect identified", "outcome_month": "2026-08", "persistent_id": "abc123", "snapped": true}), "Bicycle theft, Kings Cross");
        assert_eq!(c.title, "Bicycle theft, Kings Cross");
        assert_eq!(c.summary.as_deref(), Some("Bicycle theft, reported to the British Transport Police in July 2026. The police publish the month only, and the point is snapped to a nearby street or place, not the address."));
        assert_eq!(value(&c, "Outcome"), "Investigation complete; no suspect identified (August 2026)");
        assert_eq!(value(&c, "Near"), "Kings Cross");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
        let asb = present("street-crime", serde_json::json!({"category": "anti-social-behaviour", "month": "2026-07", "street": "On or near Earlstoke Street", "street_id": 1, "location_type": "Force", "snapped": true}), "x");
        assert_eq!(value(&asb, "Outcome"), "none recorded");
    }

    #[test]
    fn a_cable_reads_its_landings_and_says_the_route_is_schematic() {
        let c = present("submarine-cables", serde_json::json!({"cable_id": "2africa", "name": "2Africa", "map_color": "#939597", "route": "schematic", "length_km": 45000.0, "length_text": "45,000 km", "owners": ["Bayobab", "Meta"], "suppliers": ["ASN"], "ready_for_service": "2024", "ready_for_service_year": 2024, "planned": false, "url": "https://www.2africacable.net/", "landing_count": 2, "landings": [{"id": "luanda-angola", "name": "Luanda, Angola", "country": "Angola"}, {"id": "bude-united-kingdom", "name": "Bude, United Kingdom", "country": "United Kingdom"}]}), "2Africa");
        assert_eq!(c.title, "2Africa");
        assert_eq!(c.summary.as_deref(), Some("45,000 km, landing in Angola, United Kingdom, in service since 2024. The route drawn is schematic, not the surveyed track on the seabed."));
        assert_eq!(value(&c, "Owners"), "Bayobab, Meta");
        assert!(c.sections.iter().any(|s| s.heading.as_deref() == Some("Landings (2)")));
        assert!(c.links.iter().any(|l| l.label.contains("own site")));
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
        let l = present("cable-landings", serde_json::json!({"landing_id": "bude-united-kingdom", "name": "Bude, United Kingdom", "cables": ["2Africa", "Apollo"], "planned_cables": ["Amitié"], "cable_count": 3}), "Bude");
        assert_eq!(l.summary.as_deref(), Some("2 cables ashore here, 1 more planned."));
        assert!(l.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{l:#?}");
    }

    #[test]
    fn the_grid_reads_as_lines_substations_and_plants() {
        let line = present("power-grid", serde_json::json!({"osm": "way/1", "kind": "line", "name": "Bramley - Fleet", "operator": "National Grid", "voltage_kv": 400.0, "voltages_kv": [400.0, 275.0], "cables": 6, "circuits": 2}), "400 kV line: Bramley - Fleet");
        assert_eq!(line.summary.as_deref(), Some("A 400 kV overhead line with 2 circuits."));
        assert_eq!(value(&line, "Voltage"), "400 / 275 kV");
        assert!(line.links.iter().any(|l| l.url == "https://www.openstreetmap.org/way/1"));
        assert!(line.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{line:#?}");
        let plant = present("power-grid", serde_json::json!({"osm": "relation/6", "kind": "plant", "name": "Didcot B", "source": "gas", "method": "combustion", "output_mw": 2000.0, "repd_id": "123"}), "Didcot B");
        assert_eq!(plant.summary.as_deref(), Some("A gas plant with 2,000 MW of electrical output."));
        assert!(plant.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{plant:#?}");
        let sub = present("power-grid", serde_json::json!({"osm": "node/2", "kind": "substation", "name": "Bramley", "substation_type": "transmission", "voltage_kv": 400.0, "voltages_kv": [400.0, 132.0], "owner": "NGET"}), "Bramley (400 kV)");
        assert_eq!(sub.summary.as_deref(), Some("A transmission substation at 400 kV."));
        assert!(sub.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{sub:#?}");
    }

    #[test]
    fn a_fire_says_what_a_pixel_can_and_cannot_tell() {
        let c = present("fires", serde_json::json!({"satellite": "NOAA-20", "instrument": "VIIRS", "brightness_k": 367.0, "brightness_ti5_k": 300.1, "frp_mw": 45.8, "scan_km": 0.4, "track_km": 0.4, "confidence": "h", "daynight": "D", "version": "2.0NRT", "acquired": "2026-09-16T12:30:00Z"}), "Fire, 46 MW, high confidence");
        assert_eq!(c.title, "Fire, 46 MW");
        assert_eq!(c.summary.as_deref(), Some("A 375 m pixel that NOAA-20 saw burning at 16 Sep 12:30 UTC, radiating 46 MW with high confidence. The satellite cannot tell a wildfire from a flare, a field or a furnace."));
        assert_eq!(value(&c, "Pass"), "daytime");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
    }

    #[test]
    fn a_fireball_reads_its_energy_and_an_airport_its_runways() {
        let f = present("fireballs", serde_json::json!({"impact_energy_kt": 440.0, "radiated_energy_1e10_j": 375000.0, "peak_altitude_km": 23.3, "velocity_kms": 18.6, "velocity_ecef_kms": [12.8, -13.3, -2.4], "detected": "2013-02-15T03:20:33Z"}), "Fireball, 440.0 kt");
        assert_eq!(f.title, "Fireball, 440 kt");
        assert_eq!(f.summary.as_deref(), Some("A bright meteor seen from orbit at 15 Feb 2013 03:20 UTC, releasing the energy of 440 kilotons of TNT, brightest at 23 km up. Chelyabinsk-class."));
        assert!(f.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{f:#?}");
        let a = present("airports", serde_json::json!({"ident": "EGLL", "name": "London Heathrow Airport", "type": "large_airport", "icao": "EGLL", "iata": "LHR", "elevation_ft": 83.0, "country": "GB", "region": "GB-ENG", "municipality": "London", "scheduled_service": true, "url": "http://www.heathrowairport.com/", "wikipedia": "https://en.wikipedia.org/wiki/Heathrow_Airport", "runway_count": 2, "runways": [{"ident": "09L/27R", "length_ft": 12799.0, "width_ft": 164.0, "surface": "ASP", "heading_deg": 89.6, "lighted": true}, {"ident": "09R/27L", "length_ft": 12008.0, "width_ft": 164.0, "surface": "ASP", "heading_deg": 89.6, "lighted": true}]}), "London Heathrow Airport (LHR)");
        assert_eq!(a.title, "London Heathrow Airport (LHR / EGLL)");
        assert_eq!(a.summary.as_deref(), Some("Large airport at London, GB, 2 runways, the longest 12,799 ft (3,901 m), with scheduled airline service."));
        let rw = a.sections.iter().find(|s| s.heading.as_deref() == Some("Runways")).unwrap();
        assert_eq!(rw.rows[0].value, "12,799 ft (3,901 m), 164 ft wide, asphalt, lit");
        assert_eq!(rw.rows[0].note.as_deref(), Some("true heading 90° (E)"));
        assert_eq!(a.links.len(), 2);
        assert!(a.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{a:#?}");
        let closed = present("airports", serde_json::json!({"ident": "XXXX", "name": "Old Field", "type": "closed", "closed": true, "country": "GB"}), "Old Field");
        assert!(closed.summary.as_deref().unwrap().ends_with("Closed."));
    }

    #[test]
    fn a_river_cell_reads_its_flow_and_trend_and_names_the_river() {
        let c = present("river-discharge", serde_json::json!({"discharge_m3s": 0.34, "discharge_7d_m3s": [3.12, 1.67, 0.8, 0.53, 0.42, 0.76, 0.34], "discharge_week_ago_m3s": 3.12, "discharge_7d_peak_m3s": 3.12, "model_day": "2026-09-16", "rivers": ["River Thames", "River Crane"], "place": "River Thames at Kingston upon Thames", "gauges_in_cell": 3}), "x");
        assert_eq!(c.title, "River Thames at Kingston upon Thames");
        assert_eq!(c.summary.as_deref(), Some("Modelled flow of 0.3 m³/s today, falling — 3.1 m³/s a week ago. A 5 km river-model cell, not the gauge's own reading."));
        assert_eq!(value(&c, "Last 7 days"), "3.1 → 1.7 → 0.8 → 0.5 → 0.4 → 0.8 → 0.3");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
    }

    #[test]
    fn a_propagation_path_reads_as_a_beacon_heard() {
        let c = present("hf-propagation", serde_json::json!({"band": "40 m", "band_mhz": 7, "tx_callsign": "G4LRP", "rx_callsign": "OE3GBB", "tx_locator": "IO91ta", "rx_locator": "JN87aq", "distance_km": 1242.0, "bearing_deg": 104.0, "spots": 3, "best_snr_db": -18.0, "tx_power_dbm": 23.0, "last_spot": "2026-09-16T18:28:00Z", "window_minutes": 10}), "x");
        assert_eq!(c.title, "40 m: G4LRP → OE3GBB");
        assert_eq!(c.summary.as_deref(), Some("A 40 m beacon at 200 mW heard 1,242 km away 3 times in the last 10 minutes, best signal -18 dB. The band is open along this path."));
        assert_eq!(value(&c, "Transmit power"), "200 mW");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");
        assert_eq!(super::dbm_words(37.0), "5 W");
        assert_eq!(super::dbm_words(40.0), "10 W");
    }

    #[test]
    fn a_food_business_reads_as_its_rating_and_what_it_means() {
        let c = present("food-hygiene", serde_json::json!({"name": "THE CROWN", "business_type": "Pub/bar/nightclub", "scheme": "FHRS", "rating": 3, "rating_date": "2026-03-12", "new_rating_pending": true, "hygiene_points": 10, "structural_points": 5, "management_points": 10, "address": "The Crown, 1 High Street, Birmingham", "postcode": "B1 1AA", "authority": "Birmingham", "authority_code": "402", "authority_url": "http://www.birmingham.gov.uk", "fhrsid": "55", "url": "https://ratings.food.gov.uk/business/55"}), "THE CROWN");
        assert_eq!(c.title, "THE CROWN");
        assert_eq!(c.summary.as_deref(), Some("Rated 3 out of 5, generally satisfactory on 12 Mar 2026 by Birmingham; a pub, bar or nightclub. A new rating is pending publication."));
        assert_eq!(value(&c, "Rating"), "3 out of 5");
        assert_eq!(value(&c, "Hygiene"), "10 points lost");
        assert_eq!(c.links[0].url, "https://ratings.food.gov.uk/business/55");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");

        let scottish = present("food-hygiene", serde_json::json!({"name": "MOROCCAN MARKET", "business_type": "Retailers - other", "scheme": "FHIS", "rating": "improvement_required", "rating_date": "2023-07-28", "address": "George Street, Aberdeen", "postcode": "AB25 1HZ", "authority": "Aberdeen City", "authority_code": "760", "authority_url": "http://www.aberdeencity.gov.uk", "fhrsid": "1608170", "url": "https://ratings.food.gov.uk/business/1608170"}), "MOROCCAN MARKET");
        assert_eq!(scottish.summary.as_deref(), Some("Rated Improvement required on 28 Jul 2023 by Aberdeen City; a retailer."));
        assert!(scottish.sections.iter().all(|s| s.heading.as_deref() != Some("Inspection scores")), "FHIS publishes no scores");

        let waiting = present("food-hygiene", serde_json::json!({"name": "CAFFI CYMRU", "scheme": "FHRS", "rating": "awaiting_inspection", "authority": "Gwynedd", "fhrsid": "56", "url": "https://ratings.food.gov.uk/business/56"}), "CAFFI CYMRU");
        assert_eq!(waiting.summary.as_deref(), Some("Awaiting inspection by Gwynedd, so no rating yet."));
    }

    #[test]
    fn the_alerting_magnetometer_reads_as_the_national_aurora_alert() {
        let c = present("geomagnetic-activity", serde_json::json!({"site": "SUM", "location": "Sumburgh Head, UK", "project": "AWN", "description": "Raspberry Pi magnetometer system.", "since": "2017-08-01T00:00", "activity_nt": 33.7, "hour": "2026-09-17T11:00:00Z", "status": "green", "peak_24h_nt": 78.6, "alerting": true, "alert_level": "green", "thresholds_nt": [{"status": "green", "from_nt": 0.0}, {"status": "yellow", "from_nt": 50.0}], "hours": [{"hour": "2026-09-17T10:00:00Z", "nt": 78.6, "status": "yellow"}, {"hour": "2026-09-17T11:00:00Z", "nt": 33.7, "status": "green"}], "url": "https://aurorawatch.lancs.ac.uk/summary/awn/sum/"}), "Sumburgh Head");
        assert_eq!(c.title, "Sumburgh Head (SUM)");
        assert_eq!(c.summary.as_deref(), Some("Geomagnetic disturbance 33.7 nT in the hour from 17 Sep 11:00 UTC: no significant activity, peaking at 78.6 nT in the last day. This is the instrument that sets the AuroraWatch UK alert, currently green. Aurora is unlikely to be visible by eye or camera from anywhere in the UK."));
        assert_eq!(value(&c, "AuroraWatch UK alert"), "green");
        assert_eq!(value(&c, "yellow"), "from 50 nT");
        let last = c.sections.iter().find(|s| s.heading.as_deref() == Some("Last 24 hours")).unwrap();
        assert_eq!(last.rows[0].value, "33.7 nT, green", "newest first");
        assert!(c.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{c:#?}");

        let closed = present("geomagnetic-activity", serde_json::json!({"site": "SID", "location": "Sidmouth, UK", "project": "AWN", "activity_nt": 13.7, "hour": "2018-08-12T09:00:00Z", "status": "green", "peak_24h_nt": 13.7, "until": "2018-09-01T00:00", "hours": []}), "Sidmouth");
        assert!(closed.summary.as_deref().unwrap().ends_with("The site is closed."), "{:?}", closed.summary);
        assert!(closed.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{closed:#?}");
    }
}
