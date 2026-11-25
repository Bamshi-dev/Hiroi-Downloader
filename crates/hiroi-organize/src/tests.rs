use super::*;

fn of(name: &str) -> (String, u32) {
    let episode = episode(name).unwrap_or_else(|| panic!("{name} is not an episode"));
    (episode.series, episode.number)
}

#[test]
fn markers_and_separators() {
    assert_eq!(of("gardening vid 1.mp4"), ("gardening".into(), 1));
    assert_eq!(of("Gardening_vid_02.mp4"), ("Gardening".into(), 2));
    assert_eq!(
        of("gardening vid 3 - tomatoes.mkv"),
        ("gardening".into(), 3)
    );
    assert_eq!(
        of("Deep.Learning.Lecture.04.720p.mp4"),
        ("Deep Learning".into(), 4)
    );
    assert_eq!(of("Rust Course ep01.webm"), ("Rust Course".into(), 1));
    assert_eq!(
        of("Rust Course - Part 12 [1080p].mp4"),
        ("Rust Course".into(), 12)
    );
    assert_eq!(of("algebra 7.pdf"), ("algebra".into(), 7));
}

#[test]
fn seasons_and_episodes() {
    assert_eq!(
        of("Show.Name.S01E02.1080p.x264.mkv"),
        ("Show Name".into(), 1002)
    );
    assert_eq!(
        of("[Group] Show Name s2e10.mkv"),
        ("Show Name".into(), 2010)
    );
}

#[test]
fn numbers_that_are_not_positions() {
    assert_eq!(of("CS50 lecture 3.mp4"), ("CS50".into(), 3));
    assert_eq!(of("Talks 2024 - talk 5.mp4"), ("Talks 2024".into(), 5));
    assert!(episode("report 2023.pdf").is_none());
    assert!(episode("movie 1080p x264.mkv").is_none());
    assert!(episode("backup 20240101.tar.gz").is_none());
}

#[test]
fn numbered_names_that_are_not_series() {
    for name in [
        "IMG_0001.jpg",
        "DSC0042.JPG",
        "Untitled 2.png",
        "file1.zip",
        "a 1.txt",
        "notes.txt",
    ] {
        assert!(episode(name).is_none(), "{name}");
    }
}

#[test]
fn copy_numbers_are_not_episodes() {
    assert!(episode("clip (1).mp4").is_none());
    assert!(episode("Annual report (2).pdf").is_none());
    assert_eq!(of("algebra 3 (1).pdf"), ("algebra".into(), 3));
}

#[test]
fn a_marker_with_nothing_before_it_names_the_series() {
    assert_eq!(of("Lecture 3 - Tomatoes.mp4"), ("Lecture".into(), 3));
}

#[test]
fn series_match_across_case_separators_and_typos() {
    let a = episode("gardening vid 1.mp4").unwrap();
    for other in [
        "GarDening-vid-2.mp4",
        "garden_ing vid 3.mp4",
        "gardneing vid 4.mp4",
    ] {
        assert!(a.same_series(&episode(other).unwrap()), "{other}");
    }
    assert!(!a.same_series(&episode("gardenhouses vid 1.mp4").unwrap()));
    // Short names must match exactly: one letter is a different word.
    let short = episode("math 1.pdf").unwrap();
    assert!(!short.same_series(&episode("maps 2.pdf").unwrap()));
}

#[test]
fn group_names() {
    let date = "2026-11-20";
    let series = [
        "gardening vid 1.mp4",
        "gardening vid 2.mp4",
        "gardening vid 3.mp4",
    ];
    assert_eq!(
        group_name(Batch {
            names: &series,
            date,
            ..Batch::default()
        }),
        "gardening"
    );

    let prefix = ["Annual report summary.pdf", "Annual report tables.xlsx"];
    assert_eq!(
        group_name(Batch {
            names: &prefix,
            date,
            ..Batch::default()
        }),
        "Annual report"
    );

    let mixed = ["a.zip", "b.pdf"];
    let titled = Batch {
        names: &mixed,
        title: Some("Course material | Example University"),
        host: Some("www.example.edu"),
        date,
    };
    assert_eq!(group_name(titled), "Course material");
    assert_eq!(
        group_name(Batch {
            title: None,
            ..titled
        }),
        "example.edu 2026-11-20"
    );
    assert_eq!(
        group_name(Batch {
            date,
            ..Batch::default()
        }),
        "Downloads 2026-11-20"
    );
}
