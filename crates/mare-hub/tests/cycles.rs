//! The hub's cycle, driven by hand on a fake clock.
//!
//! Every test here runs the hub's cycle thread on the test's own thread, and
//! runs each client by hand between the two halves of a cycle. A client that
//! has not run when the hub waits is late: the fake clock jumps straight to
//! the deadline. So which clients are late is decided by the test, exactly.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{DEVICE, connect, fake_hub, unique_hub};
use mare_client::{Client, ClientConfig, ConnectError};
use mare_hub::fake::silence;
use mare_hub::{Clock, HubError};
use mare_protocol::registry::{
    RegistryHeader, RegistryLayout, RegistryView, reject_reason, slot_state,
};
use mare_protocol::ring::UmpEvent;
use mare_protocol::segment::{SegmentConfig, SegmentLayout, SegmentView};
use mare_sys::{SharedMemory, current_pid};

/// Channel 0 counts frames; channel 1 is minus that.
fn ramp(channel: u32, frame: u64) -> f32 {
    let x = (frame % 10_000) as f32;
    if channel == 0 { x } else { -x }
}

/// Writes `value` to every sample of every output.
fn constant(client: &mut Client, value: f32) -> bool {
    client
        .try_cycle(|cycle| {
            for channel in 0..cycle.output_channels() {
                cycle.output(channel).unwrap().fill(value);
            }
        })
        .unwrap()
}

#[test]
fn a_client_hears_the_device_and_the_device_hears_the_client() {
    let hub = unique_hub();
    let (mut engine, mut keeper, _clock) = fake_hub(&hub, 4, ramp);
    let mut client = connect(&mut keeper, &hub, &ClientConfig::new("double", 2, 2)).unwrap();
    assert_eq!(client.info().sample_rate, 48_000);
    assert_eq!(client.info().period_frames, 64);

    for expected_cycle in 1..=5u64 {
        engine.begin_cycle().unwrap();
        let ran = client
            .try_cycle(|cycle| {
                assert_eq!(cycle.number(), expected_cycle);
                assert_eq!(cycle.frames(), 64);
                assert_eq!(cycle.frame_time(), (expected_cycle - 1) * 64);
                for channel in 0..2 {
                    let (input, output) = cycle.input_output(channel, channel).unwrap();
                    for (out, x) in output.iter_mut().zip(input) {
                        *out = 2.0 * x;
                    }
                }
            })
            .unwrap();
        assert!(ran);
        let report = engine.end_cycle().unwrap();
        assert_eq!((report.woken, report.on_time, report.late), (1, 1, 0));
        let start = engine.device().last_period_start();
        let left = engine.device().last_output(0);
        let right = engine.device().last_output(1);
        for i in 0..64 {
            let x = ramp(0, start + i as u64);
            assert_eq!(left[i], 2.0 * x);
            assert_eq!(right[i], -2.0 * x);
        }
    }
    assert_eq!(client.last_cycle(), 5);
    assert_eq!(client.late_cycles(), 0);
    assert_eq!(engine.stats().client_cycles, 5);
}

