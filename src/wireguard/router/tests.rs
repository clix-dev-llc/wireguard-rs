use super::KeyPair;
use super::SIZE_MESSAGE_PREFIX;
use super::{Callbacks, Device};

use super::SIZE_KEEPALIVE;

use super::super::dummy;
use super::super::dummy_keypair;
use super::super::tests::make_packet;

use crate::platform::udp::Reader;

use std::net::IpAddr;
use std::ops::Deref;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use env_logger;
use num_cpus;
use test::Bencher;

extern crate test;

const SIZE_MSG: usize = 1024;

const TIMEOUT: Duration = Duration::from_millis(1000);

struct EventTracker<E> {
    rx: Mutex<Receiver<E>>,
    tx: Mutex<Sender<E>>,
}

impl<E> EventTracker<E> {
    fn new() -> Self {
        let (tx, rx) = channel();
        EventTracker {
            rx: Mutex::new(rx),
            tx: Mutex::new(tx),
        }
    }

    fn log(&self, e: E) {
        self.tx.lock().unwrap().send(e).unwrap();
    }

    fn wait(&self, timeout: Duration) -> Option<E> {
        match self.rx.lock().unwrap().recv_timeout(timeout) {
            Ok(v) => Some(v),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => panic!("Disconnect"),
        }
    }

    fn now(&self) -> Option<E> {
        self.wait(Duration::from_millis(0))
    }
}

// type for tracking events inside the router module
struct Inner {
    send: EventTracker<(usize, bool)>,
    recv: EventTracker<(usize, bool)>,
    need_key: EventTracker<()>,
    key_confirmed: EventTracker<()>,
}

#[derive(Clone)]
struct Opaque {
    inner: Arc<Inner>,
}

impl Deref for Opaque {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

struct TestCallbacks();

impl Opaque {
    fn new() -> Opaque {
        Opaque {
            inner: Arc::new(Inner {
                send: EventTracker::new(),
                recv: EventTracker::new(),
                need_key: EventTracker::new(),
                key_confirmed: EventTracker::new(),
            }),
        }
    }
}

macro_rules! no_events {
    ($opq:expr) => {
        assert_eq!($opq.send.now(), None, "unexpected send event");
        assert_eq!($opq.recv.now(), None, "unexpected recv event");
        assert_eq!($opq.need_key.now(), None, "unexpected need_key event");
        assert_eq!(
            $opq.key_confirmed.now(),
            None,
            "unexpected key_confirmed event"
        );
    };
}

impl Callbacks for TestCallbacks {
    type Opaque = Opaque;

    fn send(t: &Self::Opaque, size: usize, sent: bool, _keypair: &Arc<KeyPair>, _counter: u64) {
        t.send.log((size, sent))
    }

    fn recv(t: &Self::Opaque, size: usize, sent: bool, _keypair: &Arc<KeyPair>) {
        t.recv.log((size, sent))
    }

    fn need_key(t: &Self::Opaque) {
        t.need_key.log(());
    }

