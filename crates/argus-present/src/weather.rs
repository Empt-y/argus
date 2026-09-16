//! Aviation weather in words: METARs, TAFs and SIGMETs.
//!
//! A METAR is a terse code — `24016G30KT 10SM -RA BR BKN038 11/06 A2994` —
//! designed to be read by pilots who learned it and machines that parse it.
//! The driver already stores the decoded fields it gets from the Aviation
//! Weather Center; this turns them into sentences, and decodes the parts
//! the centre leaves as codes: present weather (`-RA BR` is light rain and
//! mist), cloud cover words, flight categories, and the TAF, which arrives
//! as raw text only and is parsed here into forecast periods.

use crate::format::{self, celsius, feet, knots, num, when};
use crate::util::{push, row, row_note, section, Take};
use crate::{Card, Link, Subject};
use serde_json::{Map, Value};

// --- present weather -------------------------------------------------------

/// A weather group, `-SHRA` or `VCTS` or `BR`, in words.
pub fn weather_group(code: &str) -> String {
    let code = code.trim();
    let mut rest = code;
    let mut intensity = "";
    if let Some(r) = rest.strip_prefix('-') {
        intensity = "light ";
        rest = r;
    } else if let Some(r) = rest.strip_prefix('+') {
        intensity = "heavy ";
        rest = r;
    }
    let mut vicinity = false;
    if let Some(r) = rest.strip_prefix("VC") {
        vicinity = true;
        rest = r;
    }
    let mut descriptor: Option<&str> = None;
    let mut phenomena: Vec<&str> = Vec::new();
    let mut i = 0;
    let bytes = rest.as_bytes();
    while i + 2 <= bytes.len() {
        let part = &rest[i..i + 2];
        i += 2;
        match part {
            "MI" => descriptor = Some("shallow"),
            "BC" => descriptor = Some("patches of"),
            "PR" => descriptor = Some("partial"),
            "DR" => descriptor = Some("drifting"),
            "BL" => descriptor = Some("blowing"),
            "SH" => descriptor = Some("showers of"),
            "TS" => descriptor = Some("thunderstorm with"),
            "FZ" => descriptor = Some("freezing"),
            "DZ" => phenomena.push("drizzle"),
            "RA" => phenomena.push("rain"),
            "SN" => phenomena.push("snow"),
            "SG" => phenomena.push("snow grains"),
            "IC" => phenomena.push("ice crystals"),
            "PL" => phenomena.push("ice pellets"),
            "GR" => phenomena.push("hail"),
            "GS" => phenomena.push("small hail"),
            "UP" => phenomena.push("unknown precipitation"),
            "BR" => phenomena.push("mist"),
            "FG" => phenomena.push("fog"),
            "FU" => phenomena.push("smoke"),
            "VA" => phenomena.push("volcanic ash"),
            "DU" => phenomena.push("dust"),
            "SA" => phenomena.push("sand"),
            "HZ" => phenomena.push("haze"),
            "PY" => phenomena.push("spray"),
            "PO" => phenomena.push("dust whirls"),
            "SQ" => phenomena.push("squalls"),
            "FC" => phenomena.push(if intensity == "heavy " { "tornado" } else { "funnel cloud" }),
            "SS" => phenomena.push("sandstorm"),
            "DS" => phenomena.push("dust storm"),
            "NS" if rest.starts_with("NSW") => return "no significant weather".into(),
            other => phenomena.push(other),
        }
    }
    let mut out = String::new();
    if intensity == "heavy " && phenomena == ["tornado"] {
        // "+FC" is a tornado, not "heavy funnel cloud".
    } else {
        out.push_str(intensity);
    }
    match descriptor {
        Some("thunderstorm with") if phenomena.is_empty() => out.push_str("thunderstorm"),
        Some("showers of") if phenomena.is_empty() => out.push_str("showers"),
        Some(d) => {
            out.push_str(d);
            out.push(' ');
            out.push_str(&phenomena.join(" and "));
        }
        None => out.push_str(&phenomena.join(" and ")),
    }
    if vicinity {
        out.push_str(" in the vicinity");
    }
    out.trim().to_string()
}

/// A whole `wx_string`: `-RA BR` → `light rain, mist`.
pub fn weather(wx: &str) -> String {
    wx.split_whitespace()
        .map(weather_group)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn cover(code: &str) -> &'static str {
    match code {
        "FEW" => "few clouds",
        "SCT" => "scattered clouds",
        "BKN" => "broken clouds",
        "OVC" => "overcast",
        "OVX" | "VV" => "sky obscured",
        "CLR" | "SKC" => "clear sky",
        "NSC" => "no significant cloud",
        "NCD" => "no cloud detected",
        "CAVOK" => "ceiling and visibility OK",
        _ => "cloud",
    }
}

