use std::collections::BTreeSet;

use kurama_adapters::RandomIds;
use kurama_protocol::traits::IdGenerator;

#[test]
fn random_ids_are_prefixed_lowercase_and_unique() {
    let ids = RandomIds;
    let generated = [
        ids.session_id().to_string(),
        ids.agent_id().to_string(),
        ids.operation_id().to_string(),
        ids.call_id().to_string(),
    ];

    assert_eq!(generated[0].len(), 34);
    assert!(generated[0].starts_with("s_"));
    assert!(generated[1].starts_with("a_"));
    assert!(generated[2].starts_with("o_"));
    assert!(generated[3].starts_with("c_"));
    assert!(generated.iter().all(|id| {
        id[2..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }));
    assert_eq!(generated.iter().collect::<BTreeSet<_>>().len(), 4);
}