    fn key_confirmed(t: &Self::Opaque) {
        t.key_confirmed.log(());
    }
}

fn init() {
    let _ = env_logger::builder().is_test(true).try_init();
}

fn make_packet_padded(size: usize, src: IpAddr, dst: IpAddr, id: u64) -> Vec<u8> {
    let p = make_packet(size, src, dst, id);
    let mut o = vec![0; p.len() + SIZE_MESSAGE_PREFIX];
    o[SIZE_MESSAGE_PREFIX..SIZE_MESSAGE_PREFIX + p.len()].copy_from_slice(&p[..]);
    o
}

#[bench]
fn bench_outbound(b: &mut Bencher) {
    struct BencherCallbacks {}
    impl Callbacks for BencherCallbacks {
        type Opaque = Arc<AtomicUsize>;
        fn send(
            t: &Self::Opaque,
            size: usize,
            _sent: bool,
            _keypair: &Arc<KeyPair>,
            _counter: u64,
        ) {
            t.fetch_add(size, Ordering::SeqCst);
        }
        fn recv(_: &Self::Opaque, _size: usize, _sent: bool, _keypair: &Arc<KeyPair>) {}
        fn need_key(_: &Self::Opaque) {}
        fn key_confirmed(_: &Self::Opaque) {}
    }

    // create device
    let (_fake, _reader, tun_writer, _mtu) = dummy::TunTest::create(false);
    let router: Device<_, BencherCallbacks, dummy::TunWriter, dummy::VoidBind> =
        Device::new(num_cpus::get(), tun_writer);

    // add new peer
    let opaque = Arc::new(AtomicUsize::new(0));
    let peer = router.new_peer(opaque.clone());
    peer.add_keypair(dummy_keypair(true));

    // add subnet to peer
    let (mask, len, dst) = ("192.168.1.0", 24, "192.168.1.20");
    let mask: IpAddr = mask.parse().unwrap();
    peer.add_allowed_ip(mask, len);

    // create "IP packet"
    let dst = dst.parse().unwrap();
    let src = match dst {
        IpAddr::V4(_) => "127.0.0.1".parse().unwrap(),
        IpAddr::V6(_) => "::1".parse().unwrap(),
    };
    let msg = make_packet_padded(1024, src, dst, 0);

    // every iteration sends 10 GB
    b.iter(|| {
        opaque.store(0, Ordering::SeqCst);
        while opaque.load(Ordering::Acquire) < 10 * 1024 * 1024 {
            router.send(msg.to_vec()).unwrap();
        }
    });
}

#[test]
fn test_outbound() {
    init();

    // create device
    let (_fake, _reader, tun_writer, _mtu) = dummy::TunTest::create(false);
    let router: Device<_, TestCallbacks, _, _> = Device::new(1, tun_writer);
    router.set_outbound_writer(dummy::VoidBind::new());

    let tests = vec![
        ("192.168.1.0", 24, "192.168.1.20", true),
        ("172.133.133.133", 32, "172.133.133.133", true),
        ("172.133.133.133", 32, "172.133.133.132", false),
        (
            "2001:db8::ff00:42:0000",
            112,
            "2001:db8::ff00:42:3242",
            true,
        ),
        (
            "2001:db8::ff00:42:8000",
            113,
            "2001:db8::ff00:42:0660",
            false,
        ),
        (
            "2001:db8::ff00:42:8000",
            113,
            "2001:db8::ff00:42:ffff",
            true,
        ),
    ];

    for (mask, len, dst, okay) in tests.iter() {
        let len = *len;
        let okay = *okay;

        println!(
            "Check: {} {} {}/{}",
            dst,
            if okay { "\\in" } else { "\\notin" },
            mask,
            len
        );

        for set_key in vec![true, false] {
            for confirm_with_staged_packet in vec![true, false] {
                let send_keepalive = (!confirm_with_staged_packet || !okay) && set_key;
                let send_payload = okay && set_key;
                let need_key = ((confirm_with_staged_packet && set_key) || !set_key) && okay;

                println!(
                    "  confirm_with_staged_packet = {}, send_keepalive = {}, set_key = {}",
                    confirm_with_staged_packet, send_keepalive, set_key
                );

                // add new peer
                let opaque = Opaque::new();
                let peer = router.new_peer(opaque.clone());
                let mask: IpAddr = mask.parse().unwrap();

                // confirm using keepalive
                if set_key && (!confirm_with_staged_packet) {
                    peer.add_keypair(dummy_keypair(true));
                }

                // map subnet to peer
                peer.add_allowed_ip(mask, len);

                // create "IP packet"
                let dst = dst.parse().unwrap();
                let src = match dst {
                    IpAddr::V4(_) => "127.0.0.1".parse().unwrap(),
                    IpAddr::V6(_) => "::1".parse().unwrap(),
                };
                let msg = make_packet_padded(SIZE_MSG, src, dst, 0);

                // crypto-key route the IP packet
                let res = router.send(msg);
                assert_eq!(
                    res.is_ok(),
                    okay,
                    "crypto-routing / destination lookup failure"
                );

                // confirm using staged packet
                if set_key && confirm_with_staged_packet {
                    peer.add_keypair(dummy_keypair(true));
                }

                // check for key-material request
                if need_key {
                    assert_eq!(
                        opaque.need_key.wait(TIMEOUT),
                        Some(()),
                        "should have requested a new key, if no encryption state was set"
                    );
                }

                // check for keepalive
                if send_keepalive {
                    assert_eq!(
                        opaque.send.wait(TIMEOUT),
                        Some((SIZE_KEEPALIVE, false)),
                        "keepalive should be sent before transport message"
                    );
                }

                // check for encryption of payload
                if send_payload {
                    assert_eq!(
                        opaque.send.wait(TIMEOUT),
                        Some((SIZE_KEEPALIVE + SIZE_MSG, false)),
                        "message buffer should be encrypted"
                    )
                }

                // check that we handled all events
                no_events!(opaque);
            }
        }
    }
}

#[test]
fn test_bidirectional() {
    init();

    let tests = [
        (
            ("192.168.1.0", 24, "192.168.1.20", true),
            ("172.133.133.133", 32, "172.133.133.133", true),
        ),
        (
            ("192.168.1.0", 24, "192.168.1.20", true),
            ("172.133.133.133", 32, "172.133.133.133", true),
        ),
        (
            (
                "2001:db8::ff00:42:8000",
                113,
                "2001:db8::ff00:42:ffff",
                true,
            ),
            (
                "2001:db8::ff40:42:8000",
                113,
                "2001:db8::ff40:42:ffff",
                true,
            ),
        ),
        (
            (
                "2001:db8::ff00:42:8000",
                113,
                "2001:db8::ff00:42:ffff",
                true,
            ),
            (
                "2001:db8::ff40:42:8000",
                113,
                "2001:db8::ff40:42:ffff",
                true,
            ),
        ),
    ];

    for (p1, p2) in tests.iter() {
        for confirm_with_staged_packet in vec![true, false] {
            println!(
                "peer1 = {:?}, peer2 = {:?}, confirm_with_staged_packet = {}",
                p1, p2, confirm_with_staged_packet
            );

            let ((bind_reader1, bind_writer1), (bind_reader2, bind_writer2)) =
                dummy::PairBind::pair();

            let confirm_packet_size = if confirm_with_staged_packet {
                SIZE_KEEPALIVE + SIZE_MSG
            } else {
                SIZE_KEEPALIVE
            };

            // create matching device
            let (_fake, _, tun_writer1, _) = dummy::TunTest::create(false);
            let (_fake, _, tun_writer2, _) = dummy::TunTest::create(false);

            let router1: Device<_, TestCallbacks, _, _> = Device::new(1, tun_writer1);
            router1.set_outbound_writer(bind_writer1);

            let router2: Device<_, TestCallbacks, _, _> = Device::new(1, tun_writer2);
            router2.set_outbound_writer(bind_writer2);

            // prepare opaque values for tracing callbacks

            let opaque1 = Opaque::new();
            let opaque2 = Opaque::new();

            // create peers with matching keypairs and assign subnets

            let peer1 = router1.new_peer(opaque1.clone());
            let peer2 = router2.new_peer(opaque2.clone());

            {
                let (mask, len, _ip, _okay) = p1;
                let mask: IpAddr = mask.parse().unwrap();
                peer1.add_allowed_ip(mask, *len);
                peer1.add_keypair(dummy_keypair(false));
            }

            {
                let (mask, len, _ip, _okay) = p2;
                let mask: IpAddr = mask.parse().unwrap();
                peer2.add_allowed_ip(mask, *len);
                peer2.set_endpoint(dummy::UnitEndpoint::new());
            }

            if confirm_with_staged_packet {
                // create IP packet
                let (_mask, _len, ip1, _okay) = p1;
                let (_mask, _len, ip2, _okay) = p2;
                let msg = make_packet_padded(
                    SIZE_MSG,
                    ip1.parse().unwrap(), // src
                    ip2.parse().unwrap(), // dst
                    0,
                );

                // stage packet for sending
                router2.send(msg).expect("failed to sent staged packet");

                // a new key should have been requested from the handshake machine
                assert_eq!(
                    opaque2.need_key.wait(TIMEOUT),
                    Some(()),
                    "a new key should be requested since a packet was attempted transmitted"
                );

                no_events!(opaque1);
                no_events!(opaque2);
            }

            // add a keypair
            assert_eq!(peer1.get_endpoint(), None, "no endpoint has yet been set");
            peer2.add_keypair(dummy_keypair(true));

            // this should cause a key-confirmation packet (keepalive or staged packet)
            assert_eq!(
                opaque2.send.wait(TIMEOUT),
                Some((confirm_packet_size, true)),
                "expected successful transmission of a confirmation packet"
            );

            // no other events should fire
            no_events!(opaque1);
            no_events!(opaque2);

            // read confirming message received by the other end ("across the internet")
            let mut buf = vec![0u8; SIZE_MSG * 2];
            let (len, from) = bind_reader1.read(&mut buf).unwrap();
            buf.truncate(len);

            assert_eq!(
                len,
                if confirm_with_staged_packet {
                    SIZE_MSG + SIZE_KEEPALIVE
                } else {
                    SIZE_KEEPALIVE
                },
                "unexpected size of confirmation message"
            );

            // pass to the router for processing
            router1
                .recv(from, buf)
                .expect("failed to receive confirmation message");

            // check that a receive event is fired
            assert_eq!(
                opaque1.recv.wait(TIMEOUT),
                Some((confirm_packet_size, true)),
                "we expect processing to be successful"
            );

            // the key is confirmed
            assert_eq!(
                opaque1.key_confirmed.wait(TIMEOUT),
                Some(()),
                "confirmation message should confirm the key"
            );

            // peer1 learns the endpoint
            assert!(
                peer1.get_endpoint().is_some(),
                "peer1 should learn the endpoint of peer2 from the confirmation message (roaming)"
            );

            // no other events should fire
            no_events!(opaque1);
            no_events!(opaque2);
            // now that peer1 has an endpoint
            // route packets in the other direction: peer1 -> peer2
            for id in 1..11 {
                println!("packet: {}", id);

                let message_size = 1024;

                // pass IP packet to router
                let (_mask, _len, ip1, _okay) = p1;
                let (_mask, _len, ip2, _okay) = p2;
                let msg = make_packet_padded(
                    message_size,
                    ip2.parse().unwrap(), // src
                    ip1.parse().unwrap(), // dst
                    id,
                );

                router1
                    .send(msg)
                    .expect("we expect routing to be successful");

                // encryption succeeds and the correct size is logged
                assert_eq!(
                    opaque1.send.wait(TIMEOUT),
                    Some((message_size + SIZE_KEEPALIVE, true)),
                    "expected send event for peer1 -> peer2 payload"
                );

                // otherwise no events
                no_events!(opaque1);
                no_events!(opaque2);

                // receive ("across the internet") on the other end
                let mut buf = vec![0u8; 2048];
                let (len, from) = bind_reader2.read(&mut buf).unwrap();
                buf.truncate(len);
                router2.recv(from, buf).unwrap();

                // check that decryption succeeds
                assert_eq!(
                    opaque2.recv.wait(TIMEOUT),
                    Some((message_size + SIZE_KEEPALIVE, true)),
                    "decryption and routing should succeed"
                );

                // otherwise no events
                no_events!(opaque1);
                no_events!(opaque2);
            }
        }
    }
}