fn cloud_type(code: &str) -> Option<&'static str> {
    match code {
        "CB" => Some("cumulonimbus"),
        "TCU" => Some("towering cumulus"),
        _ => None,
    }
}

pub fn flight_category(code: &str) -> (&'static str, &'static str) {
    match code {
        "VFR" => ("VFR", "visual flight rules — ceiling above 3,000 ft and visibility over 5 miles"),
        "MVFR" => ("Marginal VFR", "ceiling 1,000–3,000 ft or visibility 3–5 miles"),
        "IFR" => ("IFR", "instrument flight rules — ceiling 500–1,000 ft or visibility 1–3 miles"),
        "LIFR" => ("Low IFR", "ceiling below 500 ft or visibility under a mile"),
        _ => ("Unknown", ""),
    }
}

/// Relative humidity from temperature and dew point (Magnus).
fn humidity(t: f64, td: f64) -> f64 {
    let e = |x: f64| (17.625 * x / (243.04 + x)).exp();
    (100.0 * e(td) / e(t)).clamp(0.0, 100.0)
}

fn airport_name(name: &str) -> String {
    name.replace(" Arpt", " Airport")
        .replace(" Intl", " International")
        .replace(" Rgnl", " Regional")
        .replace(" Muni", " Municipal")
        .replace(" Fld", " Field")
}

// --- METAR -----------------------------------------------------------------

