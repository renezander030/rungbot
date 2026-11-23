//! `cancel --order-id` narrows a pair's cancel to one resting venue order.

use rungbot_exec::reconcile::cancel_targets;
use rungbot_exec::venue::ParsedOrder;

fn resting(ids: &[&str]) -> Vec<ParsedOrder> {
    ids.iter()
        .map(|id| ParsedOrder {
            order_id: id.to_string(),
            side: "buy".into(),
            qty: 1.0,
            price: Some(10.0),
            ..Default::default()
        })
        .collect()
}

fn ids(v: &[ParsedOrder]) -> Vec<&str> {
    v.iter().map(|o| o.order_id.as_str()).collect()
}

#[test]
fn without_an_id_every_resting_order_is_a_target() {
    let got = cancel_targets(resting(&["o1", "o2", "o3"]), None).unwrap();
    assert_eq!(ids(&got), ["o1", "o2", "o3"]);
}

#[test]
fn an_id_keeps_only_that_order() {
    let got = cancel_targets(resting(&["o1", "o2", "o3"]), Some("o2")).unwrap();
    assert_eq!(ids(&got), ["o2"]);
}

#[test]
fn an_id_that_is_not_resting_cancels_nothing() {
    let err = cancel_targets(resting(&["o1", "o2"]), Some("o9")).unwrap_err();
    assert_eq!(err, "order o9 is not resting; nothing cancelled");
    assert!(cancel_targets(Vec::new(), Some("o1")).is_err());
}
