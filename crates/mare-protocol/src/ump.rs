//! What the protocol needs to know about Universal MIDI Packets.
//!
//! MARÉ carries MIDI as MIDI 2.0 Universal MIDI Packets. The hub does not
//! interpret them beyond their length, which the message type in the top
//! four bits of the first word decides, as the UMP specification's table of
//! message types says.

/// The message type of a packet: the top four bits of its first word.
pub const fn message_type(word0: u32) -> u8 {
    (word0 >> 28) as u8
}

/// How many 32-bit words the packet starting with `word0` has: 1, 2, 3 or 4.
///
/// Reserved message types have the length the specification assigns them,
/// so a reader can skip a packet it does not understand.
pub const fn packet_words(word0: u32) -> usize {
    match message_type(word0) {
        0x0 | 0x1 | 0x2 | 0x6 | 0x7 => 1,
        0x3 | 0x4 | 0x8 | 0x9 | 0xA => 2,
        0xB | 0xC => 3,
        _ => 4,
    }
}