#[test]
fn many_clients_are_summed_and_a_late_one_loses_only_its_own_output() {
    let hub = unique_hub();
    let (mut engine, mut keeper, _clock) = fake_hub(&hub, 64, silence);
    let mut clients: Vec<Client> = (0..40)
        .map(|i| {
            connect(
                &mut keeper,
                &hub,
                &ClientConfig::new(&format!("c{i}"), 0, 2),
            )
            .unwrap()
        })
        .collect();
    let value = |i: usize| (i + 1) as f32;
    let all: f32 = (0..40).map(value).sum();

    // Every client in time.
    engine.begin_cycle().unwrap();
    for (i, client) in clients.iter_mut().enumerate() {
        assert!(constant(client, value(i)));
    }
    let report = engine.end_cycle().unwrap();
    assert_eq!(report.on_time, 40);
    assert!(engine.device().last_output(0).iter().all(|&x| x == all));
    assert!(engine.device().last_output(1).iter().all(|&x| x == all));

    // Client 7 does not run this cycle: only its share is missing.
    engine.begin_cycle().unwrap();
    for (i, client) in clients.iter_mut().enumerate() {
        if i != 7 {
            assert!(constant(client, value(i)));
        }
    }
    let report = engine.end_cycle().unwrap();
    assert_eq!((report.on_time, report.late), (39, 1));
    assert_eq!(
        report.waited,
        engine.budget(),
        "the hub waited until the deadline, no more"
    );
    assert!(
        engine
            .device()
            .last_output(0)
            .iter()
            .all(|&x| x == all - value(7))
    );

    // It finishes that cycle after the deadline. Its late output is never
    // mixed, and from the next cycle it is woken again.
    assert!(constant(&mut clients[7], 1000.0));
    engine.begin_cycle().unwrap();
    for (i, client) in clients.iter_mut().enumerate() {
        assert!(constant(client, value(i)));
    }
    let report = engine.end_cycle().unwrap();
    assert_eq!((report.on_time, report.late, report.busy), (40, 0, 0));
    assert!(engine.device().last_output(0).iter().all(|&x| x == all));

    assert_eq!(clients[7].late_cycles(), 1);
    assert_eq!(clients[8].late_cycles(), 0);
    let stats = engine.stats();
    assert_eq!(stats.late, 1);
    assert_eq!(stats.client_cycles, 40 + 39 + 40);
}

#[test]
fn a_client_still_on_an_earlier_cycle_is_not_woken_or_touched() {
    let hub = unique_hub();
    let (mut engine, mut keeper, _clock) = fake_hub(&hub, 4, ramp);
    let mut slow = connect(&mut keeper, &hub, &ClientConfig::new("slow", 2, 2)).unwrap();
    let mut steady = connect(&mut keeper, &hub, &ClientConfig::new("steady", 0, 2)).unwrap();

    engine.begin_cycle().unwrap(); // cycle 1: the slow client misses it
    assert!(constant(&mut steady, 1.0));
    assert_eq!(engine.end_cycle().unwrap().late, 1);

    // Cycle 2: the slow client has not finished cycle 1, so it is skipped,
    // and its input buffers keep cycle 1's samples.
    engine.begin_cycle().unwrap();
    assert!(constant(&mut steady, 1.0));
    let report = engine.end_cycle().unwrap();
    assert_eq!((report.woken, report.busy, report.on_time), (1, 1, 1));
    assert!(engine.device().last_output(0).iter().all(|&x| x == 1.0));

    // Now it runs, and what it sees is cycle 1, intact.
    let ran = slow
        .try_cycle(|cycle| {
            assert_eq!(cycle.number(), 1);
            assert_eq!(cycle.frame_time(), 0);
            let input = cycle.input(0).unwrap();
            assert!(input.iter().enumerate().all(|(i, &x)| x == i as f32));
        })
        .unwrap();
    assert!(ran);
    // It has nothing more to do until woken.
    assert!(!slow.try_cycle(|_| unreachable!()).unwrap());

    engine.begin_cycle().unwrap(); // cycle 3: both again
    assert!(constant(&mut slow, 10.0));
    assert!(constant(&mut steady, 1.0));
    let report = engine.end_cycle().unwrap();
    assert_eq!((report.woken, report.on_time), (2, 2));
    assert!(engine.device().last_output(1).iter().all(|&x| x == 11.0));
    assert_eq!(slow.late_cycles(), 2, "one late, one skipped");
    let stats = engine.stats();
    assert_eq!((stats.late, stats.busy), (1, 1));
}

