use super::*;

pub(super) fn host() -> (Host, mpsc::Receiver<Outgoing>, Connection) {
    let (outgoing, rx) = mpsc::channel(64);
    let host = Host {
        nonce: "01".repeat(16),
        timeout: Duration::from_secs(30),
        outgoing,
        lost: CancellationToken::new(),
        ledger: Mutex::default(),
    };
    let connection = Connection(1);
    host.activate(connection, "fixture", "2025-11-25", None);
    (host, rx, connection)
}

fn form() -> Question {
    Question::Form {
        message: "untrusted message\nwith a newline".into(),
        schema: ElicitFormSchema::new().integer_field("quantity", None, true),
        credential_fields: Vec::new(),
    }
}

pub(super) fn frame(rx: &mut mpsc::Receiver<Outgoing>) -> Value {
    let Outgoing::Frame(bytes) = rx.try_recv().expect("queued frame") else {
        panic!("expected frame");
    };
    assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
    serde_json::from_slice(&bytes).expect("valid frame")
}

pub(super) fn response(host: &Host, session: &str, id: u64, payload: Value) {
    let mut value = json!({
        "v": VERSION, "type": "response", "session": session, "id": id.to_string()
    });
    value
        .as_object_mut()
        .unwrap()
        .extend(payload.as_object().unwrap().clone());
    receive(host, &serde_json::to_vec(&value).unwrap());
}

#[test]
fn invalid_replies_leave_live_questions_and_integer_answers_keep_precision() {
    let (host, mut rx, connection) = host();
    let mut tickets = host
        .open(connection, vec![((), form())], None, None)
        .unwrap();
    let (_, ticket) = tickets.first_mut().unwrap();
    let request = frame(&mut rx);
    assert_eq!(
        request["request"]["message"],
        "untrusted message\nwith a newline"
    );
    let session = host.session(connection.0);
    for (remote, payload, reason) in [
        (
            host.session(2),
            json!({"action":"decline"}),
            "cross_session",
        ),
        (
            session.clone(),
            json!({"action":"confirm_url"}),
            "wrong_kind",
        ),
        (
            session.clone(),
            json!({"action":"accept","content":{"quantity":"1"}}),
            "invalid_payload",
        ),
        (
            session.clone(),
            json!({"action":"accept","content":{}}),
            "invalid_payload",
        ),
    ] {
        response(&host, &remote, ticket.id, payload);
        assert_eq!(frame(&mut rx)["reason"], reason);
    }
    response(
        &host,
        &session,
        ticket.id,
        json!({"action":"accept","content":{"quantity":i64::MAX}}),
    );
    assert_eq!(frame(&mut rx)["status"], "accepted");
    let result = elicit_result(ticket.receiver.try_recv().unwrap());
    assert_eq!(
        serde_json::to_value(result).unwrap()["content"]["quantity"],
        i64::MAX
    );
    response(&host, &session, ticket.id, json!({"action":"decline"}));
    assert_eq!(frame(&mut rx)["reason"], "duplicate");
}

#[test]
fn stale_replies_are_rejected_before_the_waiter_wakes() {
    let (host, mut rx, connection) = host();
    let stop = CancellationToken::new();
    let mut first = host
        .open(connection, vec![((), form())], Some(&stop), None)
        .unwrap();
    let mut second = host
        .open(connection, vec![((), form())], None, None)
        .unwrap();
    let _ = frame(&mut rx);
    let _ = frame(&mut rx);
    stop.cancel();
    let second_id = second[0].1.id;
    host.ledger
        .lock()
        .unwrap()
        .pending
        .get_mut(&second_id)
        .unwrap()
        .deadline = Instant::now() - Duration::from_secs(1);

    let session = host.session(connection.0);
    for (tickets, reason) in [
        (&mut first, Withdrawal::ServerCancelled),
        (&mut second, Withdrawal::Expired),
    ] {
        let (_, ticket) = tickets.first_mut().unwrap();
        response(&host, &session, ticket.id, json!({"action":"decline"}));
        assert_eq!(frame(&mut rx)["reason"], reason.code());
        assert_eq!(frame(&mut rx)["reason"], "stale");
        assert!(matches!(ticket.receiver.try_recv(), Ok(Err(actual)) if actual == reason));
    }
}

#[test]
fn pending_limit_and_connection_switch_release_the_batch() {
    let (host, mut rx, connection) = host();
    let mut tickets = host
        .open(
            connection,
            (0..MAX_PENDING).map(|i| (i, form())).collect(),
            None,
            None,
        )
        .unwrap();
    assert!(matches!(
        host.open(connection, vec![((), form())], None, None),
        Err(Withdrawal::Busy)
    ));
    for _ in 0..MAX_PENDING {
        assert_eq!(frame(&mut rx)["type"], "needs_input");
    }
    host.activate(Connection(2), "replacement", "2025-11-25", None);
    for (_, ticket) in &mut tickets {
        assert!(matches!(
            ticket.receiver.try_recv(),
            Ok(Err(Withdrawal::Disconnected))
        ));
        assert_eq!(frame(&mut rx)["reason"], "disconnected");
    }
    response(
        &host,
        &host.session(connection.0),
        tickets[0].1.id,
        json!({"action":"decline"}),
    );
    assert_eq!(frame(&mut rx)["reason"], "cross_session");
}