pub fn metar<'a>(subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let station = t.str("station").unwrap_or(subject.key).to_string();
    let name = t.str("name").map(airport_name);
    let title = match &name {
        Some(n) => format!("{n} ({station})"),
        None => station.clone(),
    };

    // Conditions.
    let mut rows = Vec::new();
    let mut summary: Vec<String> = Vec::new();

    let wind_dir = t.f64("wind_dir_deg");
    let wind_kt = t.f64("wind_speed_kt");
    let gust = t.f64("gust_kt");
    let wind = match (wind_dir, wind_kt) {
        (_, Some(0.0)) => Some("calm".to_string()),
        (Some(0.0), Some(kt)) => Some(format!("variable at {}", knots(kt))),
        (Some(d), Some(kt)) => Some(match gust {
            Some(g) => format!("{} at {} gusting {} kt", format::bearing(d), num(kt, 0), num(g, 0)),
            None => format!("{} at {}", format::bearing(d), knots(kt)),
        }),
        (None, Some(kt)) => Some(knots(kt)),
        _ => None,
    };
    if let Some(w) = &wind {
        rows.push(row("Wind", w.clone()));
        summary.push(format!("Wind {w}"));
    }

    let vis = t.f64("visibility_mi");
    let or_more = t.bool("visibility_or_more").unwrap_or(false);
    if let Some(mi) = vis {
        let text = if or_more {
            format!("{} miles or more ({} km+)", num(mi, 2), num(mi * 1.609, 0))
        } else {
            format!("{} miles ({} km)", num(mi, 2), num(mi * 1.609, 1))
        };
        rows.push(row("Visibility", text.clone()));
        summary.push(format!("visibility {text}"));
    }

    if let Some(wx) = t.str("weather") {
        let text = weather(wx);
        rows.push(row_note("Weather", text.clone(), wx));
        summary.push(text);
    }

    if let Some(clouds) = t.value("clouds").and_then(Value::as_array) {
        let mut parts = Vec::new();
        for layer in clouds {
            let code = layer.get("cover").and_then(Value::as_str).unwrap_or("");
            let base = layer.get("base_ft").and_then(Value::as_f64);
            let text = match (code, base) {
                ("OVX", Some(b)) => format!("sky obscured, vertical visibility {}", feet(b)),
                (c, Some(b)) => format!("{} at {}", cover(c), feet(b)),
                (c, None) => cover(c).to_string(),
            };
            parts.push(text);
        }
        if !parts.is_empty() {
            rows.push(row("Clouds", parts.join("; ")));
            summary.push(parts.join(", "));
        }
    } else {
        t.skip("clouds");
    }

    let temp = t.f64("temp_c");
    let dew = t.f64("dewpoint_c");
    if let Some(tc) = temp {
        rows.push(row("Temperature", format!("{} ({} °F)", celsius(tc), num(tc * 1.8 + 32.0, 0))));
        summary.push(celsius(tc));
    }
    if let Some(d) = dew {
        rows.push(row("Dew point", celsius(d)));
    }
    if let (Some(tc), Some(d)) = (temp, dew) {
        rows.push(row("Humidity", format!("{}%", num(humidity(tc, d), 0))));
    }
    if let Some(a) = t.f64("altimeter_inhg") {
        rows.push(row("Altimeter", format!("{} inHg ({} hPa)", num(a, 2), num(a * 33.8639, 0))));
    }
    push(&mut rows, "Sea-level pressure", t.f64("sea_level_pressure_hpa").map(|p| format!("{} hPa", num(p, 1))));
    push(
        &mut rows,
        "Pressure tendency",
        t.f64("pressure_tendency_hpa").map(|p| format!("{}{} hPa over 3 h", if p > 0.0 { "+" } else { "" }, num(p, 1))),
    );
    for (key, label) in [
        ("precip_in", "Precipitation, last hour"),
        ("precip_3h_in", "Precipitation, 3 h"),
        ("precip_6h_in", "Precipitation, 6 h"),
        ("precip_24h_in", "Precipitation, 24 h"),
        ("snow_in", "Snow depth"),
    ] {
        push(&mut rows, label, t.f64(key).map(|v| format!("{} in ({} mm)", num(v, 2), num(v * 25.4, 1))));
    }
    let category = t.str("flight_category");
    if let Some(c) = category {
        let (name, meaning) = flight_category(c);
        rows.push(row_note("Flight category", name, meaning));
    }

    let mut card = Card {
        title,
        subtitle: Some("Aerodrome weather report".into()),
        summary: (!summary.is_empty()).then(|| {
            let mut s = summary.join(", ");
            if let Some(c) = category {
                s.push_str(&format!(". {}.", flight_category(c).0));
            } else {
                s.push('.');
            }
            s
        }),
        sections: Vec::new(),
        links: Vec::new(),
    };
    card.sections.extend(section(Some("Conditions"), rows));

    // The forecast.
    let taf_text = t.str("taf");
    let issued = t.str("taf_issued");
    let valid_from = t.str("taf_valid_from");
    let valid_to = t.str("taf_valid_to");
    if let Some(taf) = taf_text {
        let mut rows = Vec::new();
        if let Some(v) = format::span(valid_from, valid_to) {
            rows.push(row("Valid", v));
        }
        push(&mut rows, "Issued", issued.and_then(when));
        let base = issued.and_then(format::parse_time);
        for period in decode_taf(taf, base) {
            rows.push(row(period.0, period.1));
        }
        card.sections.extend(section(Some("Forecast (TAF)"), rows));
    }

    // About the report.
    let mut rows = Vec::new();
    push(&mut rows, "Report", t.str("report_type").map(|r| match r {
        "SPECI" => "SPECI — a special report, issued because conditions changed".to_string(),
        other => other.to_string(),
    }));
    if t.bool("auto") == Some(true) {
        rows.push(row("Observed by", "automated station, no human observer"));
    }
    if t.bool("corrected") == Some(true) {
        rows.push(row("Corrected", "yes — this report replaces an earlier one"));
    }
    if t.bool("maintenance") == Some(true) {
        rows.push(row("Maintenance", "the station has flagged itself for maintenance ($)"));
    }
    if t.bool("nosig") == Some(true) {
        rows.push(row("Trend", "no significant change expected in the next two hours (NOSIG)"));
    }
    let off = t.strings("sensors_off");
    if !off.is_empty() {
        rows.push(row("Sensors off", off.iter().map(|s| s.replace('_', " ")).collect::<Vec<_>>().join(", ")));
    }
    card.sections.extend(section(Some("Report"), rows));

    // The station.
    let mut rows = vec![row("ICAO", station.clone())];
    push(&mut rows, "IATA", t.str("iata").map(str::to_string));
    push(&mut rows, "Country", t.str("country").map(str::to_string));
    push(&mut rows, "Elevation", t.f64("elevation_m").map(|m| format!("{} m ({} ft)", num(m, 0), num(m / 0.3048, 0))));
    card.sections.extend(section(Some("Station"), rows));

    // The raw text, last, for people who read it.
    let mut rows = Vec::new();
    push(&mut rows, "METAR", t.str("raw").map(str::to_string));
    push(&mut rows, "TAF", taf_text.map(str::to_string));
    card.sections.extend(section(Some("Raw"), rows));

    card.links.push(Link {
        label: "Aviation Weather Center".into(),
        url: format!("https://aviationweather.gov/data/metar/?id={station}&hours=12&decoded=yes&include_taf=yes"),
    });
    card
}

// --- TAF -------------------------------------------------------------------

/// One forecast period: when, and what.
type Period = (String, String);

