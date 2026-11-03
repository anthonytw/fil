//! `fil forward` and `fil ports` through the daemon's socket (DESIGN.md sections 7.3, 7.4 and
//! 8.5; `docs/m2/plan.md`, F2): management requests reach the session's forward table, and the
//! ports answer lists every session with its forwards.

mod common;

use std::num::NonZeroU16;

use common::*;
use fil_proto::ChannelId;
use fil_proto::msg::{
    ControlMsg, ForwardOp, ForwardResult, PortsRow, Request, Response, SessionName,
};
use fil_proto::mux::{
    BindAddr, ForwardKind, ForwardRefusal, ForwardSpec, ListenState, Origin, Target, TargetHost,
};
use fil_transport::Link;

fn remote(to: u16) -> ForwardSpec {
    ForwardSpec::new(
        ForwardKind::Remote,
        BindAddr::Loopback,
        0,
        Some(Target {
            host: TargetHost::Ip("127.0.0.1".parse().unwrap()),
            port: NonZeroU16::new(to).unwrap(),
        }),
    )
    .unwrap()
}

fn name(s: &str) -> SessionName {
    s.parse().unwrap()
}

async fn forward(d: &Running, session: &str, op: ForwardOp) -> Response {
    ask(
        d,
        Request::Forward {
            session: name(session),
            op,
        },
    )
    .await
}

/// The session and forward rows of a `Ports` answer, in order (the host's own listeners, which
/// discovery adds on Linux, are not this test's).
async fn ports(d: &Running, session: Option<&str>) -> Result<Vec<PortsRow>, Response> {
    all_ports(d, session).await.map(|rows| {
        rows.into_iter()
            .filter(|r| matches!(r, PortsRow::Session(_) | PortsRow::Forward(_)))
            .collect()
    })
}

/// Every part of a `Ports` answer, in order.
async fn all_ports(d: &Running, session: Option<&str>) -> Result<Vec<PortsRow>, Response> {
    let link = d.pipe().await;
    let first = request_rev(
        &link,
        Request::Ports {
            session: session.map(name),
        },
        d.paths.revision,
    )
    .await;
    let mut rows = Vec::new();
    let mut next = first;
    loop {
        let Response::Ports { part, more } = next else {
            link.close();
            return Err(next);
        };
        rows.extend(part.rows);
        if !more {
            link.close();
            return Ok(rows);
        }
        let (ch, payload) = tokio::time::timeout(WAIT, link.recv())
            .await
            .expect("the next part in time")
            .expect("the link stays up until the last part");
        assert_eq!(ch, ChannelId::CONTROL);
        let ControlMsg::Response(r) = ControlMsg::decode(&payload).unwrap() else {
            panic!("not a response");
        };
        next = r;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_requests_and_the_ports_answer() {
    let s = Scratch::new();
    let d = start(&s, config(CAT)).await;
    let a = Client::attach(d.pipe().await, Connect::new("main", key(1))).await;
    assert!(a.reply.is_some());

    // No such session.
    assert_eq!(
        forward(&d, "nope", ForwardOp::Add(remote(9))).await,
        Response::NoSuchSession
    );
    assert_eq!(ports(&d, Some("nope")).await, Err(Response::NoSuchSession));

    // An -R 0 from a request: added, bound by the daemon, not consented by anyone yet.
    let Response::Forward(ForwardResult::Added(id)) =
        forward(&d, "main", ForwardOp::Add(remote(9))).await
    else {
        panic!("not added");
    };
    // A pivot into the laptop's network is refused.
    let pivot = ForwardSpec::new(
        ForwardKind::Remote,
        BindAddr::Loopback,
        0,
        Some(Target {
            host: TargetHost::Ip("192.0.2.7".parse().unwrap()),
            port: NonZeroU16::new(22).unwrap(),
        }),
    )
    .unwrap();
    assert_eq!(
        forward(&d, "main", ForwardOp::Add(pivot)).await,
        Response::Forward(ForwardResult::Refused(ForwardRefusal::NotPermitted))
    );

    let Response::Sessions { list, .. } = ask(&d, Request::ListSessions).await else {
        panic!("not a list");
    };
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].forwards, 1);

    let rows = ports(&d, None).await.unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        matches!(&rows[0], PortsRow::Session(s) if s.name.as_str() == "main" && s.forwards == 1)
    );
    let PortsRow::Forward(f) = &rows[1] else {
        panic!("not a forward row");
    };
    assert_eq!((f.id, f.origin, f.consented), (id, Origin::Request, false));
    let Some(ListenState::Listening { port }) = f.listen else {
        panic!("not bound: {:?}", f.listen);
    };
    // The same forward row for the session alone (the session row's output time moves on).
    let focused = ports(&d, Some("main")).await.unwrap();
    assert_eq!(focused.len(), 2);
    assert_eq!(focused[1], rows[1]);
    // The -R listener is the daemon's own: discovery never lists it as anyone's listener.
    let all = all_ports(&d, None).await.unwrap();
    assert!(
        !all.iter()
            .any(|r| matches!(r, PortsRow::Listener(l) if l.port == port)),
        "{all:?}"
    );

    // `fil forward --rm PORT` finds an -R 0 by the port it got.
    assert_eq!(
        forward(
            &d,
            "main",
            ForwardOp::Remove {
                kind: None,
                port: port.get()
            }
        )
        .await,
        Response::Forward(ForwardResult::Removed(id))
    );
    assert_eq!(
        forward(
            &d,
            "main",
            ForwardOp::Remove {
                kind: Some(ForwardKind::Remote),
                port: port.get()
            }
        )
        .await,
        Response::Forward(ForwardResult::Refused(ForwardRefusal::NoSuchForward))
    );
    assert_eq!(ports(&d, None).await.unwrap().len(), 1);
    drop(a);
    d.stop().await;
}