#[test]
fn a_batch_with_an_oversized_frame_registers_no_questions() {
    let (host, mut rx, connection) = host();
    let oversize = Question::Url {
        message: "a".repeat(MAX_FRAME),
        url: "https://example.com".into(),
    };
    assert!(matches!(
        host.open(connection, vec![(0, form()), (1, oversize)], None, None),
        Err(Withdrawal::Busy)
    ));
    assert!(host.ledger.lock().unwrap().pending.is_empty());
    assert!(rx.try_recv().is_err());
}

#[test]
fn url_and_sampling_answers_require_explicit_provenance() {
    let (host, mut rx, connection) = host();
    assert!(Question::url("unsafe".into(), "file:///tmp/file".into()).is_none());
    let url = Question::url("confirm?".into(), "https://example.com".into()).unwrap();
    let sample = Question::Sampling(Box::new(CreateMessageParams::new(Vec::new(), 12)));
    let mut tickets = host
        .open(connection, vec![(0, url), (1, sample)], None, None)
        .unwrap();
    let _ = frame(&mut rx);
    let _ = frame(&mut rx);
    let session = host.session(connection.0);
    response(
        &host,
        &session,
        tickets[0].1.id,
        json!({"action":"accept","content":{}}),
    );
    assert_eq!(frame(&mut rx)["reason"], "wrong_kind");
    response(
        &host,
        &session,
        tickets[0].1.id,
        json!({"action":"confirm_url"}),
    );
    assert_eq!(frame(&mut rx)["status"], "accepted");
    let result = elicit_result(tickets[0].1.receiver.try_recv().unwrap());
    assert_eq!(result.action, ElicitAction::Accept);
    assert!(result.content.is_none());

    for (model, status) in [("fake-model", "rejected"), (OPERATOR_MODEL, "accepted")] {
        response(
            &host,
            &session,
            tickets[1].1.id,
            json!({
                "action":"sample", "origin":"operator",
                "result":{"model":model,"role":"assistant","content":{"type":"text","text":"fixture"}}
            }),
        );
        assert_eq!(frame(&mut rx)["status"], status);
    }
}

#[test]
fn malformed_frames_get_fixed_errors_without_echoing_payloads() {
    let (host, mut rx, _) = host();
    receive(&host, b"{this could contain a secret}");
    assert_eq!(
        frame(&mut rx),
        json!({
            "v":1, "type":"response_status", "status":"rejected", "reason":"malformed"
        })
    );
}

#[test]
fn wire_schemas_use_their_json_types_and_enum_choices() {
    let schema: ElicitFormSchema = serde_json::from_value(json!({
        "type": "object",
        "required": ["quantity", "labels", "mode", "colors"],
        "properties": {
            "quantity": {"type":"integer","minimum":0.5},
            "labels": {"type":"array","items":{"type":"string"}},
            "mode": {"type":"string","enum":["fast","slow"]},
            "colors": {"type":"array","items":{"type":"string","enum":["red","blue, green"]}},
            "titled": {"type":"string","oneOf":[{"const":"a","title":"Alpha"}]}
        }
    }))
    .unwrap();
    for key in ["quantity", "labels"] {
        assert!(matches!(
            schema.properties[key],
            PrimitiveSchemaDefinition::Raw(_)
        ));
    }
    let expected = ExpectedAnswer::Form(schema);
    let accepted = json!({
        "quantity": 2, "labels": ["one"], "mode": "fast", "colors": ["red", "blue, green"],
        "titled": "a"
    });
    let validate = |content| {
        let answer = serde_json::from_value(json!({"action":"accept","content":content})).unwrap();
        expected.validate(&answer)
    };
    assert_eq!(validate(accepted.clone()), Ok(()));
    for (key, invalid) in [
        ("quantity", json!(2.5)),
        ("labels", json!("one")),
        ("mode", json!("other")),
        ("colors", json!(["red", "blue"])),
        ("titled", json!("Alpha")),
    ] {
        let mut content = accepted.clone();
        content[key] = invalid;
        assert_eq!(validate(content), Err("invalid_payload"), "{key}");
    }
}

