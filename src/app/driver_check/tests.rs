use super::wasapi_endpoint;

#[test]
fn the_host_prefix_is_not_part_of_the_endpoint() {
    let endpoint = "{0.0.0.00000000}.{9f0b0d09-0b0b-4a0b-9b0b-0b0b0b0b0b0b}";
    assert_eq!(wasapi_endpoint(&format!("WASAPI:{endpoint}")), endpoint);
}

#[test]
fn a_plain_endpoint_is_left_alone() {
    let endpoint = "{0.0.0.00000000}.{9f0b0d09-0b0b-4a0b-9b0b-0b0b0b0b0b0b}";
    assert_eq!(wasapi_endpoint(endpoint), endpoint);
}
