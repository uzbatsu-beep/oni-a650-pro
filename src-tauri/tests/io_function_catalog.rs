use a650_core::{catalog::load_embedded_parameters, safety::validate_parameter_value};
use serde_json::Value;
use std::collections::BTreeSet;

#[test]
fn io_function_choices_match_the_ui_and_the_fc06_validator() {
    // The offline UI embeds a copy of the catalog. A stale copy would display
    // choices that the write guard rejects, or omit valid drive functions.
    let page = include_str!("../../ui/index.html");
    let (_, catalog_js) = page.split_once("const PARAM_CATALOG = ").unwrap();
    let catalog: Vec<Value> = serde_json::Deserializer::from_str(catalog_js)
        .into_iter().next().unwrap().unwrap();
    let parameters = load_embedded_parameters();
    let inputs: BTreeSet<u16> = (0..=39).chain(50..=56).chain([58]).collect();
    let outputs: BTreeSet<u16> = (0..=23).chain(26..=28).chain(30..=42).collect();
    let analog: BTreeSet<u16> = (0..=13).collect();
    let control: BTreeSet<u16> = (0..=3).collect();

    for (group, range, expected) in [
        ("F04", 0..=9, &inputs),
        ("F04", 15..=15, &control),
        ("F05", 0..=3, &outputs),
        ("F07", 0..=2, &analog),
        ("F22", 0..=4, &inputs),
        ("F22", 7..=11, &outputs),
    ] {
        for index in range {
            let id = format!("{group}.{index:02}");
            let p = parameters.iter().find(|p| p.param_id == id).unwrap();
            let row = catalog.iter().find(|row| row["id"] == id.as_str()).unwrap();
            assert_eq!(p.data_type, "enum16", "{id}");
            assert_eq!(row["dt"], "enum16", "{id}");
            assert_eq!(row["en"].as_str(), Some(p.enum_values.as_str()), "{id}");
            assert_eq!(row["def"].as_str(), Some(p.default_value.as_str()), "{id}");
            let mut codes = BTreeSet::new();
            for entry in p.enum_values.split(';') {
                let (raw, label) = entry.split_once('=').unwrap();
                assert!(!label.trim().is_empty(), "{id} raw {raw}");
                assert!(codes.insert(raw.parse::<u16>().unwrap()), "{id} duplicate {raw}");
            }
            assert_eq!(&codes, expected, "{id}");
            for raw in 0..=99 {
                assert_eq!(validate_parameter_value(p, raw).is_ok(), codes.contains(&raw), "{id}: {raw}");
            }
        }
    }
}