#[test]
fn a_client_that_never_answers_never_holds_the_hub() {
    let hub = unique_hub();
    let (mut engine, mut keeper, clock) = fake_hub(&hub, 4, silence);
    let _hung = connect(&mut keeper, &hub, &ClientConfig::new("hung", 2, 2)).unwrap();
    let mut fine = connect(&mut keeper, &hub, &ClientConfig::new("fine", 2, 2)).unwrap();
    let period = Duration::from_nanos(64 * 1_000_000_000 / 48_000);
    for n in 0..100u32 {
        engine.begin_cycle().unwrap();
        let start = clock.now();
        assert!(constant(&mut fine, 0.5));
        let report = engine.end_cycle().unwrap();
        // The clock is fake, so this is exact: the hub waits for the budget
        // and not a nanosecond more, every cycle.
        assert!(clock.now() - start <= engine.budget());
        assert!(engine.budget() < period);
        assert_eq!(report.on_time, 1);
        assert_eq!(report.woken + report.busy, 2);
        assert!(
            engine.device().last_output(0).iter().all(|&x| x == 0.5),
            "cycle {n}"
        );
    }
    let stats = engine.stats();
    assert_eq!(stats.late + stats.busy, 100);
    assert_eq!(stats.late, 1, "late once, then busy until it answers");
    assert_eq!(stats.longest_wait_ns, engine.budget().as_nanos() as u64);
}

#[test]
fn a_client_is_not_woken_or_counted_until_it_is_ready() {
    let hub = unique_hub();
    let (mut engine, mut keeper, _clock) = fake_hub(&hub, 4, silence);
    let mut client =
        common::connect_only(&mut keeper, &hub, &ClientConfig::new("slow start", 0, 2)).unwrap();
    // Adopted, but its audio thread has not started: three cycles pass it by.
    for _ in 0..3 {
        let report = engine.run_cycle().unwrap();
        assert_eq!((report.woken, report.late, report.busy), (0, 0, 0));
    }
    assert_eq!(client.late_cycles(), 0);
    // Its first wait makes it ready; the next cycle wakes it.
    assert!(!client.try_cycle(|_| unreachable!()).unwrap());
    engine.begin_cycle().unwrap();
    assert!(constant(&mut client, 0.5));
    let report = engine.end_cycle().unwrap();
    assert_eq!((report.woken, report.on_time), (1, 1));
    assert_eq!(report.cycle, 4);
    assert!(engine.device().last_output(0).iter().all(|&x| x == 0.5));
    let stats = engine.stats();
    assert_eq!((stats.late, stats.busy), (0, 0));
}

#[test]
fn a_client_that_leaves_is_freed_and_its_slot_reused() {
    let hub = unique_hub();
    let (mut engine, mut keeper, _clock) = fake_hub(&hub, 1, silence);
    let mut first = connect(&mut keeper, &hub, &ClientConfig::new("first", 0, 2)).unwrap();
    assert_eq!(first.slot(), 0);
    engine.begin_cycle().unwrap();
    assert!(constant(&mut first, 1.0));
    engine.end_cycle().unwrap();

    // The only slot is taken.
    assert!(matches!(
        connect(&mut keeper, &hub, &ClientConfig::new("second", 0, 2)),
        Err(ConnectError::Full)
    ));

    first.leave();
    keeper.poll(); // asks the cycle thread to let go
    assert_eq!(engine.clients().count(), 0);
    // Until the cycle thread has let go, the slot is not handed out again.
    assert!(matches!(
        connect(&mut keeper, &hub, &ClientConfig::new("second", 0, 2)),
        Err(ConnectError::Full)
    ));
    engine.run_cycle().unwrap(); // lets go
    keeper.poll(); // frees
    assert_eq!(keeper.stats().left, 1);

    let mut second = connect(&mut keeper, &hub, &ClientConfig::new("second", 0, 2)).unwrap();
    assert_eq!(second.slot(), 0);
    let ids: Vec<_> = engine.clients().collect();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].generation, 1);
    engine.begin_cycle().unwrap();
    assert!(constant(&mut second, 2.0));
    assert_eq!(engine.end_cycle().unwrap().on_time, 1);
    assert!(engine.device().last_output(0).iter().all(|&x| x == 2.0));
}

