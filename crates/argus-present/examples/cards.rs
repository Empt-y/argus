//! Render cards for rows piped in from the store, one JSON object per line
//! with `layer_id`, `entity_kind`, `entity_key`, `label` and `attrs` — the
//! shape `SELECT row_to_json(e) FROM entities e` gives. Prints each card,
//! and at the end which keys per layer fell through to the "Also" section,
//! so a presenter can be checked against a whole layer rather than the one
//! record it was written from.
//!
//!   psql -At -c "select row_to_json(e) from entities e" | cargo run -p argus-present --example cards

use std::collections::BTreeMap;
use std::io::BufRead;

fn main() {
    let quiet = std::env::args().any(|a| a == "--summary");
    let mut leftovers: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for line in std::io::stdin().lock().lines() {
        let line = line.unwrap();
        if line.trim().is_empty() {
            continue;
        }
        let row: serde_json::Value = serde_json::from_str(&line).expect("a JSON row");
        let layer = row["layer_id"].as_str().unwrap_or("?").to_string();
        let card = argus_present::card(argus_present::Subject {
            layer_id: &layer,
            kind: row["entity_kind"].as_str().unwrap_or(""),
            key: row["entity_key"].as_str().unwrap_or(""),
            label: row["label"].as_str(),
            attrs: &row["attrs"],
        });
        *counts.entry(layer.clone()).or_default() += 1;
        for s in &card.sections {
            if s.heading.as_deref() == Some("Also") {
                for r in &s.rows {
                    *leftovers.entry(layer.clone()).or_default().entry(r.label.clone()).or_default() += 1;
                }
            }
        }
        if !quiet {
            println!("== [{layer}] {}", card.title);
            if let Some(s) = &card.subtitle {
                println!("   {s}");
            }
            if let Some(s) = &card.summary {
                println!("   {s}");
            }
            for s in &card.sections {
                if let Some(h) = &s.heading {
                    println!("   -- {h}");
                }
                for r in &s.rows {
                    match &r.note {
                        Some(n) => println!("   {:<22} {}  [{n}]", r.label, r.value),
                        None => println!("   {:<22} {}", r.label, r.value),
                    }
                }
            }
            for l in &card.links {
                println!("   -> {} {}", l.label, l.url);
            }
        }
    }
    eprintln!("\nrows per layer: {counts:?}");
    eprintln!("keys that fell through to 'Also':");
    for (layer, keys) in &leftovers {
        eprintln!("  {layer}: {keys:?}");
    }
}