/// Decode a TAF into periods. `base` is when it was issued, for the month
/// and year that the `ddhh` day-hour groups leave out.
pub fn decode_taf(taf: &str, base: Option<chrono::DateTime<chrono::Utc>>) -> Vec<Period> {
    let tokens: Vec<&str> = taf.split_whitespace().collect();
    let mut i = 0;
    // Header: TAF [AMD|COR] ICAO ddhhmmZ dddd/dddd
    while i < tokens.len() && (tokens[i] == "TAF" || tokens[i] == "AMD" || tokens[i] == "COR") {
        i += 1;
    }
    let mut amended = taf.starts_with("TAF AMD");
    if taf.starts_with("TAF COR") {
        amended = true;
    }
    i += 1; // ICAO
    if i < tokens.len() && tokens[i].ends_with('Z') {
        i += 1;
    }
    let mut base_valid: Option<String> = None;
    if i < tokens.len() && is_validity(tokens[i]) {
        base_valid = Some(validity(tokens[i], base));
        i += 1;
    }

    let mut periods: Vec<Period> = Vec::new();
    let mut when_label = match base_valid {
        Some(v) => format!("From {v}"),
        None => "Forecast".to_string(),
    };
    if amended {
        when_label.push_str(" (amended)");
    }
    let mut current: Vec<String> = Vec::new();

    let flush = |periods: &mut Vec<Period>, label: &str, parts: &mut Vec<String>| {
        if !parts.is_empty() {
            periods.push((label.to_string(), parts.join(", ")));
            parts.clear();
        }
    };

    let mut prob: Option<&str> = None;
    // TX/TN are the forecast's extremes for the whole validity, and UK TAFs
    // put them last, after the change groups; they belong on the first
    // period, not on whichever BECMG happened to come before them.
    let mut extremes: Vec<String> = Vec::new();
    while i < tokens.len() {
        let tok = tokens[i];
        i += 1;
        if tok == "RMK" {
            break;
        }
        if (tok.starts_with("TX") || tok.starts_with("TN"))
            && tok.contains('/')
            && let Some(e) = taf_element(tok, None, base)
        {
            extremes.push(e.text);
            continue;
        }
        if let Some(fm) = tok.strip_prefix("FM")
            && fm.len() == 6
            && fm.chars().all(|c| c.is_ascii_digit())
        {
            flush(&mut periods, &when_label, &mut current);
            when_label = format!("From {}", day_hour_minute(fm, base));
            prob = None;
            continue;
        }
        if tok == "BECMG" || tok == "TEMPO" || tok == "INTER" {
            flush(&mut periods, &when_label, &mut current);
            let window = if i < tokens.len() && is_validity(tokens[i]) {
                let w = validity(tokens[i], base);
                i += 1;
                w
            } else {
                String::new()
            };
            let verb = match tok {
                "BECMG" => "Becoming",
                "TEMPO" => "Temporarily",
                _ => "Intermittently",
            };
            when_label = match prob.take() {
                Some(p) => format!("{verb} {window} ({p}% chance)"),
                None => format!("{verb} {window}"),
            };
            continue;
        }
        if let Some(p) = tok.strip_prefix("PROB") {
            flush(&mut periods, &when_label, &mut current);
            // PROB30 alone starts a period; PROB30 TEMPO is handled above.
            if i < tokens.len() && (tokens[i] == "TEMPO" || tokens[i] == "INTER") {
                prob = Some(p);
            } else if i < tokens.len() && is_validity(tokens[i]) {
                when_label = format!("{p}% chance {}", validity(tokens[i], base));
                i += 1;
            }
            continue;
        }
        // Elements within a period.
        if let Some(text) = taf_element(tok, tokens.get(i).copied(), base) {
            if text.consumed_next {
                i += 1;
            }
            current.push(text.text);
        }
    }
    flush(&mut periods, &when_label, &mut current);
    if !extremes.is_empty() {
        match periods.first_mut() {
            Some(first) => {
                first.1.push_str(", ");
                first.1.push_str(&extremes.join(", "));
            }
            None => periods.push((when_label, extremes.join(", "))),
        }
    }
    periods
}

struct Element {
    text: String,
    consumed_next: bool,
}

