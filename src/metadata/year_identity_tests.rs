use super::*;
use serde_json::{Value, json};

fn movie(title: &str, year: u16, popularity: f64) -> Value {
    json!({"media_type": "movie", "title": title,
        "release_date": format!("{year:04}-01-01"), "popularity": popularity})
}

#[test]
fn explicit_year_selects_the_remake_identity_in_both_response_orders() {
    let mut years = Vec::new();
    for results in [
        vec![movie("Dune", 2021, 1000.0), movie("Dune", 1984, 1.0)],
        vec![movie("Dune", 1984, 1.0), movie("Dune", 2021, 1000.0)],
    ] {
        let found = lookup_using("Dune (1984)", |_| Some(json!({"results": results})))
            .unwrap()
            .unwrap();
        years.push(found.year);
    }
    assert_eq!(years, [1984, 1984]);
    assert!(is_confident_match("Dune (1984)", "Dune", 1984));
}

#[test]
fn only_the_wrong_year_remains_a_nonconfident_best_effort_guess() {
    let found = lookup_using("Dune (1984)", |_| {
        Some(json!({"results": [movie("Dune", 2021, 1000.0)]}))
    })
    .unwrap()
    .unwrap();
    assert_eq!(found.year, 2021);
    assert!(!is_confident_match("Dune (1984)", &found.title, found.year));
    assert!(!is_confident_match("Dune (1984)", "Dune", 0));
}

#[test]
fn wrong_year_exact_variant_does_not_return_before_the_correct_trimmed_match() {
    let seen = std::cell::RefCell::new(Vec::new());
    let found = lookup_using("DUNE_(1984)_UE", |query| {
        seen.borrow_mut().push(query.to_string());
        match query {
            "Dune Ue" => Some(json!({"results": [movie("Dune UE", 2021, 1000.0)]})),
            "Dune" => Some(json!({"results": [movie("Dune", 1984, 1.0)]})),
            _ => panic!("unexpected query {query}"),
        }
    })
    .unwrap()
    .unwrap();
    assert_eq!(seen.into_inner(), ["Dune Ue", "Dune"]);
    assert_eq!((found.title.as_str(), found.year), ("Dune", 1984));
}

#[test]
fn failed_packaging_variant_still_finds_the_correct_year() {
    let found = lookup_using("Dune (1984) UE", |query| match query {
        "Dune Ue" => None,
        "Dune" => Some(json!({"results": [
            movie("Dune", 2021, 1000.0), movie("Dune", 1984, 1.0)
        ]})),
        _ => panic!("unexpected query {query}"),
    })
    .unwrap()
    .unwrap();
    assert_eq!(found.year, 1984);
    assert!(is_confident_match(
        "Dune (1984) UE",
        &found.title,
        found.year
    ));
}

#[test]
fn failed_variant_and_wrong_year_guess_preserve_lookup_failure() {
    let found = lookup_using("DUNE_(1984)_UE", |query| match query {
        "Dune Ue" => None,
        "Dune" => Some(json!({"results": [movie("Dune", 2021, 1000.0)]})),
        _ => panic!("unexpected query {query}"),
    });
    let failed = found.expect_err("wrong-year guess must not override a failed query");
    assert_eq!(best_effort(Err(failed)).unwrap().year, 2021);
}

#[test]
fn conflicting_explicit_years_never_become_confident() {
    for label in ["Dune (1984) (2021)", "Dune (2021) (1984)"] {
        for year in [1984, 2021] {
            assert!(!is_confident_match(label, "Dune", year));
        }
        let failed = lookup_using(&format!("{label} UE"), |query| {
            if query.ends_with("Ue") {
                None
            } else {
                Some(json!({"results": [movie("Dune", 1984, 1.0)]}))
            }
        });
        assert!(failed.is_err());
    }
    assert!(is_confident_match("Dune (1984) (1984)", "Dune", 1984));
}

#[test]
fn no_explicit_year_keeps_popularity_and_tv_preferences() {
    let results = json!({"results": [
        movie("Dune", 1984, 1.0), movie("Dune", 2021, 1000.0)
    ]});
    let found = lookup_using("Dune", |_| Some(results.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(found.year, 2021);
    assert!(is_confident_match("Dune", "Dune", 1984));

    let results = json!({"results": [
        movie("Dune", 1984, 1000.0),
        {"media_type": "tv", "name": "Dune", "first_air_date": "2000-01-01",
            "popularity": 1.0}
    ]});
    let tv = lookup_using("Dune Season 1", |_| Some(results.clone()))
        .unwrap()
        .unwrap();
    assert!(tv.media_type.is_tv());
    let film = lookup_using("Dune", |_| Some(results.clone()))
        .unwrap()
        .unwrap();
    assert!(!film.media_type.is_tv());
}

#[test]
fn explicit_year_identity_precedes_tv_preference_and_keeps_tv_tiebreaks() {
    for tv_year in [1984, 2000] {
        let results = json!({"results": [
            movie("Dune", 1984, 1000.0),
            {"media_type": "tv", "name": "Dune",
                "first_air_date": format!("{tv_year}-01-01"), "popularity": 1.0}
        ]});
        let found = lookup_using("Dune (1984) Season 1", |_| Some(results.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(found.year, 1984);
        assert_eq!(found.media_type.is_tv(), tv_year == 1984);
    }
}

#[test]
fn bare_years_unicode_and_query_budget_keep_existing_behavior() {
    assert_eq!(query_variants("BLADE_RUNNER_2049"), ["Blade Runner 2049"]);
    assert!(is_confident_match(
        "BLADE_RUNNER_2049",
        "Blade Runner 2049",
        2017
    ));
    assert!(is_confident_match(
        "Amélie (2001) - Blu-ray",
        "Amélie",
        2001
    ));
    assert_eq!(
        query_variants("DUNE_(1984)_UE_BD3_4K_3D_UK"),
        [
            "Dune Ue Bd3 4k 3d Uk",
            "Dune Ue Bd3 4k 3d",
            "Dune Ue Bd3 4k",
            "Dune Ue Bd3",
            "Dune Ue"
        ]
    );
    assert!(!is_confident_match(
        "Amélie (2001) - Blu-ray",
        "Amélie",
        2021
    ));
}

#[test]
fn explicit_year_with_no_api_key_still_skips_the_request() {
    assert!(lookup("Dune (1984)", "").unwrap().is_none());
}