#[test]
fn events_go_both_ways_on_the_same_clock() {
    let hub = unique_hub();
    let (mut engine, mut keeper, _clock) = fake_hub(&hub, 4, silence);
    let mut client = connect(&mut keeper, &hub, &ClientConfig::new("notes", 0, 2)).unwrap();
    let id = engine.clients().next().unwrap();
    let note_on = |time| UmpEvent {
        time,
        words: [0x4090_3C00, 0xC000_0000, 0, 0],
    };
    engine.send_event(id, note_on(70)).unwrap();
    engine.send_event(id, note_on(100)).unwrap();

    engine.begin_cycle().unwrap(); // frames 0..64
    client
        .try_cycle(|cycle| {
            let first = cycle.next_event().unwrap();
            let second = cycle.next_event().unwrap();
            assert_eq!((first.time, second.time), (70, 100));
            assert!(cycle.next_event().is_none());
            for n in 0..3 {
                let time = cycle.frame_time() + n;
                cycle.send_event(note_on(time)).unwrap();
            }
        })
        .unwrap();
    engine.end_cycle().unwrap();
    assert_eq!(engine.stats().events_received, 3);

    // A full ring hands the event back instead of losing it silently.
    let mut refused = 0;
    for n in 0..300 {
        if engine.send_event(id, note_on(n)).is_err() {
            refused += 1;
        }
    }
    assert_eq!(refused, 300 - 256);
}

#[test]
fn a_second_hub_of_the_same_name_is_refused_and_a_missing_one_is_reported() {
    let hub = unique_hub();
    let (_engine, _keeper, _clock) = fake_hub(&hub, 4, silence);
    let clock: std::sync::Arc<dyn Clock> = std::sync::Arc::new(mare_hub::SystemClock::new());
    let device = mare_hub::fake::FakeDevice::new(DEVICE, mare_hub::fake::Pacing::RealTime);
    assert!(matches!(
        mare_hub::create(&mare_hub::HubConfig::new(hub.clone()), device, clock),
        Err(HubError::Exists)
    ));
    assert!(matches!(
        Client::connect(&unique_hub(), &ClientConfig::new("x", 0, 2)),
        Err(ConnectError::NoHub(_))
    ));
}