fn taf_element(tok: &str, next: Option<&str>, base: Option<chrono::DateTime<chrono::Utc>>) -> Option<Element> {
    let plain = |s: String| Some(Element { text: s, consumed_next: false });
    // Wind: dddffKT, dddffGggKT, VRBffKT, or MPS.
    if let Some(w) = wind_group(tok) {
        return plain(w);
    }
    // Visibility.
    if tok == "CAVOK" {
        return plain("ceiling and visibility OK".into());
    }
    if tok == "9999" {
        return plain("visibility 10 km or more".into());
    }
    if tok.len() == 4 && tok.chars().all(|c| c.is_ascii_digit()) {
        let m: f64 = tok.parse().ok()?;
        return plain(format!("visibility {}", if m >= 1000.0 { format!("{} km", num(m / 1000.0, 1)) } else { format!("{} m", num(m, 0)) }));
    }
    if let Some(v) = tok.strip_suffix("SM") {
        let v = v.trim_start_matches('P');
        let or_more = tok.starts_with('P');
        // "1 1/2SM": the whole number came as the previous token, which we
        // cannot see here; "1/2SM" alone is handled.
        let miles: Option<f64> = if let Some((a, b)) = v.split_once('/') {
            Some(a.parse::<f64>().ok()? / b.parse::<f64>().ok()?)
        } else {
            v.parse().ok()
        };
        let miles = miles?;
        return plain(format!("visibility {} miles{}", num(miles, 2), if or_more { " or more" } else { "" }));
    }
    if tok.chars().all(|c| c.is_ascii_digit()) && tok.len() == 1
        && let Some(n) = next
        && n.ends_with("SM")
        && n.contains('/')
    {
        // "1 1/2SM" — whole miles then a fraction.
        let whole: f64 = tok.parse().ok()?;
        let frac = n.trim_end_matches("SM");
        let (a, b) = frac.split_once('/')?;
        let miles = whole + a.parse::<f64>().ok()? / b.parse::<f64>().ok()?;
        return Some(Element { text: format!("visibility {} miles", num(miles, 2)), consumed_next: true });
    }
    // Clouds.
    for prefix in ["FEW", "SCT", "BKN", "OVC"] {
        if let Some(rest) = tok.strip_prefix(prefix) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let kind = cloud_type(&rest[digits.len()..]);
            let base_ft = digits.parse::<f64>().ok().map(|h| h * 100.0);
            let mut text = match base_ft {
                Some(b) => format!("{} at {}", cover(prefix), feet(b)),
                None => cover(prefix).to_string(),
            };
            if let Some(k) = kind {
                text.push_str(&format!(" ({k})"));
            }
            return plain(text);
        }
    }
    if let Some(vv) = tok.strip_prefix("VV") {
        if let Ok(h) = vv.parse::<f64>() {
            return plain(format!("sky obscured, vertical visibility {}", feet(h * 100.0)));
        }
        return plain("sky obscured".into());
    }
    if matches!(tok, "SKC" | "NSC" | "NCD" | "CLR") {
        return plain(cover(tok).into());
    }
    if tok == "NSW" {
        return plain("no significant weather".into());
    }
    // Temperatures: TX24/1615Z TNM02/1706Z
    if let Some(rest) = tok.strip_prefix("TX").or_else(|| tok.strip_prefix("TN")) {
        let label = if tok.starts_with("TX") { "max" } else { "min" };
        let (temp, at) = rest.split_once('/')?;
        let t: f64 = temp.replace('M', "-").parse().ok()?;
        let at = at.trim_end_matches('Z');
        return plain(format!("{label} {} at {}", celsius(t), day_hour(at, base)));
    }
    if let Some(q) = tok.strip_prefix('Q')
        && q.len() == 4
        && let Ok(hpa) = q.parse::<f64>()
    {
        return plain(format!("QNH {} hPa", num(hpa, 0)));
    }
    if let Some(ws) = tok.strip_prefix("WS") {
        return plain(format!("wind shear {ws}"));
    }
    // Weather codes: anything left that decodes to words.
    let decoded = weather_group(tok);
    if decoded != tok && !decoded.is_empty() && tok.chars().all(|c| c.is_ascii_uppercase() || c == '-' || c == '+') {
        return plain(decoded);
    }
    None
}

fn wind_group(tok: &str) -> Option<String> {
    let (body, unit) = if let Some(b) = tok.strip_suffix("KT") {
        (b, 1.0)
    } else if let Some(b) = tok.strip_suffix("MPS") {
        (b, 1.943_844)
    } else {
        return None;
    };
    if body.len() < 5 {
        return None;
    }
    let (dir, rest) = body.split_at(3);
    let (speed, gust) = match rest.split_once('G') {
        Some((s, g)) => (s, Some(g)),
        None => (rest, None),
    };
    let speed: f64 = speed.parse::<f64>().ok()? * unit;
    let gust = gust.and_then(|g| g.parse::<f64>().ok()).map(|g| g * unit);
    let dir_text = if dir == "VRB" {
        "variable".to_string()
    } else if dir == "000" && speed == 0.0 {
        return Some("wind calm".into());
    } else {
        format::bearing(dir.parse::<f64>().ok()?)
    };
    Some(match gust {
        Some(g) => format!("wind {dir_text} at {} gusting {} kt", num(speed, 0), num(g, 0)),
        None => format!("wind {dir_text} at {}", knots(speed)),
    })
}

fn is_validity(tok: &str) -> bool {
    tok.len() == 9 && tok.as_bytes()[4] == b'/' && tok.chars().filter(|c| c.is_ascii_digit()).count() == 8
}

