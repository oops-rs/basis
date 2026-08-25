use super::*;

/// A minimal domain error, standing in for `TemplateError`/`MemoryError`:
/// just enough to prove the generic helpers hand the right pieces back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Duplicate {
    name: String,
    first_path: PathBuf,
    second_path: PathBuf,
}

fn path(name: &str) -> PathBuf {
    PathBuf::from(name)
}

#[test]
fn a_root_with_no_repeated_name_loads_everything() {
    let found = load_root(
        vec![path("b.md"), path("a.md")],
        |p| Ok::<_, Duplicate>((p.display().to_string(), ())),
        |name, first_path, second_path| Duplicate {
            name,
            first_path,
            second_path,
        },
    )
    .expect("no duplicates");

    assert_eq!(found.len(), 2);
}

#[test]
fn duplicate_names_in_one_root_are_blamed_on_the_sorted_pair() {
    // Both files claim "same"; sorting first is what makes the blamed pair
    // deterministic regardless of the order `paths` arrived in.
    let error = load_root(
        vec![path("z-second.md"), path("a-first.md")],
        |_| Ok::<_, Duplicate>(("same".to_string(), ())),
        |name, first_path, second_path| Duplicate {
            name,
            first_path,
            second_path,
        },
    )
    .expect_err("two files claiming one name must refuse");

    assert_eq!(
        error,
        Duplicate {
            name: "same".to_string(),
            first_path: path("a-first.md"),
            second_path: path("z-second.md"),
        }
    );
}

#[test]
fn a_parse_failure_short_circuits_before_any_duplicate_check() {
    let error = load_root(
        vec![path("bad.md")],
        |_| {
            Err::<(String, ()), _>(Duplicate {
                name: "irrelevant".to_string(),
                first_path: path("bad.md"),
                second_path: path("bad.md"),
            })
        },
        |name, first_path, second_path| Duplicate {
            name,
            first_path,
            second_path,
        },
    )
    .expect_err("the parse error propagates");

    assert_eq!(error.first_path, path("bad.md"));
}

#[test]
fn merging_roots_keeps_the_strongest_writer_of_a_name() {
    let strong: BTreeMap<String, &str> = BTreeMap::from([
        ("shared".to_string(), "strong"),
        ("only-strong".to_string(), "strong"),
    ]);
    let weak: BTreeMap<String, &str> = BTreeMap::from([
        ("shared".to_string(), "weak"),
        ("only-weak".to_string(), "weak"),
    ]);

    let merged = merge_roots([Ok::<_, Duplicate>(strong), Ok(weak)]).expect("both roots load");

    assert_eq!(merged.iter().filter(|v| **v == "strong").count(), 2);
    assert_eq!(merged.iter().filter(|v| **v == "weak").count(), 1);
    assert_eq!(
        merged.len(),
        3,
        "a name repeated across roots is one entry, not two"
    );
}

#[test]
fn a_failing_root_fails_the_whole_merge() {
    let ok: BTreeMap<String, &str> = BTreeMap::from([("a".to_string(), "x")]);
    let error = Duplicate {
        name: "b".to_string(),
        first_path: path("one.md"),
        second_path: path("two.md"),
    };

    let result = merge_roots([Ok(ok), Err(error.clone())]);

    assert_eq!(result, Err(error));
}
