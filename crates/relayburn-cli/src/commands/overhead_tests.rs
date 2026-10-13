use super::*;

#[test]
fn format_line_range_pads_to_four() {
    assert_eq!(format_line_range(7, 11), "   7-  11");
    assert_eq!(format_line_range(100, 200), " 100- 200");
}

#[test]
fn duplicate_scoped_flag_is_an_error() {
    let err = merge_scoped_flag("--since", Some("7d"), Some("1d"))
        .expect_err("duplicate flag must not pick a winner");
    assert!(err.to_string().contains("both before and after"));
}

#[test]
fn scoped_flag_takes_whichever_side_was_given() {
    assert_eq!(
        merge_scoped_flag("--since", Some(1), None).unwrap(),
        Some(1)
    );
    assert_eq!(
        merge_scoped_flag("--since", None, Some(2)).unwrap(),
        Some(2)
    );
    assert_eq!(
        merge_scoped_flag::<u8>("--since", None, None).unwrap(),
        None
    );
}
