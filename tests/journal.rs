use alloy::primitives::Address;
use tangle_inference_core::journal::SettlementJournal;

#[test]
fn open_drops_a_line_this_operator_will_not_settle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settlements.jsonl");
    let mine = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
    let line = |operator: &str, hash: &str| {
        format!(
            r#"{{"owed":{{"auth_hash":"{hash}","spend_auth":{{"commitment":"0x01","serviceId":1,"jobIndex":0,"amount":"1","operator":"{operator}","nonce":0,"expiry":1,"signature":"0x"}},"amount":1,"metered":false}}}}"#
        )
    };
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n{}\nnot-json\n",
            line(mine, "mine"),
            line("0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc", "other"),
            line("0xnot-an-address", "foreign")
        ),
    )
    .unwrap();
    let journal = SettlementJournal::open(path.clone(), mine.parse::<Address>().unwrap())
        .unwrap()
        .unwrap();
    let owed = journal.owed();
    assert_eq!(owed.len(), 1);
    assert_eq!(owed[0].auth_hash, "mine");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("mine"));
    assert!(!text.contains("other"));
    assert!(!text.contains("foreign"));
    assert!(!text.contains("not-json"));
    assert!(!text.contains("0x3c44"));
}