#[test]
fn the_hub_refuses_segments_it_cannot_trust() {
    let hub = unique_hub();
    let (_engine, mut keeper, _clock) = fake_hub(&hub, 4, silence);
    let registry = SharedMemory::open(&hub.registry()).unwrap();
    let len = registry.len() as u64;
    // SAFETY: the mapping is kept to the end of the test.
    let header = unsafe { RegistryHeader::read_from(registry.base(), len) }.unwrap();
    let layout = RegistryLayout::validate(&header, len).unwrap();
    // SAFETY: as above.
    let view = unsafe { RegistryView::from_raw(registry.base(), len, layout) }.unwrap();

    // Plays the part of a client by hand, with a segment spoiled by `spoil`,
    // and returns the hub's answer.
    let attempt = |keeper: &mut mare_hub::Housekeeper,
                   spoil: &dyn Fn(&SegmentView, &mut mare_protocol::segment::SegmentHeader),
                   create: bool| {
        let slot = view.slot(0).unwrap();
        slot.state
            .compare_exchange(
                slot_state::FREE,
                slot_state::CLAIMED,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .unwrap();
        slot.client_pid.store(current_pid(), Ordering::Release);
        let generation = slot.generation.load(Ordering::Acquire);
        let seg_layout = SegmentLayout::new(SegmentConfig {
            input_channels: 1,
            output_channels: 1,
            max_frames: DEVICE.max_frames,
            events_in_capacity: 0,
            events_out_capacity: 0,
        })
        .unwrap();
        let memory = create.then(|| {
            let memory =
                SharedMemory::create(&hub.segment(0, generation), seg_layout.size() as usize)
                    .unwrap();
            // SAFETY: the memory is kept until the hub has answered.
            let segment =
                unsafe { SegmentView::from_raw(memory.base(), memory.len() as u64, seg_layout) }
                    .unwrap();
            let mut header = seg_layout.header("mallory", 0).unwrap();
            spoil(&segment, &mut header);
            // SAFETY: nobody reads it yet.
            unsafe { segment.write_header(&header) };
            memory
        });
        slot.state.store(slot_state::REQUESTED, Ordering::Release);
        keeper.poll();
        let state = slot.state.load(Ordering::Acquire);
        let reason = slot.reject_reason.load(Ordering::Acquire);
        // Acknowledge, as a client does, so the slot is freed.
        slot.state.store(slot_state::LEAVING, Ordering::Release);
        keeper.poll();
        assert_eq!(slot.state.load(Ordering::Acquire), slot_state::FREE);
        drop(memory);
        (state, reason)
    };

    let rejected = |reason| (slot_state::REJECTED, reason);
    assert_eq!(
        attempt(&mut keeper, &|_, _| {}, false),
        rejected(reject_reason::NO_SEGMENT)
    );
    assert_eq!(
        attempt(&mut keeper, &|_, h| h.magic = *b"NOTMARE\0", true),
        rejected(reject_reason::BAD_SEGMENT)
    );
    assert_eq!(
        attempt(&mut keeper, &|_, h| h.audio_offset = 0, true),
        rejected(reject_reason::BAD_SEGMENT)
    );
    assert_eq!(
        attempt(&mut keeper, &|_, h| h.output_channels = 4000, true),
        rejected(reject_reason::BAD_SEGMENT)
    );
    // A header that claims more memory than is mapped.
    assert_eq!(
        attempt(
            &mut keeper,
            &|_, h| {
                h.max_frames = 8192;
                h.channel_stride = 8192 * 4;
                h.segment_size = 384 + 2 * 8192 * 4;
            },
            true
        ),
        rejected(reject_reason::BAD_SEGMENT)
    );
    assert_eq!(keeper.stats().rejected, 5);
    assert_eq!(keeper.stats().accepted, 0);
}

#[test]
fn a_slot_claimed_by_a_client_that_died_at_once_is_freed() {
    let hub = unique_hub();
    let (_engine, mut keeper, _clock) = fake_hub(&hub, 1, silence);
    let registry = SharedMemory::open(&hub.registry()).unwrap();
    let len = registry.len() as u64;
    // SAFETY: the mapping is kept to the end of the test.
    let header = unsafe { RegistryHeader::read_from(registry.base(), len) }.unwrap();
    let layout = RegistryLayout::validate(&header, len).unwrap();
    // SAFETY: as above.
    let view = unsafe { RegistryView::from_raw(registry.base(), len, layout) }.unwrap();
    let slot = view.slot(0).unwrap();

    // A client claims the only slot and dies before storing its process id.
    slot.state
        .compare_exchange(
            slot_state::FREE,
            slot_state::CLAIMED,
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .unwrap();
    keeper.poll();
    assert_eq!(slot.state.load(Ordering::Acquire), slot_state::CLAIMED);
    assert!(matches!(
        connect(&mut keeper, &hub, &ClientConfig::new("later", 0, 2)),
        Err(ConnectError::Full)
    ));

    // After the grace, the slot is free again, under a new generation.
    std::thread::sleep(mare_hub::CLAIM_GRACE + Duration::from_millis(50));
    keeper.poll();
    assert_eq!(slot.state.load(Ordering::Acquire), slot_state::FREE);
    assert_eq!(slot.generation.load(Ordering::Acquire), 1);
    let client = connect(&mut keeper, &hub, &ClientConfig::new("later", 0, 2)).unwrap();
    assert_eq!(client.slot(), 0);
}

#[test]
fn a_hub_can_be_probed_without_taking_a_slot() {
    let name = unique_hub();
    assert!(matches!(
        mare_client::probe(&name),
        Err(ConnectError::NoHub(_))
    ));
    let (engine, mut keeper, _clock) = fake_hub(&name, 1, silence);
    let info = mare_client::probe(&name).unwrap();
    assert_eq!(
        (info.sample_rate, info.period_frames, info.max_frames),
        (DEVICE.sample_rate, DEVICE.period_frames, DEVICE.max_frames)
    );
    assert_eq!(
        (info.device_inputs, info.device_outputs),
        (DEVICE.input_channels, DEVICE.output_channels)
    );
    assert_eq!(info.hub_pid, current_pid());
    // The only slot is still free: a client takes it.
    let client = connect(&mut keeper, &name, &ClientConfig::new("after", 1, 1)).unwrap();
    assert_eq!(client.info(), info);
    drop((client, engine));
}
