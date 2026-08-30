use std::collections::BTreeSet;

use kurama_adapters::RandomIds;
use kurama_protocol::traits::IdGenerator;

#[test]
fn random_ids_use_expected_lowercase_hex_shapes() {
    let ids = RandomIds;
    let generated = [
        ids.session_id().to_string(),
        ids.agent_id().to_string(),
        ids.operation_id().to_string(),
        ids.call_id().to_string(),
    ];

    assert_eq!(generated[0].len(), 12);
    assert!(generated[0].starts_with("ses_"));
    assert!(generated[0][4..].bytes().all(is_lowercase_hex));
    assert!(generated[1].starts_with("a_"));
    assert!(generated[2].starts_with("o_"));
    assert!(generated[3].starts_with("c_"));
    assert!(
        generated[1..]
            .iter()
            .all(|id| id.len() == 34 && id[2..].bytes().all(is_lowercase_hex))
    );
    assert_eq!(generated.iter().collect::<BTreeSet<_>>().len(), 4);
}

fn is_lowercase_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}