/// `1612/1712` → `16 Sep 12:00 – 17 Sep 12:00 UTC`.
fn validity(tok: &str, base: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let (a, b) = tok.split_once('/').unwrap_or((tok, tok));
    format!("{} – {} UTC", day_hour(a, base), day_hour(b, base))
}

/// `1612` → `16 Sep 12:00` (month from the issue time; day-hour alone
/// without it).
fn day_hour(dh: &str, base: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let (d, h) = dh.split_at(dh.len().min(2));
    let h = h.get(..2).unwrap_or(h);
    match base {
        Some(b) => format!("{} {} {h}:00", d.trim_start_matches('0'), month_for_day(d, b)),
        None => format!("day {d} {h}:00"),
    }
}

fn day_hour_minute(dhm: &str, base: Option<chrono::DateTime<chrono::Utc>>) -> String {
    let (d, rest) = dhm.split_at(2);
    let (h, m) = rest.split_at(2);
    match base {
        Some(b) => format!("{} {} {h}:{m} UTC", d.trim_start_matches('0'), month_for_day(d, b)),
        None => format!("day {d} {h}:{m} UTC"),
    }
}

/// The month a day-of-month belongs to, given when the TAF was issued: a
/// TAF issued on the 30th that names the 1st means next month.
fn month_for_day(day: &str, base: chrono::DateTime<chrono::Utc>) -> String {
    use chrono::Datelike;
    let day: u32 = day.parse().unwrap_or(base.day());
    let month = if day + 7 < base.day() {
        // Wrapped into next month.
        base.month() % 12 + 1
    } else {
        base.month()
    };
    chrono::NaiveDate::from_ymd_opt(2000, month, 1)
        .map(|d| d.format("%b").to_string())
        .unwrap_or_default()
}

// --- SIGMET ----------------------------------------------------------------

pub fn hazard(code: &str) -> &'static str {
    match code {
        "TURB" => "turbulence",
        "ICE" | "ICING" => "icing",
        "TS" | "TSGR" => "thunderstorms",
        "VA" => "volcanic ash",
        "MTW" => "mountain waves",
        "DS" => "dust storm",
        "SS" => "sandstorm",
        "TC" => "tropical cyclone",
        "RDOACT" => "radioactive cloud",
        "FZRA" => "freezing rain",
        "WS" => "wind shear",
        _ => "hazard",
    }
}

pub fn qualifier(code: &str) -> &'static str {
    match code {
        "SEV" => "severe",
        "MOD" => "moderate",
        "EMBD" => "embedded",
        "FRQ" => "frequent",
        "OBSC" => "obscured",
        "ISOL" => "isolated",
        "OCNL" => "occasional",
        "SQL" => "squall line",
        "HVY" => "heavy",
        "ERUPTION" => "eruption",
        _ => "",
    }
}

fn level(ft: f64) -> String {
    if ft <= 0.0 {
        "the surface".into()
    } else if ft >= 18_000.0 {
        format!("FL{} ({} ft)", num(ft / 100.0, 0), num(ft, 0))
    } else {
        feet(ft)
    }
}

pub fn sigmet<'a>(_subject: Subject<'a>, attrs: &'a Map<String, Value>, used: &mut Vec<&'a str>) -> Card {
    let mut t = Take::new(attrs, used);
    let hazard_code = t.str("hazard").unwrap_or("");
    let qual_code = t.str("qualifier").unwrap_or("");
    let what = {
        let q = qualifier(qual_code);
        let h = hazard(hazard_code);
        let mut s = if q.is_empty() { h.to_string() } else { format!("{q} {h}") };
        if let Some(first) = s.get(..1) {
            let up = first.to_uppercase();
            s.replace_range(..1, &up);
        }
        s
    };
    let fir = t.str("fir").map(|f| {
        // "YBBB BRISBANE" → "Brisbane FIR (YBBB)"
        match f.split_once(' ') {
            Some((code, name)) => format!("{} FIR ({code})", title_case(name)),
            None => format!("{f} FIR"),
        }
    });
    let base = t.f64("base_ft");
    let top = t.f64("top_ft");
    let levels = match (base, top) {
        (Some(b), Some(tp)) => Some(format!("{} to {}", level(b), level(tp))),
        (None, Some(tp)) => Some(format!("up to {}", level(tp))),
        (Some(b), None) => Some(format!("from {}", level(b))),
        _ => None,
    };
    let valid = format::span(t.str("valid_from"), t.str("valid_to"));
    let movement = match (t.str("movement_dir"), t.f64("movement_kt")) {
        (Some(d), Some(kt)) if d != "-" && kt > 0.0 => Some(format!("moving {d} at {}", knots(kt))),
        (Some("-"), _) | (_, Some(0.0)) => Some("stationary".into()),
        (Some(d), None) => Some(format!("moving {d}")),
        _ => None,
    };

    let mut rows = Vec::new();
    rows.push(row_note("Hazard", what.clone(), format!("{qual_code} {hazard_code}").trim().to_string()));
    push(&mut rows, "Levels", levels.clone());
    push(&mut rows, "Valid", valid.clone());
    push(&mut rows, "Movement", movement.clone());
    push(&mut rows, "Airspace", fir.clone());
    push(&mut rows, "Issued by", t.str("issuing_office").map(str::to_string));

    let mut summary = what.clone();
    if let Some(l) = &levels {
        summary.push_str(&format!(", {l}"));
    }
    if let Some(m) = &movement {
        summary.push_str(&format!(", {m}"));
    }
    summary.push('.');

    let mut card = Card {
        title: match &fir {
            Some(f) => format!("{what} — {f}"),
            None => what,
        },
        subtitle: Some("Aviation hazard warning (SIGMET)".into()),
        summary: Some(summary),
        sections: Vec::new(),
        links: Vec::new(),
    };
    card.sections.extend(section(None, rows));
    let mut raw = Vec::new();
    push(&mut raw, "Message", t.str("raw").map(str::to_string));
    card.sections.extend(section(Some("Raw"), raw));
    card
}

fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_weather_decodes_to_words() {
        assert_eq!(weather("-RA BR"), "light rain, mist");
        assert_eq!(weather("+TSRA"), "heavy thunderstorm with rain");
        assert_eq!(weather("VCSH"), "showers in the vicinity");
        assert_eq!(weather("-SHRA BR"), "light showers of rain, mist");
        assert_eq!(weather("BCFG"), "patches of fog");
        assert_eq!(weather("TS"), "thunderstorm");
        assert_eq!(weather("+FC"), "tornado");
        assert_eq!(weather("DRSN"), "drifting snow");
        assert_eq!(weather("FZRA"), "freezing rain");
    }

    #[test]
    fn a_real_taf_becomes_forecast_periods() {
        let taf = "TAF PAHO 161137Z 1612/1712 24016G28KT P6SM BKN040 BKN080 FM162000 25012G22KT P6SM VCSH SCT040 BKN060 FM170400 25009KT P6SM FEW003 SCT060 SCT100";
        let base = "2026-09-16T11:37:00Z".parse().ok();
        let periods = decode_taf(taf, base);
        assert_eq!(periods.len(), 3, "{periods:?}");
        assert_eq!(periods[0].0, "From 16 Sep 12:00 – 17 Sep 12:00 UTC");
        assert_eq!(
            periods[0].1,
            "wind 240° (WSW) at 16 gusting 28 kt, visibility 6 miles or more, broken clouds at 4,000 ft (1,219 m), broken clouds at 8,000 ft (2,438 m)"
        );
        assert_eq!(periods[1].0, "From 16 Sep 20:00 UTC");
        assert!(periods[1].1.contains("showers in the vicinity"), "{}", periods[1].1);
        assert_eq!(periods[2].0, "From 17 Sep 04:00 UTC");
        assert!(periods[2].1.starts_with("wind 250° (WSW) at 9 kt (17 km/h)"), "{}", periods[2].1);
    }

    #[test]
    fn a_uk_taf_with_probabilities_and_becoming_groups() {
        let taf = "TAF EGLL 160458Z 1606/1712 28008KT 9999 FEW045 PROB30 TEMPO 1611/1615 8000 -SHRA PROB30 TEMPO 1704/1708 8000 -RA BKN014 BECMG 1708/1712 6000 RA SHRA BKN012 TX24/1615Z TNM02/1706Z";
        let base = "2026-09-16T04:58:00Z".parse().ok();
        let p = decode_taf(taf, base);
        assert_eq!(p.len(), 4, "{p:?}");
        assert!(p[0].1.contains("visibility 10 km or more"));
        assert!(p[0].1.contains("max 24 °C at 16 Sep 15:00"), "{}", p[0].1);
        assert!(p[0].1.contains("min -2 °C at 17 Sep 06:00"), "{}", p[0].1);
        assert_eq!(p[1].0, "Temporarily 16 Sep 11:00 – 16 Sep 15:00 UTC (30% chance)");
        assert_eq!(p[1].1, "visibility 8 km, light showers of rain");
        assert_eq!(p[3].0, "Becoming 17 Sep 08:00 – 17 Sep 12:00 UTC");
        assert!(p[3].1.contains("rain, showers of rain, broken clouds at 1,200 ft"), "{}", p[3].1);
    }

    #[test]
    fn a_taf_that_crosses_into_next_month_names_the_right_month() {
        let taf = "TAF EGLL 300458Z 3006/0112 28008KT 9999 FEW045 FM010200 20010KT 9999 SCT020";
        let base = "2026-09-30T04:58:00Z".parse().ok();
        let p = decode_taf(taf, base);
        assert_eq!(p[0].0, "From 30 Sep 06:00 – 1 Oct 12:00 UTC");
        assert_eq!(p[1].0, "From 1 Oct 02:00 UTC");
    }

    #[test]
    fn a_metar_card_reads_as_a_sentence_with_the_codes_underneath() {
        let attrs: Map<String, Value> = serde_json::from_str(r#"{"raw": "METAR PAHO 161153Z AUTO 24016G30KT 10SM BKN038 OVC046 11/06 A2994 RMK AO2 TSNO $", "taf": "TAF PAHO 161137Z 1612/1712 24016G28KT P6SM BKN040", "auto": true, "iata": "HOM", "name": "Homer Arpt", "clouds": [{"cover": "BKN", "base_ft": 3800.0}, {"cover": "OVC", "base_ft": 4600.0}], "temp_c": 10.6, "country": "US", "gust_kt": 30.0, "station": "PAHO", "dewpoint_c": 6.1, "taf_issued": "2026-09-16T11:37:00+00:00", "elevation_m": 6.0, "maintenance": true, "report_type": "METAR", "sensors_off": ["lightning"], "precip_24h_in": 0.47, "taf_valid_to": "2026-09-17T12:00:00+00:00", "wind_dir_deg": 240.0, "visibility_mi": 10.0, "wind_speed_kt": 16.0, "altimeter_inhg": 29.94, "taf_valid_from": "2026-09-16T12:00:00+00:00", "flight_category": "VFR", "weather": "-RA BR"}"#).unwrap();
        let value = Value::Object(attrs.clone());
        let card = crate::card(Subject { layer_id: "metars", kind: "station", key: "PAHO", label: Some("PAHO"), attrs: &value });
        assert_eq!(card.title, "Homer Airport (PAHO)");
        assert_eq!(
            card.summary.as_deref(),
            Some("Wind 240° (WSW) at 16 gusting 30 kt, visibility 10 miles (16.1 km), light rain, mist, broken clouds at 3,800 ft (1,158 m), overcast at 4,600 ft (1,402 m), 10.6 °C. VFR.")
        );
        let conditions = &card.sections[0];
        assert_eq!(conditions.heading.as_deref(), Some("Conditions"));
        let find = |label: &str| conditions.rows.iter().find(|r| r.label == label).map(|r| r.value.clone());
        assert_eq!(find("Humidity").as_deref(), Some("74%"));
        assert_eq!(find("Altimeter").as_deref(), Some("29.94 inHg (1,014 hPa)"));
        assert_eq!(find("Precipitation, 24 h").as_deref(), Some("0.47 in (11.9 mm)"));
        let forecast = card.sections.iter().find(|s| s.heading.as_deref() == Some("Forecast (TAF)")).unwrap();
        assert!(forecast.rows.iter().any(|r| r.label.starts_with("From 16 Sep 12:00")));
        let report = card.sections.iter().find(|s| s.heading.as_deref() == Some("Report")).unwrap();
        assert!(report.rows.iter().any(|r| r.value.contains("maintenance")));
        // Nothing left over for the generic tail: every key was understood.
        assert!(card.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{card:#?}");
        assert!(card.links.iter().any(|l| l.url.contains("PAHO")));
    }

    #[test]
    fn a_sigmet_says_what_where_and_how_high() {
        let attrs = serde_json::json!({"fir": "YBBB BRISBANE", "raw": "YBBB SIGMET K03 VALID 161200/161600 YMRF-", "hazard": "TURB", "top_ft": 5000, "base_ft": 0, "valid_to": "1789574400", "qualifier": "SEV", "valid_from": "1789560000", "movement_kt": 0, "movement_dir": "-", "issuing_office": "YMRF"});
        let card = crate::card(Subject { layer_id: "sigmets", kind: "event", key: "YBBB:K03", label: Some("SEV TURB"), attrs: &attrs });
        assert_eq!(card.title, "Severe turbulence — Brisbane FIR (YBBB)");
        assert_eq!(card.summary.as_deref(), Some("Severe turbulence, the surface to 5,000 ft (1,524 m), stationary."));
        let rows = &card.sections[0].rows;
        assert_eq!(rows.iter().find(|r| r.label == "Valid").unwrap().value, "16 Sep 12:00 – 16:00 UTC");
        assert!(card.sections.iter().all(|s| s.heading.as_deref() != Some("Also")), "{card:#?}");
    }
}