#[tokio::test]
async fn ask_resolves_the_race_between_a_reply_and_the_request_token() {
    let (host, mut rx, connection) = host();
    let session = host.session(connection.0);
    for cancel_first in [true, false] {
        let token = CancellationToken::new();
        let mut ask = std::pin::pin!(host.ask(connection, form(), token.clone()));
        assert!(
            ask.as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        let id = frame(&mut rx)["id"].as_str().unwrap().parse().unwrap();
        if cancel_first {
            token.cancel();
            response(&host, &session, id, json!({"action":"decline"}));
            assert_eq!(frame(&mut rx)["reason"], "server_cancelled");
            assert_eq!(frame(&mut rx)["reason"], "stale");
            assert!(matches!(ask.await, Err(Withdrawal::ServerCancelled)));
        } else {
            response(&host, &session, id, json!({"action":"decline"}));
            token.cancel();
            assert_eq!(frame(&mut rx)["status"], "accepted");
            // The bridge already handed off the answer. tower-mcp still
            // checks the token before sending any stable MCP reply.
            assert!(matches!(ask.await, Ok(Answer::Decline {})));
            assert!(rx.try_recv().is_err());
        }
    }
}

#[test]
fn evicted_retired_ids_become_unknown() {
    let (host, mut rx, connection) = host();
    let session = host.session(connection.0);
    let mut ids = Vec::new();
    for _ in 0..=MAX_RETIRED {
        let tickets = host
            .open(connection, vec![((), form())], None, None)
            .unwrap();
        let id = tickets[0].1.id;
        ids.push(id);
        assert_eq!(frame(&mut rx)["type"], "needs_input");
        response(&host, &session, id, json!({"action":"decline"}));
        assert_eq!(frame(&mut rx)["status"], "accepted");
    }
    response(&host, &session, ids[0], json!({"action":"decline"}));
    assert_eq!(frame(&mut rx)["reason"], "unknown");
    response(&host, &session, ids[1], json!({"action":"decline"}));
    assert_eq!(frame(&mut rx)["reason"], "duplicate");
}

#[test]
fn command_withdrawal_only_applies_to_newer_stable_questions() {
    let (host, mut rx, connection) = host();
    let token = CancellationToken::new();
    let open = |token| {
        host.open(connection, vec![((), form())], token, None)
            .unwrap()
    };
    let mut earlier = open(Some(&token));
    let watermark = host.ledger.lock().unwrap().next_id;
    let mut current = open(Some(&token));
    let mut final_round = open(None);
    for _ in 0..3 {
        assert_eq!(frame(&mut rx)["type"], "needs_input");
    }

    host.withdraw_stable_after(watermark);
    let withdrawn = frame(&mut rx);
    assert_eq!(withdrawn["id"], current[0].1.id.to_string());
    assert_eq!(withdrawn["reason"], "cancelled");
    assert!(matches!(
        current[0].1.receiver.try_recv(),
        Ok(Err(Withdrawal::Cancelled))
    ));
    for ticket in [&mut earlier[0].1, &mut final_round[0].1] {
        response(
            &host,
            &host.session(connection.0),
            ticket.id,
            json!({"action":"decline"}),
        );
        assert_eq!(frame(&mut rx)["status"], "accepted");
        assert!(matches!(
            ticket.receiver.try_recv(),
            Ok(Ok(Answer::Decline {}))
        ));
    }
}

#[test]
fn unsupported_versions_keep_identifiers_without_decoding_actions() {
    let (host, mut rx, connection) = host();
    let session = host.session(connection.0);
    let tickets = host
        .open(connection, vec![((), form())], None, None)
        .unwrap();
    let id = tickets[0].1.id;
    let _ = frame(&mut rx);
    response(
        &host,
        &session,
        id,
        json!({"v":2,"action":"future_action","future_field":true}),
    );
    assert_eq!(
        frame(&mut rx),
        json!({
            "v":1, "type":"response_status", "session":session, "id":id.to_string(),
            "status":"rejected", "reason":"unsupported_version"
        })
    );
    response(
        &host,
        &session,
        id,
        json!({"action":"decline","unexpected_field":true}),
    );
    assert_eq!(frame(&mut rx)["reason"], "malformed");
    response(&host, &session, id, json!({"action":"decline"}));
    assert_eq!(frame(&mut rx)["status"], "accepted");
}

#[test]
fn a_full_output_queue_can_drop_status_without_losing_the_answer() {
    let (host, mut rx, connection) = host();
    let mut tickets = host
        .open(connection, vec![((), form())], None, None)
        .unwrap();
    let ticket = &mut tickets[0].1;
    let _ = frame(&mut rx);
    for _ in 0..host.outgoing.max_capacity() {
        host.send(&json!({"type":"queued"})).unwrap();
    }
    response(
        &host,
        &host.session(connection.0),
        ticket.id,
        json!({"action":"decline"}),
    );
    assert!(matches!(
        ticket.receiver.try_recv(),
        Ok(Ok(Answer::Decline {}))
    ));
    assert!(!host.lost.is_cancelled());
    assert_eq!(rx.len(), host.outgoing.max_capacity());
}
