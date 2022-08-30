use crate::MediaTime;

use super::*;

fn sr(ssrc: u32, ntp_time: MediaTime) -> RtcpFb {
    RtcpFb::SenderInfo(SenderInfo {
        ssrc: ssrc.into(),
        ntp_time,
        rtp_time: 4,
        sender_packet_count: 5,
        sender_octet_count: 6,
    })
}

fn rr(ssrc: u32) -> RtcpFb {
    RtcpFb::ReceiverReport(ReceiverReport {
        ssrc: ssrc.into(),
        fraction_lost: 3,
        packets_lost: 1234,
        max_seq: 4000,
        jitter: 5,
        last_sr_time: 12,
        last_sr_delay: 1,
    })
}

fn gb(ssrc: u32) -> RtcpFb {
    RtcpFb::Goodbye(ssrc.into())
}

#[test]
fn test_sr() {
    let mut buf = vec![0; 1200];

    let now = MediaTime::now();

    let mut fb = VecDeque::new();
    fb.push_back(sr(1, now));

    let n = RtcpFb::build_feedback(&mut fb, &mut buf);
    buf.truncate(n);
    assert_eq!(n, 28);

    let mut iter = RtcpFb::feedback(&buf);

    assert_eq!(iter.next(), Some(sr(1, now)));
}

#[test]
fn test_rr() {
    let mut buf = vec![0; 1200];

    let mut fb = VecDeque::new();
    fb.push_back(rr(2));

    let n = RtcpFb::build_feedback(&mut fb, &mut buf);
    buf.truncate(n);
    assert_eq!(n, 32);

    let mut iter = RtcpFb::feedback(&buf);

    assert_eq!(iter.next(), Some(rr(2)));
}

#[test]
fn test_sr_rr() {
    let mut buf = vec![0; 1200];

    let now = MediaTime::now();

    let mut fb = VecDeque::new();
    fb.push_back(rr(2));
    fb.push_back(sr(1, now));

    let n = RtcpFb::build_feedback(&mut fb, &mut buf);
    buf.truncate(n);
    assert_eq!(n, 52);

    let mut iter = RtcpFb::feedback(&buf);

    assert_eq!(iter.next(), Some(sr(1, now)));
    assert_eq!(iter.next(), Some(rr(2)));
}

#[test]
fn test_sr_rr_more_than_31() {
    let mut buf = vec![0; 1200];

    let now = MediaTime::now();

    let mut fb = VecDeque::new();
    for i in 0..33 {
        fb.push_back(rr(i + 2));
    }
    fb.push_back(sr(1, now));

    let n = RtcpFb::build_feedback(&mut fb, &mut buf);
    buf.truncate(n);
    assert_eq!(n, 828);

    let mut iter = RtcpFb::feedback(&buf);

    assert_eq!(iter.next(), Some(sr(1, now)));
    for i in 0..33 {
        fb.push_back(rr(i + 2));
    }
}

#[test]
fn test_gb() {
    let mut buf = vec![0; 1200];

    let mut fb = VecDeque::new();
    fb.push_back(gb(1));

    let n = RtcpFb::build_feedback(&mut fb, &mut buf);
    buf.truncate(n);
    assert_eq!(n, 8);

    println!("{:?}", buf);

    let mut iter = RtcpFb::feedback(&buf);

    assert_eq!(iter.next(), Some(gb(1)));
}

#[test]
fn test_gb_2() {
    let mut buf = vec![0; 1200];

    let mut fb = VecDeque::new();
    fb.push_back(gb(2));
    fb.push_back(gb(1));

    let n = RtcpFb::build_feedback(&mut fb, &mut buf);
    buf.truncate(n);
    assert_eq!(n, 12);

    println!("{:?}", buf);

    let mut iter = RtcpFb::feedback(&buf);

    assert_eq!(iter.next(), Some(gb(2)));
    assert_eq!(iter.next(), Some(gb(1)));
}

#[test]
fn test_gb_more_than_31() {
    let mut buf = vec![0; 1200];

    let mut fb = VecDeque::new();
    for i in 0..33 {
        fb.push_back(gb(1 + i));
    }

    let n = RtcpFb::build_feedback(&mut fb, &mut buf);
    buf.truncate(n);
    assert_eq!(n, 140);

    let mut iter = RtcpFb::feedback(&buf);

    for i in 0..33 {
        assert_eq!(iter.next(), Some(gb(i + 1)));
    }
}