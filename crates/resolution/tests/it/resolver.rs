use constellation_resolution::shared_directory_depth;

#[test]
fn shared_directory_depth_counts_leading_app_segments() {
    assert_eq!(
        shared_directory_depth("customer/contact/urls.py", "customer/contact/views.py"),
        2,
        "sibling files share their whole directory depth",
    );

    assert_eq!(
        shared_directory_depth("customer/contact/urls.py", "customer/interaction/views.py"),
        1,
        "different apps under one parent share only that parent",
    );

    assert_eq!(
        shared_directory_depth("customer/contact/urls.py", "inventory/views.py"),
        0,
        "unrelated app trees share nothing",
    );

    assert_eq!(
        shared_directory_depth("urls.py", "views.py"),
        0,
        "files with no directory share nothing",
    );

    assert_eq!(
        shared_directory_depth("a\\b\\urls.py", "a\\b\\views.py"),
        2,
        "windows separators count the same as forward slashes",
    );
}
